mod server;
mod tunnel;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[clap(name = "burrow-server", about = "Tunnel server for burrow")]
struct Cli {
    /// Domain for tunnel subdomains (e.g. "example.com" → abc123.example.com).
    #[clap(long, env = "BURROW_DOMAIN")]
    domain: String,

    /// Address to listen on.
    #[clap(long, env = "BURROW_LISTEN", default_value = "0.0.0.0:8080")]
    listen: String,

    /// Optional comma-separated list of allowed auth tokens.
    /// If empty, no authentication is required.
    #[clap(long, env = "BURROW_AUTH_TOKENS")]
    auth_tokens: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("burrow=info".parse()?))
        .init();

    let cli = Cli::parse();

    let allowed_tokens: Option<Vec<String>> = cli.auth_tokens.map(|t| {
        t.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });

    let config = server::ServerConfig {
        domain: cli.domain,
        listen: cli.listen,
        allowed_tokens,
    };

    server::run(config).await
}
