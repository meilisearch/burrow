use std::collections::HashMap;
use std::sync::Arc;

use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use tokio::sync::{Mutex, RwLock, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};
use uuid::Uuid;

use burrow_core::{ClientMessage, ServerMessage, TunnelFrame, TunnelRequest, TunnelResponse, decode_frame, encode};

/// A registered tunnel: holds the WebSocket sender and pending visitor requests.
pub struct Tunnel {
    pub subdomain: String,
    ws_tx: Mutex<SplitSink<WebSocketStream<TokioIo<Upgraded>>, Message>>,
    /// Pending visitor connections waiting for the client's response.
    pending: RwLock<HashMap<Uuid, oneshot::Sender<TunnelResponse>>>,
}

impl Tunnel {
    pub fn new(
        subdomain: String,
        ws_tx: SplitSink<WebSocketStream<TokioIo<Upgraded>>, Message>,
    ) -> Self {
        Self {
            subdomain,
            ws_tx: Mutex::new(ws_tx),
            pending: RwLock::new(HashMap::new()),
        }
    }

    /// Notify the tunnel client about a new visitor connection and send the
    /// request as a binary frame atomically (holding the ws_tx lock across both).
    /// Returns a receiver that will contain the response from the client.
    pub async fn send_new_connection_with_request(
        &self,
        request: &TunnelRequest,
    ) -> anyhow::Result<(Uuid, oneshot::Receiver<TunnelResponse>)> {
        let stream_id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();

        self.pending.write().await.insert(stream_id, tx);

        // Hold the lock across both sends for atomic delivery
        let mut ws = self.ws_tx.lock().await;

        // 1. Send the JSON NewConnection control message
        let msg = ServerMessage::NewConnection { stream_id };
        let text = serde_json::to_string(&msg)?;
        ws.send(Message::Text(text.into())).await?;

        // 2. Send the binary request frame immediately after
        let frame = TunnelFrame::Request(request.clone());
        let binary = encode(stream_id, &frame);
        ws.send(Message::Binary(binary.into())).await?;

        Ok((stream_id, rx))
    }

    /// Cancel a pending visitor connection (e.g. on timeout).
    pub async fn cancel_pending(&self, stream_id: &Uuid) {
        self.pending.write().await.remove(stream_id);
    }

    /// Send a raw message to the tunnel client.
    pub async fn send_message(&self, msg: &ServerMessage) -> anyhow::Result<()> {
        let text = serde_json::to_string(msg)?;
        self.ws_tx.lock().await.send(Message::Text(text.into())).await?;
        Ok(())
    }

    /// Resolve a pending visitor connection with a tunnel response.
    pub async fn resolve_connection(&self, stream_id: Uuid, response: TunnelResponse) {
        if let Some(tx) = self.pending.write().await.remove(&stream_id) {
            let _ = tx.send(response);
        } else {
            warn!(%stream_id, "no pending connection for response");
        }
    }
}

/// Subdomains that cannot be claimed by tunnel clients.
const RESERVED_SUBDOMAINS: &[&str] = &["new", "www", "api", "admin", "app"];

/// Global registry of active tunnels, keyed by subdomain.
pub struct TunnelRegistry {
    tunnels: RwLock<HashMap<String, Arc<Tunnel>>>,
    allowed_tokens: Option<Vec<String>>,
    domain: String,
}

impl TunnelRegistry {
    pub fn new(domain: String, allowed_tokens: Option<Vec<String>>) -> Self {
        Self {
            tunnels: RwLock::new(HashMap::new()),
            allowed_tokens,
            domain,
        }
    }

    /// Validate an auth token (if authentication is enabled).
    pub fn validate_token(&self, token: &Option<String>) -> bool {
        match &self.allowed_tokens {
            None => true,
            Some(allowed) => match token {
                Some(t) => allowed.contains(t),
                None => false,
            },
        }
    }

    /// Register a new tunnel and return the assigned subdomain + public URL.
    pub async fn register(
        &self,
        ws_tx: SplitSink<WebSocketStream<TokioIo<Upgraded>>, Message>,
        requested_subdomain: Option<String>,
    ) -> anyhow::Result<(String, String, Arc<Tunnel>)> {
        let subdomain = self.assign_subdomain(requested_subdomain).await;
        let url = format!("https://{}.{}", subdomain, self.domain);

        let tunnel = Arc::new(Tunnel::new(subdomain.clone(), ws_tx));
        self.tunnels.write().await.insert(subdomain.clone(), tunnel.clone());

        info!(subdomain = %subdomain, url = %url, "tunnel registered");
        Ok((subdomain, url, tunnel))
    }

    /// Remove a tunnel from the registry.
    pub async fn unregister(&self, subdomain: &str) {
        self.tunnels.write().await.remove(subdomain);
        info!(subdomain = %subdomain, "tunnel unregistered");
    }

    /// Look up a tunnel by subdomain.
    pub async fn get(&self, subdomain: &str) -> Option<Arc<Tunnel>> {
        self.tunnels.read().await.get(subdomain).cloned()
    }

    /// Assign a subdomain: use the requested one if available, otherwise generate a random one.
    async fn assign_subdomain(&self, requested: Option<String>) -> String {
        if let Some(req) = requested {
            let normalized = req.to_lowercase();
            if RESERVED_SUBDOMAINS.contains(&normalized.as_str()) {
                warn!(requested = %normalized, "requested subdomain is reserved, generating random one");
            } else if !self.tunnels.read().await.contains_key(&normalized) {
                return normalized;
            } else {
                warn!(requested = %normalized, "requested subdomain already taken, generating random one");
            }
        }
        self.generate_random_subdomain().await
    }

    async fn generate_random_subdomain(&self) -> String {
        use rand::Rng;
        loop {
            let subdomain: String = {
                let mut rng = rand::thread_rng();
                (0..8)
                    .map(|_| {
                        let idx = rng.gen_range(0..36u8);
                        if idx < 10 {
                            (b'0' + idx) as char
                        } else {
                            (b'a' + idx - 10) as char
                        }
                    })
                    .collect()
            };

            if !self.tunnels.read().await.contains_key(&subdomain) {
                return subdomain;
            }
        }
    }
}

/// Handle a tunnel client's WebSocket connection after the handshake.
/// Listens for Accept messages, heartbeats, and binary response frames.
pub async fn handle_client_messages(
    tunnel: Arc<Tunnel>,
    mut ws_rx: futures_util::stream::SplitStream<WebSocketStream<TokioIo<Upgraded>>>,
    registry: Arc<TunnelRegistry>,
) {
    while let Some(msg) = ws_rx.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "WebSocket read error");
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                let Ok(client_msg) = serde_json::from_str::<ClientMessage>(&text) else {
                    warn!("failed to parse client message");
                    continue;
                };
                match client_msg {
                    ClientMessage::Heartbeat => {
                        let _ = tunnel.send_message(&ServerMessage::Heartbeat).await;
                    }
                    ClientMessage::Accept { stream_id } => {
                        info!(%stream_id, "client accepted connection");
                    }
                    ClientMessage::Hello { .. } => {
                        warn!("unexpected Hello message after handshake");
                    }
                }
            }
            Message::Binary(data) => {
                match decode_frame(&data) {
                    Ok((stream_id, TunnelFrame::Response(resp))) => {
                        tunnel.resolve_connection(stream_id, resp).await;
                    }
                    Ok((stream_id, TunnelFrame::Error(err))) => {
                        warn!(%stream_id, message = %err.message, "client reported error");
                        // Synthesize a 502 response for the visitor
                        let resp = TunnelResponse {
                            status: 502,
                            headers: vec![("content-type".into(), b"text/plain".to_vec())],
                            body: format!("upstream error: {}", err.message).into_bytes(),
                        };
                        tunnel.resolve_connection(stream_id, resp).await;
                    }
                    Ok((stream_id, TunnelFrame::Request(_))) => {
                        warn!(%stream_id, "unexpected request frame from client");
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to decode binary frame");
                    }
                }
            }
            Message::Ping(data) => {
                let _ = tunnel
                    .send_message(&ServerMessage::Heartbeat)
                    .await;
                let _ = data;
            }
            Message::Close(_) => {
                info!(subdomain = %tunnel.subdomain, "client disconnected");
                break;
            }
            _ => {}
        }
    }

    registry.unregister(&tunnel.subdomain).await;
}
