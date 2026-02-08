# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Development Commands

```bash
cargo check --workspace          # Quick compile check
cargo build --workspace           # Dev build
cargo build --release -p burrow-server  # Release server binary
cargo test --workspace            # Run all tests
cargo test -p burrow-core         # Test a single crate
cargo clippy --workspace -- -D warnings  # Lint (CI enforces no warnings)
cargo fmt --all -- --check        # Check formatting
cargo fmt --all                   # Auto-format
```

## Architecture

Burrow is an HTTP tunnel (like ngrok) with 4 workspace crates:

- **burrow-core** — Shared protocol types and binary frame encoding/decoding. Published to crates.io.
- **burrow-client** — Embeddable library that connects to a burrow server and proxies traffic to a local service. Re-exports `burrow_core`. Published to crates.io.
- **burrow-server** — Deployable binary. Routes visitor HTTP by subdomain and bridges to tunnel clients via WebSocket. Not published.
- **burrow-cli** — Thin CLI wrapper around `burrow-client`. Not published.

### Control Plane (JSON over WebSocket text frames)

Client sends `Hello` → Server replies `Assigned { subdomain, url }`. Server sends `NewConnection { stream_id }` when a visitor arrives. Both sides exchange `Heartbeat` for keepalive.

### Data Plane (binary over WebSocket binary frames)

Wire format: `[16-byte UUID stream_id][1-byte frame type][payload]`. Frame types: Request (0x01), Response (0x02), Error (0x03). Strings and bodies are length-prefixed (4 bytes). Encoding/decoding lives in `burrow-core/src/frame.rs`.

### Request Flow

Visitor → server extracts subdomain from `Host` header → looks up tunnel in `TunnelRegistry` → converts HTTP to `TunnelRequest` → sends control message + binary frame atomically over WebSocket → client decodes and proxies to `localhost` → client sends binary response frame back → server converts to HTTP response → visitor.

### Server Routing (`server.rs:handle_request`)

Two paths based on the incoming request:
1. `path == "/burrow"` + WebSocket upgrade → tunnel client registration
2. Everything else → extract subdomain from Host, proxy to matching tunnel

### Key Constants

- `TUNNEL_WS_PATH = "/burrow"` — WebSocket endpoint for tunnel clients
- `PROXY_TIMEOUT = 30s` — Max wait for client response
- `MAX_BODY_SIZE = 10MB` — Request body limit
- `RESERVED_SUBDOMAINS = ["new", "www", "api", "admin", "app"]` — Cannot be claimed by tunnels
- Client reconnection: exponential backoff 1s → 60s max

## Deployment

Server is deployed on Fly.io at `meilisearch.link`. Tunnel clients connect to `wss://new.meilisearch.link/burrow`, visitors access tunnels at `https://{subdomain}.meilisearch.link`.

The Dockerfile uses `rust:1.85-bookworm` (edition 2024 requires 1.85+). Server speaks plain HTTP; TLS is terminated by the platform/reverse proxy. The server hardcodes `https://` in generated tunnel URLs.

## Publishing

Push a `v*` tag to trigger the release workflow which publishes `burrow-core` then `burrow-client` to crates.io. The `CARGO_REGISTRY_TOKEN` secret must be set in GitHub.
