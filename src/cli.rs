//! Command-line interface definitions.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "loop-engine",
    about = "Live verification daemon for Loop — watches Claude Code sessions and emits sentiment signals",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the daemon. Detaches into the background by default; use
    /// --foreground to run attached to the current terminal.
    Run {
        /// Run attached to the current terminal (no fork/setsid). Useful
        /// for development and process-supervisor setups (systemd/launchd).
        #[arg(long)]
        foreground: bool,
    },
    /// Report current daemon status.
    Status,
    /// Send SIGTERM to the running daemon.
    Stop,
}
