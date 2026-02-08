# burrow-cli

CLI tool to expose a local HTTP server to the internet through a [burrow](https://github.com/meilisearch/burrow) tunnel.

## Install

```sh
cargo install burrow-cli
```

## Usage

```sh
burrow --port 3000 --server wss://new.meilisearch.link
```

Output:

```
  burrow tunnel is live!

  Public URL:  https://a1b2c3d4.meilisearch.link
  Forwarding:  https://a1b2c3d4.meilisearch.link -> 127.0.0.1:3000
```

### Options

| Flag | Env var | Description |
|---|---|---|
| `-p, --port` | | Local port to expose |
| `-s, --server` | `BURROW_SERVER` | Tunnel server URL |
| `-t, --token` | `BURROW_TOKEN` | Auth token (if server requires it) |
| `--subdomain` | | Request a specific subdomain |

## License

MIT
