use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Messages sent from the tunnel client to the server over the WebSocket control channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Initial handshake: client announces itself and optionally requests a subdomain.
    Hello {
        token: Option<String>,
        requested_subdomain: Option<String>,
    },
    /// Client acknowledges it is ready to handle a proxied connection.
    Accept { stream_id: Uuid },
    /// Keep-alive ping from the client.
    Heartbeat,
}

/// Messages sent from the tunnel server to the client over the WebSocket control channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Server confirms the tunnel is live and provides the assigned public URL.
    Assigned { subdomain: String, url: String },
    /// Server notifies the client that a new visitor connection arrived.
    /// The client should open a new yamux stream tagged with this stream_id
    /// and relay traffic to its local service.
    NewConnection { stream_id: Uuid },
    /// Keep-alive pong from the server.
    Heartbeat,
    /// Something went wrong.
    Error { message: String },
}

/// Framing header sent at the start of each yamux stream to identify which
/// visitor connection this stream belongs to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamHeader {
    pub stream_id: Uuid,
}

/// Default WebSocket path for tunnel client registration.
pub const TUNNEL_WS_PATH: &str = "/burrow";

/// Default server port.
pub const DEFAULT_PORT: u16 = 8080;
