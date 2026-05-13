//! Process lifecycle: PID file, daemonization, signal handling, shutdown.
//!
//! Two run modes:
//!   - Foreground (`--foreground` flag): no detach, logs to stderr
//!   - Detached (default `run` behavior): fork via `daemonize`, log to file
//!
//! In both modes, SIGTERM/SIGINT/SIGHUP trigger graceful shutdown via a
//! `tokio_util::sync::CancellationToken` — sticky semantics so a signal
//! that arrives BEFORE a waiter registers still wakes the next waiter
//! (audit Day 10 caught a Notify-based race here).

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::DaemonConfig;
use crate::engine::paths;
use crate::engine::pid::pid_is_alive;

/// Write current PID to ~/.loop/daemon.pid. Refuses to overwrite if a
/// PID file already exists AND that PID is still alive.
pub fn write_pid_file() -> Result<()> {
    let pid_path = paths::daemon_pid_path()?;
    if let Some(existing) = read_pid_file(&pid_path)? {
        if pid_is_alive(existing) {
            bail!(
                "another loop-daemon is already running (pid={}, file={})",
                existing,
                pid_path.display()
            );
        }
        warn!(stale_pid = existing, "overwriting stale PID file");
    }
    fs::create_dir_all(
        pid_path
            .parent()
            .ok_or_else(|| anyhow!("no parent for pid file"))?,
    )?;
    fs::write(&pid_path, std::process::id().to_string())
        .with_context(|| format!("writing PID file at {}", pid_path.display()))?;
    Ok(())
}

pub fn read_pid_file(path: &PathBuf) -> Result<Option<u32>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let pid: u32 = trimmed
        .parse()
        .with_context(|| format!("PID file at {} is malformed", path.display()))?;
    Ok(Some(pid))
}

pub fn remove_pid_file() -> Result<()> {
    let pid_path = paths::daemon_pid_path()?;
    if pid_path.exists() {
        fs::remove_file(&pid_path)
            .with_context(|| format!("removing PID file at {}", pid_path.display()))?;
    }
    Ok(())
}

/// Install signal handlers that cancel the shutdown token on
/// SIGTERM/SIGINT/SIGHUP. Returns immediately; the handlers run as
/// detached tokio tasks for the lifetime of the runtime. Multi-fire is
/// idempotent — `CancellationToken::cancel()` is safe to call repeatedly.
pub fn install_signal_handlers(shutdown: CancellationToken) -> Result<()> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sighup = signal(SignalKind::hangup())?;

    let s1 = shutdown.clone();
    let s2 = shutdown.clone();
    let s3 = shutdown;

    tokio::spawn(async move {
        sigterm.recv().await;
        info!("received SIGTERM, initiating shutdown");
        s1.cancel();
    });
    tokio::spawn(async move {
        sigint.recv().await;
        info!("received SIGINT, initiating shutdown");
        s2.cancel();
    });
    tokio::spawn(async move {
        sighup.recv().await;
        info!("received SIGHUP, initiating shutdown");
        s3.cancel();
    });

    Ok(())
}

/// Heartbeat loop — logs liveness every `interval`. Exits when the
/// shutdown token is cancelled. Sticky semantics: if the token was
/// already cancelled before entering this loop, returns immediately
/// (vs Notify which would hang).
pub async fn heartbeat_loop(interval_secs: u64, shutdown: CancellationToken) {
    let interval = Duration::from_secs(interval_secs.max(1));
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                info!(pid = std::process::id(), "heartbeat");
            }
            _ = shutdown.cancelled() => {
                info!("heartbeat loop exiting");
                return;
            }
        }
    }
}

/// Pre-detach checks: ensure no other daemon is running, log dir is writable.
pub fn pre_detach_checks() -> Result<()> {
    paths::ensure_loop_dirs()?;
    let pid_path = paths::daemon_pid_path()?;
    if let Some(existing) = read_pid_file(&pid_path)? {
        if pid_is_alive(existing) {
            bail!(
                "another loop-daemon appears to be running (pid={}). Run `loop-daemon stop` first or remove {}",
                existing,
                pid_path.display()
            );
        }
    }
    Ok(())
}

/// Run the daemon body — heartbeat loop + signal handlers.
///
/// Order matters: write the PID file BEFORE installing signal handlers,
/// so we can't end up with a tokio runtime + handlers loaded but no
/// PID file to clean up if write_pid_file fails (audit Day 10 finding).
pub async fn run_body(cfg: &DaemonConfig) -> Result<()> {
    write_pid_file()?;
    let shutdown = CancellationToken::new();
    install_signal_handlers(shutdown.clone())?;
    info!(
        pid = std::process::id(),
        heartbeat_interval_secs = cfg.heartbeat_interval_secs,
        "loop-daemon started"
    );
    heartbeat_loop(cfg.heartbeat_interval_secs, shutdown).await;
    remove_pid_file()?;
    info!("loop-daemon exited cleanly");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn read_missing_pid_file_returns_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("absent.pid");
        let result = read_pid_file(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn read_empty_pid_file_returns_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.pid");
        fs::write(&path, "").unwrap();
        let result = read_pid_file(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn read_valid_pid_file_returns_pid() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("valid.pid");
        fs::write(&path, "12345\n").unwrap();
        let result = read_pid_file(&path).unwrap().unwrap();
        assert_eq!(result, 12345);
    }

    #[test]
    fn read_malformed_pid_file_errors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.pid");
        fs::write(&path, "not-a-number").unwrap();
        let result = read_pid_file(&path);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cancellation_fires_and_wakes_waiters() {
        let shutdown = CancellationToken::new();
        let s2 = shutdown.clone();

        let waiter = tokio::spawn(async move {
            s2.cancelled().await;
            "woke up"
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.cancel();

        let result = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter timed out")
            .unwrap();
        assert_eq!(result, "woke up");
    }

    /// Regression guard for the sticky-cancellation property that
    /// CancellationToken gives us vs Notify. If a signal fires BEFORE the
    /// waiter registers, the next call to `cancelled().await` still
    /// returns immediately — no hang.
    #[tokio::test]
    async fn cancellation_is_sticky_across_fire_then_wait() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        // Now register a waiter AFTER the fire. The future should resolve
        // immediately; a hang here means we regressed back to Notify-style
        // edge-triggered semantics.
        tokio::time::timeout(Duration::from_secs(1), shutdown.cancelled())
            .await
            .expect("sticky semantics broken — waiter hung after cancel");
    }

    /// Multi-fire is idempotent — no panic, second waiter still wakes.
    #[tokio::test]
    async fn cancellation_multi_fire_is_idempotent() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        shutdown.cancel();
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), shutdown.cancelled())
            .await
            .expect("waiter hung after multi-fire");
    }

    #[tokio::test]
    async fn heartbeat_loop_exits_on_shutdown() {
        let shutdown = CancellationToken::new();
        let s2 = shutdown.clone();

        let task = tokio::spawn(async move {
            heartbeat_loop(10, s2).await;
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        shutdown.cancel();

        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("heartbeat loop didn't exit")
            .unwrap();
    }

    /// Audit C — `pre_detach_checks` refuses when a live daemon is
    /// already running (verified by writing a PID file pointing at our
    /// own process and checking that pre_detach_checks errors).
    #[test]
    fn pre_detach_checks_refuses_when_live_daemon_present() {
        let _g = paths::ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = TempDir::new().unwrap();
        let original = std::env::var(paths::LOOP_HOME_ENV).ok();
        unsafe {
            std::env::set_var(paths::LOOP_HOME_ENV, dir.path());
        }

        std::fs::create_dir_all(dir.path()).unwrap();
        let pid_path = paths::daemon_pid_path().unwrap();
        std::fs::write(&pid_path, std::process::id().to_string()).unwrap();

        let result = pre_detach_checks();

        // Restore env regardless of assertion outcome.
        match original {
            Some(v) => unsafe {
                std::env::set_var(paths::LOOP_HOME_ENV, v);
            },
            None => unsafe {
                std::env::remove_var(paths::LOOP_HOME_ENV);
            },
        }

        assert!(
            result.is_err(),
            "expected pre_detach_checks to refuse when live daemon PID present"
        );
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("another loop-daemon"),
            "expected refusal message, got: {msg}"
        );
    }

    /// Audit C — `write_pid_file` correctly overwrites a stale file
    /// (one pointing at a PID that no longer exists).
    #[test]
    fn write_pid_file_overwrites_stale() {
        let _g = paths::ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = TempDir::new().unwrap();
        let original = std::env::var(paths::LOOP_HOME_ENV).ok();
        unsafe {
            std::env::set_var(paths::LOOP_HOME_ENV, dir.path());
        }

        std::fs::create_dir_all(dir.path()).unwrap();
        let pid_path = paths::daemon_pid_path().unwrap();
        // PID u32::MAX - 1 is not realistic on Linux/macOS.
        std::fs::write(&pid_path, (u32::MAX - 1).to_string()).unwrap();

        let result = write_pid_file();

        // Read what's in the PID file now (should be our PID).
        let written = std::fs::read_to_string(&pid_path).unwrap();
        let written_pid: u32 = written.trim().parse().unwrap();

        match original {
            Some(v) => unsafe {
                std::env::set_var(paths::LOOP_HOME_ENV, v);
            },
            None => unsafe {
                std::env::remove_var(paths::LOOP_HOME_ENV);
            },
        }

        assert!(result.is_ok(), "write_pid_file errored: {:?}", result);
        assert_eq!(written_pid, std::process::id());
    }
}
