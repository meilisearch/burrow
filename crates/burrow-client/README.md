# burrow-client

Embeddable Rust library to expose a local HTTP server to the internet through a [burrow](https://github.com/meilisearch/burrow) tunnel. Get a public URL like `https://myapp.meilisearch.link` forwarding to `localhost:3000`.

## Install

```toml
[dependencies]
burrow-client = "0.1"
tokio = { version = "1", features = ["full"] }
```

## Usage

```rust
use burrow_client::TunnelClient;
use std::net::SocketAddr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let local_addr: SocketAddr = "127.0.0.1:3000".parse()?;

    let handle = TunnelClient::new("wss://new.meilisearch.link", local_addr)
        .subdomain("myapp")
        .connect()
        .await?;

    println!("Tunnel live at: {}", handle.url());

    tokio::signal::ctrl_c().await?;
    handle.close().await;
    Ok(())
}
```

## API

**`TunnelClient`** — builder

| Method | Description |
|---|---|
| `new(server_url, local_addr)` | Create a client targeting the given server and local address |
| `.token(token)` | Set an auth token |
| `.subdomain(name)` | Request a specific subdomain |
| `.connect().await` | Connect and return a `TunnelHandle` |

**`TunnelHandle`** — running tunnel

| Method | Description |
|---|---|
| `.url()` | The assigned public URL |
| `.close().await` | Gracefully shut down the tunnel |

## Features

- Automatic reconnection with exponential backoff (1s to 60s)
- Concurrent request multiplexing over a single WebSocket
- Optional authentication and subdomain selection

## License

MIT
