//! Tracing/logging setup.
//!
//! Two output sinks:
//!   - `stderr` (text format) when running in foreground (TTY)
//!   - `~/.loop/logs/daemon.log` (JSON format, line-buffered) when detached
//!
//! Filtering via `RUST_LOG` env var (e.g. `RUST_LOG=debug` for verbose).
//! Defaults to `loop_daemon=info`.

use std::fs::OpenOptions;
use std::io;
use std::path::Path;

use anyhow::Result;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

const DEFAULT_FILTER: &str = "loop_daemon=info,warn";

pub fn init_foreground() -> Result<()> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(io::stderr).with_target(true))
        .init();
    Ok(())
}

pub fn init_detached(log_path: &Path) -> Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().json().with_writer(file).with_target(true))
        .init();
    Ok(())
}
