//! `selenite-mcp-stdio <vault-folder>`: serve a vault over MCP on stdin/stdout.
//!
//! A small standalone entry point (also used by the integration test). The CLI's
//! `selenite mcp` command wraps the same [`selenite_mcp::run_stdio`]. stdout carries the
//! protocol, so diagnostics go to stderr only.

use std::path::PathBuf;
use std::process::ExitCode;

use selenite_index::Engine;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    let Some(folder) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: selenite-mcp-stdio <vault-folder>");
        return ExitCode::from(2);
    };
    let engine = match Engine::open_local(&folder) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("cannot open vault {}: {e}", folder.display());
            return ExitCode::FAILURE;
        }
    };
    match selenite_mcp::run_stdio(engine) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
