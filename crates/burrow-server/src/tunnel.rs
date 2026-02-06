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

use burrow_core::{ClientMessage, ServerMessage};

/// A registered tunnel: holds the WebSocket sender and pending visitor requests.
pub struct Tunnel {
    pub subdomain: String,
    ws_tx: Mutex<SplitSink<WebSocketStream<TokioIo<Upgraded>>, Message>>,
    /// Pending visitor connections waiting for the client to accept.
    pending: RwLock<HashMap<Uuid, oneshot::Sender<Vec<u8>>>>,
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

    /// Notify the tunnel client that a new visitor has arrived.
    /// Returns a receiver that will eventually contain the response bytes.
    pub async fn send_new_connection(&self) -> anyhow::Result<(Uuid, oneshot::Receiver<Vec<u8>>)> {
        let stream_id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();

        self.pending.write().await.insert(stream_id, tx);

        let msg = ServerMessage::NewConnection { stream_id };
        let text = serde_json::to_string(&msg)?;
        self.ws_tx.lock().await.send(Message::Text(text.into())).await?;

        Ok((stream_id, rx))
    }

    /// Send a raw message to the tunnel client.
    pub async fn send_message(&self, msg: &ServerMessage) -> anyhow::Result<()> {
        let text = serde_json::to_string(msg)?;
        self.ws_tx.lock().await.send(Message::Text(text.into())).await?;
        Ok(())
    }

    /// Resolve a pending visitor connection with response data.
    pub async fn resolve_connection(&self, stream_id: Uuid, data: Vec<u8>) {
        if let Some(tx) = self.pending.write().await.remove(&stream_id) {
            let _ = tx.send(data);
        }
    }
}

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
            if !self.tunnels.read().await.contains_key(&normalized) {
                return normalized;
            }
            warn!(requested = %normalized, "requested subdomain already taken, generating random one");
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
/// Listens for Accept messages and heartbeats.
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
                // TODO: parse stream_id header from data and resolve the pending connection.
                // For now, this is a placeholder for the yamux/binary data path.
                let _ = data;
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
