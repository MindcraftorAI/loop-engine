// Layered configuration loader.
//
// Pattern adapted (not copied) from ecc2/src/config/mod.rs lines 493-607
// — MIT License, Copyright (c) 2026 Affaan Mustafa.
// Adapted to: (a) YAML instead of TOML, (b) Loop's narrow daemon config
// schema, (c) ~/.loop/config.yaml location shared with the TS MCP server.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::engine::paths;

const DEFAULT_HEARTBEAT_INTERVAL_SECS: u64 = 30;
const DEFAULT_LOG_LEVEL: &str = "info";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Interval between heartbeat log lines while idle. Seconds.
    pub heartbeat_interval_secs: u64,
    /// `RUST_LOG`-style filter applied when env not set.
    pub log_level: String,
    /// Sentiment classifier API key (Anthropic). Optional — daemon
    /// runs without it but emits no signals (shadow mode).
    pub anthropic_api_key: Option<String>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval_secs: DEFAULT_HEARTBEAT_INTERVAL_SECS,
            log_level: DEFAULT_LOG_LEVEL.to_string(),
            anthropic_api_key: None,
        }
    }
}

/// Top-level config matching what's on disk at `~/.loop/config.yaml`.
/// Only the `daemon:` block is owned by us; the rest of the file is
/// owned by the TS MCP server and we round-trip it untouched in v2.
/// For Day 10 we only read our block; we never write the file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LoopConfig {
    pub daemon: DaemonConfig,
}

/// Load config using layered merge: default → file-on-disk.
///
/// Layer 2 (per-project override) is reserved for Phase B; current
/// daemon scope is one config per host.
pub fn load() -> Result<DaemonConfig> {
    let path = paths::config_path()?;
    let cfg = load_from_path(&path)?;
    Ok(cfg.daemon)
}

pub fn load_from_path(path: &PathBuf) -> Result<LoopConfig> {
    if !path.exists() {
        // Missing config is not an error — daemon uses defaults.
        return Ok(LoopConfig::default());
    }
    let contents = fs::read_to_string(path)
        .with_context(|| format!("reading config at {}", path.display()))?;
    if contents.trim().is_empty() {
        return Ok(LoopConfig::default());
    }
    // serde_yml ignores fields we don't declare, so the TS-owned blocks
    // are preserved on disk (we never write back at this stage).
    let cfg: LoopConfig = serde_yml::from_str(&contents)
        .with_context(|| format!("parsing config at {}", path.display()))?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn missing_file_uses_defaults() {
        let path = PathBuf::from("/nonexistent/path/that/does/not/exist.yaml");
        let cfg = load_from_path(&path).unwrap();
        assert_eq!(cfg.daemon.heartbeat_interval_secs, 30);
        assert_eq!(cfg.daemon.log_level, "info");
        assert!(cfg.daemon.anthropic_api_key.is_none());
    }

    #[test]
    fn empty_file_uses_defaults() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"").unwrap();
        let cfg = load_from_path(&tmp.path().to_path_buf()).unwrap();
        assert_eq!(cfg.daemon.heartbeat_interval_secs, 30);
    }

    #[test]
    fn overrides_apply_via_daemon_block() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(
            b"daemon:\n  heartbeat_interval_secs: 60\n  log_level: debug\n  anthropic_api_key: sk-test\n",
        )
        .unwrap();
        let cfg = load_from_path(&tmp.path().to_path_buf()).unwrap();
        assert_eq!(cfg.daemon.heartbeat_interval_secs, 60);
        assert_eq!(cfg.daemon.log_level, "debug");
        assert_eq!(cfg.daemon.anthropic_api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn unrelated_top_level_blocks_are_ignored() {
        // The TS MCP server writes other top-level blocks (memory, lessons,
        // embeddings, llm). Daemon must tolerate them without error.
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(
            b"memory:\n  scoring:\n    similarity_weight: 0.5\nlessons:\n  promotion:\n    min_age_hours: 24\ndaemon:\n  heartbeat_interval_secs: 15\n",
        )
        .unwrap();
        let cfg = load_from_path(&tmp.path().to_path_buf()).unwrap();
        assert_eq!(cfg.daemon.heartbeat_interval_secs, 15);
    }
}
