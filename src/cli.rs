//! Command-line interface definitions.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "loop-engine",
    about = "loop-engine — cognitive-memory substrate for AI agents (host-adapter daemon)",
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
    /// Serve as a JSON-RPC 2.0 endpoint over stdio. Other processes
    /// (notably the opensquid MCP server) spawn this and drive the
    /// engine programmatically. Method surface lives in `serve.rs`.
    ///
    /// Protocol: line-delimited JSON-RPC 2.0 over stdin/stdout.
    /// Diagnostics go to stderr.
    Serve,
}
