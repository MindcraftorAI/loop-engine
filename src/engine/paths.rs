//! Path resolution: ~/.loop, ~/.claude, daemon-specific paths.
//!
//! Single source of truth so the rest of the daemon doesn't construct
//! paths ad-hoc. Mirrors the TS-side `paths.ts` semantics: LOOP_HOME env
//! override takes precedence; falls back to `~/.loop`.

use std::env;
use std::path::PathBuf;

use anyhow::{anyhow, Result};

pub const LOOP_HOME_ENV: &str = "LOOP_HOME";

/// Shared mutex for tests that mutate `LOOP_HOME` (or any env var). All
/// such tests across the crate join this mutex so cargo's parallel
/// runner doesn't race the env state.
///
/// Audit Day 14 m3: `pub(crate)` not `pub` — the invariant is "test
/// modules inside this crate join the same lock" and external test
/// crates have no business reaching into it.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Resolve LOOP_HOME — env var if set and non-empty, else `~/.loop`.
pub fn loop_home() -> Result<PathBuf> {
    if let Ok(value) = env::var(LOOP_HOME_ENV) {
        if !value.is_empty() {
            return Ok(PathBuf::from(value));
        }
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))?;
    Ok(home.join(".loop"))
}

/// `~/.loop/logs/`
pub fn logs_dir() -> Result<PathBuf> {
    Ok(loop_home()?.join("logs"))
}

/// `~/.loop/logs/daemon.log` — primary daemon log file.
pub fn daemon_log_path() -> Result<PathBuf> {
    Ok(logs_dir()?.join("daemon.log"))
}

/// `~/.loop/daemon.pid` — PID file. The daemon writes this on start
/// and reads it during `status` / `stop`.
pub fn daemon_pid_path() -> Result<PathBuf> {
    Ok(loop_home()?.join("daemon.pid"))
}

/// `~/.loop/config.yaml` — shared with the TS MCP server.
pub fn config_path() -> Result<PathBuf> {
    Ok(loop_home()?.join("config.yaml"))
}

/// `~/.loop/lessons/` — lesson layer root (shared with TS).
pub fn lessons_dir() -> Result<PathBuf> {
    Ok(loop_home()?.join("lessons"))
}

/// Status-specific subdirectories per ADR-0010 ("status-as-directory").
/// Directory is the authoritative source of truth for a lesson's status;
/// frontmatter `status` is portability metadata.
pub fn lessons_status_dir(status: &str) -> Result<PathBuf> {
    Ok(lessons_dir()?.join(status))
}

/// All 5 lesson status directory names in canonical order.
pub const LESSON_STATUS_DIRS: &[&str] =
    &["pending", "active", "promoted", "discarded", "superseded"];

/// `~/.claude/projects/` — Claude Code project transcript root.
pub fn claude_projects_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))?;
    Ok(home.join(".claude").join("projects"))
}

/// Ensure all daemon-owned directories exist. Idempotent.
pub fn ensure_loop_dirs() -> Result<()> {
    std::fs::create_dir_all(loop_home()?)?;
    std::fs::create_dir_all(logs_dir()?)?;
    std::fs::create_dir_all(lessons_dir()?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ENV_LOCK;
    use super::*;

    #[test]
    fn loop_home_honors_env() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let original = env::var(LOOP_HOME_ENV).ok();
        // SAFETY: lock held; tests within the same process don't race.
        unsafe {
            env::set_var(LOOP_HOME_ENV, "/tmp/custom-loop");
        }
        let resolved = loop_home().unwrap();
        assert_eq!(resolved, PathBuf::from("/tmp/custom-loop"));
        match original {
            Some(v) => unsafe {
                env::set_var(LOOP_HOME_ENV, v);
            },
            None => unsafe {
                env::remove_var(LOOP_HOME_ENV);
            },
        }
    }

    #[test]
    fn loop_home_falls_back_to_home_dot_loop() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let original = env::var(LOOP_HOME_ENV).ok();
        unsafe {
            env::remove_var(LOOP_HOME_ENV);
        }
        let resolved = loop_home().unwrap();
        assert!(resolved.to_string_lossy().ends_with("/.loop"));
        if let Some(v) = original {
            unsafe {
                env::set_var(LOOP_HOME_ENV, v);
            }
        }
    }
}
