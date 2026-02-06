use std::net::SocketAddr;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use burrow_client::TunnelClient;

#[derive(Debug, Parser)]
#[clap(name = "burrow", about = "Expose a local port through a burrow tunnel")]
struct Cli {
    /// Local port to expose.
    #[clap(short, long)]
    port: u16,

    /// Tunnel server URL (e.g. "wss://tunnel.example.com" or "ws://localhost:8080").
    #[clap(short, long, env = "BURROW_SERVER")]
    server: String,

    /// Optional auth token.
    #[clap(short, long, env = "BURROW_TOKEN")]
    token: Option<String>,

    /// Request a specific subdomain.
    #[clap(long)]
    subdomain: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("burrow=info".parse()?))
        .init();

    let cli = Cli::parse();

    let local_addr: SocketAddr = format!("127.0.0.1:{}", cli.port).parse()?;

    let mut client = TunnelClient::new(&cli.server, local_addr);

    if let Some(token) = cli.token {
        client = client.token(token);
    }
    if let Some(subdomain) = cli.subdomain {
        client = client.subdomain(subdomain);
    }

    let handle = client.connect().await?;

    eprintln!();
    eprintln!("  burrow tunnel is live!");
    eprintln!();
    eprintln!("  Public URL:  {}", handle.url());
    eprintln!("  Forwarding:  {} → {}", handle.url(), local_addr);
    eprintln!();

    // Wait for Ctrl+C.
    tokio::signal::ctrl_c().await?;
    eprintln!("shutting down...");
    handle.close().await;

    Ok(())
}
