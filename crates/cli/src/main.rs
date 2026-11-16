//! Command-line interface for the vault engine.

use clap::{Parser, Subcommand};
use selenite_core::APP_NAME;

#[derive(Parser)]
#[command(version, about = "Local-first notes, workspace and tasks engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new vault at PATH.
    Init {
        /// Folder to create the vault in.
        path: std::path::PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { .. } => anyhow::bail!("{APP_NAME}: `init` is not implemented yet"),
    }
}
