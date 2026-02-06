use std::net::SocketAddr;
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tracing::{error, info, warn};

use burrow_core::{ClientMessage, ServerMessage, TUNNEL_WS_PATH};

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

        let (assigned_url, ws) = handshake(ws, self.token, self.subdomain).await?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let local_addr = self.local_addr;
        let url = assigned_url.clone();

        let task = tokio::spawn(async move {
            if let Err(e) = tunnel_loop(ws, local_addr, shutdown_rx).await {
                error!(error = %e, "tunnel loop exited with error");
            }
        });

        Ok(TunnelHandle {
            url,
            _task: task,
            shutdown_tx,
        })
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

/// Main loop: listen for NewConnection messages and proxy each one.
async fn tunnel_loop(
    mut ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    local_addr: SocketAddr,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                info!("tunnel shutting down");
                let _ = ws.close(None).await;
                return Ok(());
            }
            frame = ws.next() => {
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
                                let local = local_addr;
                                tokio::spawn(async move {
                                    if let Err(e) = proxy_connection(stream_id, local).await {
                                        warn!(%stream_id, error = %e, "proxy connection failed");
                                    }
                                });
                            }
                            ServerMessage::Heartbeat => {
                                let pong = serde_json::to_string(&ClientMessage::Heartbeat)?;
                                let _ = ws.send(Message::Text(pong.into())).await;
                            }
                            ServerMessage::Error { message } => {
                                error!(%message, "server error");
                            }
                            _ => {}
                        }
                    }
                    Message::Ping(data) => {
                        let _ = ws.send(Message::Pong(data)).await;
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

/// Proxy a single visitor connection: connect to the local service and relay bytes.
///
/// TODO: This is a placeholder. The full implementation will use yamux streams
/// multiplexed over the WebSocket rather than separate TCP connections.
/// For the initial version, the server sends the full HTTP request/response
/// as binary WebSocket frames tagged with the stream_id.
async fn proxy_connection(stream_id: uuid::Uuid, local_addr: SocketAddr) -> Result<()> {
    let _local = TcpStream::connect(local_addr)
        .await
        .context("failed to connect to local service")?;

    info!(%stream_id, "connected to local service, proxying...");

    // TODO: Read from yamux stream, forward to local, relay response back.
    // This will be implemented when we wire up the yamux multiplexing layer.

    Ok(())
}
