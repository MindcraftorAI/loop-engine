//! Command-line interface definitions.

use std::path::PathBuf;

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
    /// Serve as a JSON-RPC 2.0 endpoint. Defaults to line-delimited
    /// JSON-RPC 2.0 over stdio (other processes — notably the opensquid
    /// MCP server — spawn this and drive the engine programmatically).
    ///
    /// Pass `--socket <PATH>` to bind a Unix-domain-socket listener
    /// instead. UDS mode is the long-running daemon shape — one engine
    /// process serves many concurrent connections across opensquid
    /// hooks + sessions, with the HNSW vector index rehydrated exactly
    /// once at startup and shared across all connections via
    /// `Arc<ServeState>`. UDS is Unix-only; Windows callers fall back
    /// to stdio mode (named-pipe support tracked as a follow-up).
    ///
    /// Method surface lives in `serve.rs`. Diagnostics go to stderr in
    /// either mode.
    Serve {
        /// Bind a Unix-domain-socket listener at PATH instead of using
        /// stdio. Per-connection `tokio::spawn` for cross-connection
        /// concurrency; per-connection requests still process
        /// sequentially. Path-byte limit is platform-dependent
        /// (macOS=104, Linux=108) — callers should keep paths short.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}
