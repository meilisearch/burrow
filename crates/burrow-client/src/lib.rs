//! Embeddable tunnel client library for [burrow](https://github.com/meilisearch/burrow).
//!
//! Expose a local HTTP server to the internet through a public subdomain.
//! Automatically reconnects with exponential backoff if the connection drops.
//!
//! # Example
//!
//! ```no_run
//! use burrow_client::TunnelClient;
//! use std::net::SocketAddr;
//!
//! # #[tokio::main]
//! # async fn main() -> anyhow::Result<()> {
//! let local_addr: SocketAddr = "127.0.0.1:3000".parse()?;
//!
//! let handle = TunnelClient::new("wss://new.meilisearch.link", local_addr)
//!     .subdomain("myapp")
//!     .connect()
//!     .await?;
//!
//! println!("Tunnel live at: {}", handle.url());
//! tokio::signal::ctrl_c().await?;
//! handle.close().await;
//! # Ok(())
//! # }
//! ```

pub use burrow_core;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::client::conn::http1;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tracing::{error, info, warn};

const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
use burrow_core::{
    ClientMessage, ServerMessage, TUNNEL_WS_PATH, TunnelError, TunnelFrame, TunnelRequest,
    TunnelResponse, decode_frame, encode,
};

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

/// A handle to a running tunnel. Dropping it will close the tunnel.
pub struct TunnelHandle {
    url: String,
    _task: JoinHandle<()>,
    shutdown_tx: watch::Sender<bool>,
}

impl TunnelHandle {
    /// The publicly accessible URL assigned by the tunnel server.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Gracefully shut down the tunnel.
    pub async fn close(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self._task.await;
    }
}

/// Builder for configuring and connecting a tunnel client.
pub struct TunnelClient {
    server_url: String,
    local_addr: SocketAddr,
    token: Option<String>,
    subdomain: Option<String>,
}

impl TunnelClient {
    pub fn new(server_url: impl Into<String>, local_addr: SocketAddr) -> Self {
        Self {
            server_url: server_url.into(),
            local_addr,
            token: None,
            subdomain: None,
        }
    }

    /// Set an authentication token.
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Request a specific subdomain (e.g. "myapp" → myapp.yourdomain.com).
    pub fn subdomain(mut self, subdomain: impl Into<String>) -> Self {
        self.subdomain = Some(subdomain.into());
        self
    }

    /// Connect to the tunnel server and start forwarding traffic.
    /// Returns a [`TunnelHandle`] with the assigned public URL.
    ///
    /// The client automatically reconnects with exponential backoff if the
    /// connection is lost (e.g. server restart). Reconnection stops when
    /// [`TunnelHandle::close`] is called.
    pub async fn connect(self) -> Result<TunnelHandle> {
        let ws_url = format!(
            "{}/{}",
            self.server_url.trim_end_matches('/'),
            TUNNEL_WS_PATH.trim_start_matches('/')
        );

        info!(url = %ws_url, "connecting to tunnel server");

        let (ws, _response) = connect_async(&ws_url)
            .await
            .context("failed to connect to tunnel server")?;

        let (assigned_url, ws) = handshake(ws, self.token.clone(), self.subdomain.clone()).await?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let local_addr = self.local_addr;
        let url = assigned_url.clone();
        let token = self.token;
        let subdomain = self.subdomain;

        let task = tokio::spawn(async move {
            run_with_reconnect(ws, ws_url, token, subdomain, local_addr, shutdown_rx).await;
        });

        Ok(TunnelHandle {
            url,
            _task: task,
            shutdown_tx,
        })
    }
}

/// Run the tunnel loop, reconnecting with exponential backoff on disconnect.
async fn run_with_reconnect(
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    ws_url: String,
    token: Option<String>,
    subdomain: Option<String>,
    local_addr: SocketAddr,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    // Run the first connection (already established by connect()).
    if let Err(e) = tunnel_loop(ws, local_addr, shutdown_rx.clone()).await {
        error!(error = %e, "tunnel loop exited with error");
    }

    // If shutdown was requested, don't reconnect.
    if *shutdown_rx.borrow() {
        return;
    }

    let mut backoff = BACKOFF_INITIAL;

    loop {
        warn!(delay = ?backoff, "connection lost, reconnecting");

        tokio::select! {
            _ = shutdown_rx.changed() => {
                info!("shutdown requested, stopping reconnection");
                return;
            }
            _ = tokio::time::sleep(backoff) => {}
        }

        if *shutdown_rx.borrow() {
            return;
        }

        match connect_async(&ws_url).await {
            Ok((ws, _)) => match handshake(ws, token.clone(), subdomain.clone()).await {
                Ok((_url, ws)) => {
                    info!("reconnected to tunnel server");
                    backoff = BACKOFF_INITIAL;

                    if let Err(e) = tunnel_loop(ws, local_addr, shutdown_rx.clone()).await {
                        error!(error = %e, "tunnel loop exited with error");
                    }

                    if *shutdown_rx.borrow() {
                        return;
                    }
                }
                Err(e) => {
                    error!(error = %e, "handshake failed after reconnect");
                }
            },
            Err(e) => {
                error!(error = %e, "reconnection failed");
            }
        }

        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Perform the Hello/Assigned handshake over the WebSocket.
async fn handshake(
    mut ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    token: Option<String>,
    subdomain: Option<String>,
) -> Result<(String, WebSocketStream<MaybeTlsStream<TcpStream>>)> {
    let hello = ClientMessage::Hello {
        token,
        requested_subdomain: subdomain,
    };
    let msg = serde_json::to_string(&hello)?;
    ws.send(Message::Text(msg.into())).await?;

    // Wait for the server's Assigned or Error response.
    while let Some(frame) = ws.next().await {
        let frame = frame.context("WebSocket error during handshake")?;
        match frame {
            Message::Text(text) => {
                let server_msg: ServerMessage = serde_json::from_str(&text)?;
                match server_msg {
                    ServerMessage::Assigned { url, subdomain } => {
                        info!(url = %url, subdomain = %subdomain, "tunnel assigned");
                        return Ok((url, ws));
                    }
                    ServerMessage::Error { message } => {
                        anyhow::bail!("server rejected tunnel: {message}");
                    }
                    _ => continue,
                }
            }
            Message::Ping(_) => continue,
            Message::Close(_) => anyhow::bail!("server closed connection during handshake"),
            _ => continue,
        }
    }
    anyhow::bail!("WebSocket closed before handshake completed")
}

/// Main loop: listen for control messages and binary request frames,
/// proxy each request to the local service, and send back the response.
async fn tunnel_loop(
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    local_addr: SocketAddr,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let (ws_tx, mut ws_rx) = ws.split();
    let ws_tx = Arc::new(Mutex::new(ws_tx));

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                info!("tunnel shutting down");
                let mut tx = ws_tx.lock().await;
                let _ = tx.close().await;
                return Ok(());
            }
            frame = ws_rx.next() => {
                let Some(frame) = frame else {
                    warn!("tunnel WebSocket closed by server");
                    return Ok(());
                };
                let frame = frame?;
                match frame {
                    Message::Text(text) => {
                        let server_msg: ServerMessage = serde_json::from_str(&text)?;
                        match server_msg {
                            ServerMessage::NewConnection { stream_id } => {
                                info!(%stream_id, "new visitor connection");
                            }
                            ServerMessage::Heartbeat => {
                                let pong = serde_json::to_string(&ClientMessage::Heartbeat)?;
                                let mut tx = ws_tx.lock().await;
                                let _ = tx.send(Message::Text(pong.into())).await;
                            }
                            ServerMessage::Error { message } => {
                                error!(%message, "server error");
                            }
                            _ => {}
                        }
                    }
                    Message::Binary(data) => {
                        let ws_tx = ws_tx.clone();
                        let local = local_addr;
                        tokio::spawn(async move {
                            handle_binary_frame(data.to_vec(), local, ws_tx).await;
                        });
                    }
                    Message::Ping(data) => {
                        let mut tx = ws_tx.lock().await;
                        let _ = tx.send(Message::Pong(data)).await;
                    }
                    Message::Close(_) => {
                        info!("server closed tunnel");
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Decode a binary request frame, proxy to local, and send back the response.
async fn handle_binary_frame(data: Vec<u8>, local_addr: SocketAddr, ws_tx: Arc<Mutex<WsSink>>) {
    let (stream_id, frame) = match decode_frame(&data) {
        Ok(f) => f,
        Err(e) => {
            warn!(error = %e, "failed to decode binary frame");
            return;
        }
    };

    let TunnelFrame::Request(request) = frame else {
        warn!(%stream_id, "expected request frame, got something else");
        return;
    };

    // Proxy to local service
    let response_frame = match proxy_to_local(local_addr, &request).await {
        Ok(resp) => TunnelFrame::Response(resp),
        Err(e) => {
            warn!(%stream_id, error = %e, "local proxy failed");
            TunnelFrame::Error(TunnelError {
                message: e.to_string(),
            })
        }
    };

    // Encode and send back
    let binary = encode(stream_id, &response_frame);
    let mut tx = ws_tx.lock().await;
    if let Err(e) = tx.send(Message::Binary(binary.into())).await {
        warn!(%stream_id, error = %e, "failed to send response frame");
    }
}

/// Forward a TunnelRequest to the local service and collect the response.
async fn proxy_to_local(local_addr: SocketAddr, request: &TunnelRequest) -> Result<TunnelResponse> {
    let stream = TcpStream::connect(local_addr)
        .await
        .context("failed to connect to local service")?;

    let io = TokioIo::new(stream);

    let (mut sender, conn) = http1::Builder::new()
        .handshake::<_, Full<Bytes>>(io)
        .await
        .context("HTTP handshake with local service failed")?;

    // Drive the connection in the background
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            warn!(error = %e, "local HTTP connection error");
        }
    });

    // Build the hyper request from the TunnelRequest
    let method: hyper::Method = request.method.parse().context("invalid HTTP method")?;

    let mut builder = Request::builder().method(method).uri(&request.uri);

    for (name, value) in &request.headers {
        builder = builder.header(name.as_str(), value.as_slice());
    }

    let body = if request.body.is_empty() {
        Full::new(Bytes::new())
    } else {
        Full::new(Bytes::from(request.body.clone()))
    };

    let hyper_req = builder.body(body).context("failed to build request")?;

    // Send and collect response
    let resp: Response<Incoming> = sender
        .send_request(hyper_req)
        .await
        .context("local service request failed")?;

    let status = resp.status().as_u16();
    let headers: Vec<(String, Vec<u8>)> = resp
        .headers()
        .iter()
        .map(|(name, value)| (name.to_string(), value.as_bytes().to_vec()))
        .collect();

    let body = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .context("failed to read local response body")?
        .to_bytes()
        .to_vec();

    Ok(TunnelResponse {
        status,
        headers,
        body,
    })
}
