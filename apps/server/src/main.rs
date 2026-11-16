//! `selenite-server`: serve a vault over MCP Streamable HTTP (Phase 6).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use selenite_server::auth::{self, TokenSource};
use selenite_server::{ServerConfig, run};

/// Headless server: MCP over Streamable HTTP at `/mcp`, with optional background sync.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// Vault folder to serve.
    #[arg(long, required_unless_present = "print_token")]
    vault: Option<PathBuf>,
    /// Address to listen on. Anything but loopback should sit behind a TLS reverse proxy.
    #[arg(long, default_value = "127.0.0.1:8787")]
    listen: SocketAddr,
    /// Seconds between background syncs (when the vault has remotes).
    #[arg(long, default_value_t = 300)]
    sync_interval: u64,
    /// Never sync, even if the vault has remotes.
    #[arg(long)]
    no_sync: bool,
    /// Extra allowed `Origin` (for example https://notes.example.com). Repeatable.
    #[arg(long = "allowed-origin")]
    allowed_origins: Vec<String>,
    /// Extra allowed `Host` (for example notes.example.com). Repeatable.
    #[arg(long = "allowed-host")]
    allowed_hosts: Vec<String>,
    /// Print the access token to stdout and exit (creates the token file if needed).
    #[arg(long)]
    print_token: bool,
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();

    let (token, source) = auth::resolve_default_token().context("access token")?;
    if args.print_token {
        println!("{token}");
        return Ok(());
    }
    match &source {
        TokenSource::Created(path) => eprintln!(
            "Created an access token file at {}.\nShow the token (once) with: selenite-server --print-token",
            path.display()
        ),
        TokenSource::File(path) => tracing::info!(path = %path.display(), "using token file"),
        TokenSource::Env => tracing::info!("using token from {}", auth::TOKEN_ENV),
    }

    let Some(vault) = args.vault else {
        bail!("--vault is required");
    };
    if !args.listen.ip().is_loopback() {
        tracing::warn!(
            listen = %args.listen,
            "listening on a non-loopback address over plain HTTP: put a TLS-terminating reverse \
             proxy (Caddy, nginx) in front, or the bearer token travels unencrypted"
        );
    }
    let config = ServerConfig {
        vault,
        listen: args.listen,
        token,
        sync_interval: (!args.no_sync).then(|| Duration::from_secs(args.sync_interval.max(10))),
        allowed_origins: args.allowed_origins,
        allowed_hosts: args.allowed_hosts,
    };
    let (addr, handle) = run(config).context("starting the server")?;
    tracing::info!(%addr, "MCP endpoint is /mcp (bearer token required)");

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("signal runtime")?
        .block_on(wait_for_signal());
    tracing::info!("shutting down");
    handle.shutdown();
    Ok(())
}
