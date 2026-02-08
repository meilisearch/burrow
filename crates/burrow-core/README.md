# burrow-core

Shared protocol types and binary frame encoding for the [burrow](https://github.com/meilisearch/burrow) tunnel system.

## What's inside

- **Control-plane messages** (`ClientMessage`, `ServerMessage`) — JSON over WebSocket text frames for tunnel lifecycle management (handshake, connection signaling, heartbeat).
- **Data-plane framing** (`encode`, `decode_frame`) — binary encoding for HTTP requests and responses carried over WebSocket binary frames.

### Wire format

```
[16-byte UUID stream_id][1-byte frame type][payload]
```

Frame types: `0x01` Request, `0x02` Response, `0x03` Error.

## Usage

This crate is a dependency of `burrow-client` and `burrow-server`. You typically don't need to depend on it directly — `burrow-client` re-exports it as `burrow_client::burrow_core`.

## License

MIT
