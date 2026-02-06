# Burrow

Burrow is a tunnel that exposes local HTTP servers to the internet through public subdomains. It works like ngrok: run the server on a public host, run the client alongside your local service, and get a URL like `https://myapp.yourdomain.com` that forwards traffic to `localhost:3000`.

## Architecture

```
                     Internet
                        |
         +--------------+--------------+
         |        burrow-server        |
         |   (public host, port 8080)  |
         +--------------+--------------+
                        |
                   WebSocket
                   (ws/wss)
                        |
         +--------------+--------------+
         |        burrow-client        |
         |   (your machine / app)      |
         +--------------+--------------+
                        |
                  localhost:3000
                  (your service)
```

**Request flow:** visitor HTTP request &rarr; server collects it &rarr; binary frame over WebSocket &rarr; client decodes it &rarr; forwards to localhost &rarr; collects response &rarr; binary frame back over WebSocket &rarr; server sends HTTP response to visitor.

## Crates

| Crate | Description |
|---|---|
| `burrow-core` | Shared protocol types and binary frame encoding |
| `burrow-server` | Tunnel server binary &mdash; accepts clients and routes visitor traffic |
| `burrow-client` | Embeddable client library &mdash; connect a tunnel from your own Rust code |
| `burrow-cli` | Standalone CLI tool wrapping `burrow-client` |

## Quick start

### 1. Build

```bash
cargo build --release
```

### 2. Run the server

On your public host:

```bash
./target/release/burrow-server \
  --domain yourdomain.com \
  --listen 0.0.0.0:8080
```

You need a **wildcard DNS** record pointing `*.yourdomain.com` to this host. For local testing, `localhost` works as the domain with `/etc/hosts` or just the Host header.

### 3. Connect with the CLI

On your machine, expose a local port:

```bash
./target/release/burrow-cli \
  --port 3000 \
  --server ws://yourdomain.com:8080
```

Output:

```
  burrow tunnel is live!

  Public URL:  https://abc123.yourdomain.com
  Forwarding:  https://abc123.yourdomain.com → 127.0.0.1:3000
```

You can also request a specific subdomain:

```bash
burrow-cli -p 3000 -s ws://yourdomain.com:8080 --subdomain myapp
```

### 4. Test it

```bash
curl -H "Host: abc123.yourdomain.com" http://yourdomain.com:8080/
```

## Server configuration

All options can be set via CLI flags or environment variables.

| Flag | Env var | Default | Description |
|---|---|---|---|
| `--domain` | `BURROW_DOMAIN` | *required* | Base domain for subdomains (e.g. `example.com`) |
| `--listen` | `BURROW_LISTEN` | `0.0.0.0:8080` | Address to bind the HTTP server |
| `--auth-tokens` | `BURROW_AUTH_TOKENS` | *none* | Comma-separated allow-list of tokens. If unset, no auth. |

### Authentication

To require clients to authenticate, pass a token list:

```bash
burrow-server --domain example.com --auth-tokens "secret1,secret2"
```

Clients must then provide a matching token:

```bash
burrow-cli -p 3000 -s ws://example.com:8080 --token secret1
```

### Running behind a reverse proxy (nginx, Caddy)

Burrow speaks plain HTTP on its listen port. Put it behind a reverse proxy to add TLS:

**Caddy** (automatic HTTPS with wildcard cert):

```
*.yourdomain.com {
    reverse_proxy localhost:8080
}
```

**nginx**:

```nginx
server {
    listen 443 ssl;
    server_name *.yourdomain.com;

    ssl_certificate     /path/to/wildcard.crt;
    ssl_certificate_key /path/to/wildcard.key;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
    }
}
```

The `Upgrade` / `Connection` headers are required for WebSocket tunnel connections to work.

### Docker

```dockerfile
FROM rust:1 AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p burrow-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/burrow-server /usr/local/bin/
ENTRYPOINT ["burrow-server"]
```

```bash
docker build -t burrow-server .
docker run -p 8080:8080 burrow-server --domain yourdomain.com
```

## Library integration

The `burrow-client` crate lets you embed a tunnel directly in your Rust application. This is useful for development servers, CI preview environments, or any tool that needs a public URL for a local service.

### Add the dependency

```toml
[dependencies]
burrow-client = { path = "crates/burrow-client" }  # or from your registry
tokio = { version = "1", features = ["full"] }
```

### Basic usage

```rust
use std::net::SocketAddr;
use burrow_client::TunnelClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let local_addr: SocketAddr = "127.0.0.1:3000".parse()?;

    let handle = TunnelClient::new("ws://tunnel.yourdomain.com:8080", local_addr)
        .connect()
        .await?;

    println!("Tunnel live at: {}", handle.url());

    // The tunnel runs in the background. Your app keeps doing its thing.
    // When you're done:
    handle.close().await;

    Ok(())
}
```

### With authentication and subdomain

```rust
let handle = TunnelClient::new("wss://tunnel.yourdomain.com", local_addr)
    .token("my-secret-token")
    .subdomain("preview-pr-42")
    .connect()
    .await?;
```

### Example: dev server with tunnel

```rust
use std::net::SocketAddr;
use burrow_client::TunnelClient;

async fn start_with_tunnel(port: u16, tunnel_server: &str) -> anyhow::Result<()> {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;

    // Start your HTTP server on `addr` (axum, actix, warp, etc.)
    // ...

    // Open a tunnel so the outside world can reach it
    let tunnel = TunnelClient::new(tunnel_server, addr)
        .connect()
        .await?;

    println!("Public URL: {}", tunnel.url());

    // Keep running until shutdown signal
    tokio::signal::ctrl_c().await?;
    tunnel.close().await;
    Ok(())
}
```

### API reference

**`TunnelClient`** &mdash; builder

| Method | Description |
|---|---|
| `new(server_url, local_addr)` | Create a new client targeting the given server and local address |
| `.token(token)` | Set an auth token (must match server's allow-list) |
| `.subdomain(name)` | Request a specific subdomain |
| `.connect().await` | Connect and return a `TunnelHandle` |

**`TunnelHandle`** &mdash; running tunnel

| Method | Description |
|---|---|
| `.url()` | The assigned public URL (e.g. `https://myapp.yourdomain.com`) |
| `.close().await` | Gracefully shut down the tunnel |

## Limitations

- **Request/response only.** The current implementation buffers full HTTP requests and responses. It does not support streaming, WebSocket passthrough, or Server-Sent Events through the tunnel. A future version could add yamux multiplexing for streaming support.
- **Body size.** Request bodies are limited to 10 MB by default (server-side constant).
- **30-second timeout.** If the tunnel client doesn't respond within 30 seconds, the visitor gets a 504.
- **Single connection.** Each tunnel client uses one WebSocket connection. Concurrent visitor requests are multiplexed as independent binary frames over that connection.

## License

MIT
