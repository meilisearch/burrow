use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio_tungstenite::WebSocketStream;
use tracing::{error, info};

use burrow_core::{ClientMessage, ServerMessage, TUNNEL_WS_PATH, TunnelRequest, TunnelResponse};

use crate::tunnel::{self, TunnelRegistry};

const PROXY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY_SIZE: usize = 10 * 1024 * 1024; // 10 MB

pub struct ServerConfig {
    pub domain: String,
    pub listen: String,
    pub allowed_tokens: Option<Vec<String>>,
}

pub async fn run(config: ServerConfig) -> Result<()> {
    let addr: SocketAddr = config.listen.parse().context("invalid listen address")?;
    let registry = Arc::new(TunnelRegistry::new(
        config.domain.clone(),
        config.allowed_tokens,
    ));

    let listener = TcpListener::bind(addr).await?;
    info!(addr = %addr, domain = %config.domain, "burrow server listening");

    loop {
        let (stream, peer) = listener.accept().await?;
        let registry = registry.clone();
        let domain = config.domain.clone();

        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let registry = registry.clone();
            let domain = domain.clone();

            let service = service_fn(move |req: Request<Incoming>| {
                let registry = registry.clone();
                let domain = domain.clone();
                async move { handle_request(req, registry, &domain, peer).await }
            });

            if let Err(e) = http1::Builder::new()
                .serve_connection(io, service)
                .with_upgrades()
                .await
                && !e.to_string().contains("connection closed")
            {
                error!(peer = %peer, error = %e, "connection error");
            }
        });
    }
}

async fn handle_request(
    req: Request<Incoming>,
    registry: Arc<TunnelRegistry>,
    domain: &str,
    peer: SocketAddr,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let host = req
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");

    // Check if this is a tunnel client registering via WebSocket.
    if req.uri().path() == TUNNEL_WS_PATH && is_websocket_upgrade(&req) {
        info!(peer = %peer, "tunnel client connecting");
        return handle_tunnel_upgrade(req, registry).await;
    }

    // Otherwise, route to a tunnel by subdomain.
    let subdomain = extract_subdomain(host, domain);

    let Some(subdomain) = subdomain else {
        return Ok(response(StatusCode::NOT_FOUND, "no tunnel found"));
    };

    let Some(tunnel) = registry.get(&subdomain).await else {
        return Ok(response(
            StatusCode::NOT_FOUND,
            format!("tunnel '{subdomain}' not found"),
        ));
    };

    // Convert the incoming hyper request to a TunnelRequest.
    let tunnel_request = match request_to_tunnel(req).await {
        Ok(r) => r,
        Err(e) => {
            error!(subdomain = %subdomain, error = %e, "failed to read visitor request");
            return Ok(response(StatusCode::BAD_REQUEST, "bad request"));
        }
    };

    // Send to the tunnel client and wait for a response.
    let (stream_id, rx) = match tunnel
        .send_new_connection_with_request(&tunnel_request)
        .await
    {
        Ok(pair) => pair,
        Err(e) => {
            error!(subdomain = %subdomain, error = %e, "failed to send to tunnel client");
            return Ok(response(
                StatusCode::BAD_GATEWAY,
                "tunnel client unreachable",
            ));
        }
    };

    info!(subdomain = %subdomain, %stream_id, "visitor request forwarded");

    // Wait for the client's response with a timeout.
    match tokio::time::timeout(PROXY_TIMEOUT, rx).await {
        Ok(Ok(tunnel_resp)) => Ok(tunnel_response_to_hyper(tunnel_resp)),
        Ok(Err(_recv_err)) => {
            // The sender was dropped — client disconnected
            tunnel.cancel_pending(&stream_id).await;
            Ok(response(
                StatusCode::BAD_GATEWAY,
                "tunnel client disconnected",
            ))
        }
        Err(_timeout) => {
            tunnel.cancel_pending(&stream_id).await;
            Ok(response(
                StatusCode::GATEWAY_TIMEOUT,
                "tunnel client did not respond in time",
            ))
        }
    }
}

/// Convert a hyper `Request<Incoming>` into a `TunnelRequest`.
async fn request_to_tunnel(req: Request<Incoming>) -> Result<TunnelRequest> {
    let method = req.method().to_string();
    let uri = req.uri().to_string();

    let headers: Vec<(String, Vec<u8>)> = req
        .headers()
        .iter()
        .map(|(name, value)| (name.to_string(), value.as_bytes().to_vec()))
        .collect();

    let body = http_body_util::BodyExt::collect(req.into_body())
        .await
        .context("failed to read request body")?
        .to_bytes()
        .to_vec();

    if body.len() > MAX_BODY_SIZE {
        anyhow::bail!("request body too large: {} bytes", body.len());
    }

    Ok(TunnelRequest {
        method,
        uri,
        headers,
        body,
    })
}

/// Convert a `TunnelResponse` back into a hyper `Response`.
fn tunnel_response_to_hyper(resp: TunnelResponse) -> Response<Full<Bytes>> {
    let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut builder = Response::builder().status(status);

    for (name, value) in &resp.headers {
        builder = builder.header(name.as_str(), value.as_slice());
    }

    builder
        .body(Full::new(Bytes::from(resp.body)))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Full::new(Bytes::from("failed to build response")))
                .unwrap()
        })
}

/// Handle a WebSocket upgrade request from a tunnel client.
async fn handle_tunnel_upgrade(
    req: Request<Incoming>,
    registry: Arc<TunnelRegistry>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let (response, fut) = match hyper_tungstenite_upgrade(req) {
        Ok(pair) => pair,
        Err(e) => {
            error!(error = %e, "WebSocket upgrade failed");
            return Ok(self::response(
                StatusCode::BAD_REQUEST,
                "WebSocket upgrade failed",
            ));
        }
    };

    tokio::spawn(async move {
        match fut.await {
            Ok(ws) => {
                if let Err(e) = handle_tunnel_client(ws, registry).await {
                    error!(error = %e, "tunnel client session error");
                }
            }
            Err(e) => error!(error = %e, "WebSocket upgrade future failed"),
        }
    });

    Ok(response)
}

/// Process the Hello handshake and then manage the tunnel session.
async fn handle_tunnel_client(
    ws: WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>,
    registry: Arc<TunnelRegistry>,
) -> Result<()> {
    let (ws_tx, mut ws_rx) = ws.split();

    // Wait for the client's Hello message.
    let (token, requested_subdomain) = loop {
        let Some(msg) = ws_rx.next().await else {
            anyhow::bail!("client disconnected before Hello");
        };
        let msg = msg?;
        if let tungstenite::Message::Text(text) = msg {
            let client_msg: ClientMessage = serde_json::from_str(&text)?;
            match client_msg {
                ClientMessage::Hello {
                    token,
                    requested_subdomain,
                } => {
                    break (token, requested_subdomain);
                }
                _ => continue,
            }
        }
    };

    // Validate auth token.
    if !registry.validate_token(&token) {
        let err = ServerMessage::Error {
            message: "invalid or missing auth token".to_string(),
        };
        let mut ws_tx = ws_tx;
        use futures_util::SinkExt;
        let _ = ws_tx
            .send(tungstenite::Message::Text(
                serde_json::to_string(&err)?.into(),
            ))
            .await;
        anyhow::bail!("client auth failed");
    }

    let (subdomain, url, tunnel_handle) = registry.register(ws_tx, requested_subdomain).await?;

    // Send the Assigned message.
    tunnel_handle
        .send_message(&ServerMessage::Assigned {
            subdomain: subdomain.clone(),
            url,
        })
        .await?;

    // Enter the main loop handling client messages.
    tunnel::handle_client_messages(tunnel_handle, ws_rx, registry).await;

    Ok(())
}

/// Perform a manual hyper WebSocket upgrade using tungstenite.
#[allow(clippy::type_complexity)]
fn hyper_tungstenite_upgrade(
    req: Request<Incoming>,
) -> Result<(
    Response<Full<Bytes>>,
    impl std::future::Future<Output = Result<WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>>>,
)> {
    use hyper::header::{CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, UPGRADE};
    use tungstenite::handshake::derive_accept_key;

    let key = req
        .headers()
        .get(SEC_WEBSOCKET_KEY)
        .ok_or_else(|| anyhow::anyhow!("missing Sec-WebSocket-Key header"))?
        .to_str()?
        .to_string();

    let accept_key = derive_accept_key(key.as_bytes());

    let response = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(UPGRADE, "websocket")
        .header(CONNECTION, "Upgrade")
        .header(SEC_WEBSOCKET_ACCEPT, accept_key)
        .body(Full::new(Bytes::new()))?;

    let upgrade_fut = async move {
        let upgraded = hyper::upgrade::on(req).await?;
        let ws = WebSocketStream::from_raw_socket(
            TokioIo::new(upgraded),
            tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        Ok(ws)
    };

    Ok((response, upgrade_fut))
}

fn is_websocket_upgrade<T>(req: &Request<T>) -> bool {
    req.headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// Extract the subdomain from a Host header value.
/// e.g. "abc123.example.com" with domain "example.com" → Some("abc123")
fn extract_subdomain(host: &str, domain: &str) -> Option<String> {
    let host = host.split(':').next().unwrap_or(host);
    let suffix = format!(".{domain}");
    if host.ends_with(&suffix) && host.len() > suffix.len() {
        let subdomain = &host[..host.len() - suffix.len()];
        if !subdomain.is_empty() && !subdomain.contains('.') {
            return Some(subdomain.to_lowercase());
        }
    }
    None
}

fn response(status: StatusCode, body: impl Into<String>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(Full::new(Bytes::from(body.into())))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_subdomain() {
        assert_eq!(
            extract_subdomain("abc123.example.com", "example.com"),
            Some("abc123".to_string())
        );
        assert_eq!(
            extract_subdomain("ABC.example.com", "example.com"),
            Some("abc".to_string())
        );
        assert_eq!(extract_subdomain("example.com", "example.com"), None);
        assert_eq!(extract_subdomain("other.com", "example.com"), None);
        assert_eq!(
            extract_subdomain("abc123.example.com:8080", "example.com"),
            Some("abc123".to_string())
        );
        assert_eq!(
            extract_subdomain("deep.sub.example.com", "example.com"),
            None
        );
    }
}
