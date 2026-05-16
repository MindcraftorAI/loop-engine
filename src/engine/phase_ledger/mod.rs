//! Per-session workflow phase ledger.
//!
//! Records which workflow phases (pre_research, learn, code, test,
//! audit, post_research, fix) have been logged for a given
//! `(session_id, task_id)` pair. The engine is opaque to what "task"
//! means — it's just a stable string discriminator supplied by the
//! caller. The engine speaks ledger, not task semantics.
//!
//! ### Storage layout
//!
//! One file per phase entry. Re-logging the same phase is a free
//! no-op via `Storage::put_if_version(..., None)` create-only
//! semantics. Mirrors the per-session signal store at
//! `engine/src/engine/sentiment/signals.rs` — no append-only JSONL,
//! no flock, no race on shared mutable files.
//!
//! On disk (single-user mode):
//! ```text
//! ~/.loop/phase_ledger/<session_id>/<task_id>/<phase>.yaml
//! ```
//!
//! Each entry is a YAML object:
//! ```yaml
//! phase: audit
//! logged_at: 2026-05-16T07:42:11.000Z
//! note: "13 retroactive findings, 5 HIGH fixed in same cycle"
//! ```
//!
//! ### Input safety
//!
//! `StorageKey::from_raw` panics on `..`, leading `/`, or `\`. Callers
//! supply `session_id` and `task_id` from outside the engine, so the
//! ledger functions validate them against `[A-Za-z0-9_-]{1,128}` BEFORE
//! constructing the key. Rejection is an error to the RPC caller, not
//! a daemon crash.

use bytes::Bytes;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::engine::context::Context;
use crate::engine::storage::Storage;
use crate::engine::storage::StorageKey;
use crate::engine::storage::error::StorageError;

/// The seven workflow phases the gate cares about.
///
/// Wire format is snake_case (`pre_research`, not `PreResearch`) — the
/// agent calls `log_phase` with these exact strings, and the per-file
/// path uses the same form for parity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    PreResearch,
    Learn,
    Code,
    Test,
    Audit,
    PostResearch,
    Fix,
}

impl Phase {
    /// Snake-case string form (matches the serde wire format + on-disk
    /// filename without the `.yaml` extension).
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::PreResearch => "pre_research",
            Phase::Learn => "learn",
            Phase::Code => "code",
            Phase::Test => "test",
            Phase::Audit => "audit",
            Phase::PostResearch => "post_research",
            Phase::Fix => "fix",
        }
    }

    /// Inverse of [`Self::as_str`]. Used by the loader when re-reading
    /// entries from disk. Named `parse` instead of `from_str` so we
    /// don't shadow `std::str::FromStr` (clippy::should_implement_trait).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pre_research" => Some(Phase::PreResearch),
            "learn" => Some(Phase::Learn),
            "code" => Some(Phase::Code),
            "test" => Some(Phase::Test),
            "audit" => Some(Phase::Audit),
            "post_research" => Some(Phase::PostResearch),
            "fix" => Some(Phase::Fix),
            _ => None,
        }
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One ledger entry as it exists on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub phase: Phase,
    /// RFC3339 timestamp at log time (milliseconds, UTC).
    pub logged_at: String,
    /// Optional free-text note the agent passed alongside the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Error)]
pub enum LedgerError {
    #[error("invalid id: {field} must match [A-Za-z0-9_-]{{1,128}}, got {value:?}")]
    InvalidId { field: &'static str, value: String },
    #[error("note too long: {len} bytes (max {max})")]
    NoteTooLong { len: usize, max: usize },
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("malformed entry on disk at {key}: {detail}")]
    MalformedEntry { key: String, detail: String },
}

/// Validate that a caller-supplied id is path-safe. Rejects empty,
/// over-long, or chars outside `[A-Za-z0-9_-]`. Defense-in-depth so
/// the `StorageKey::from_raw` hard-assert never fires from valid RPC.
fn validate_id(field: &'static str, value: &str) -> Result<(), LedgerError> {
    if value.is_empty() || value.len() > 128 {
        return Err(LedgerError::InvalidId {
            field,
            value: value.to_string(),
        });
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(LedgerError::InvalidId {
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

/// Note length cap. Anything longer is rejected with InvalidId-style
/// error to prevent unbounded growth via repeated logs (each entry is
/// a separate file, but each could grow large without this cap). 16 KB
/// is generous — typical notes are 1-2 sentences ("13 audit findings
/// fixed in same cycle").
const NOTE_MAX_BYTES: usize = 16 * 1024;

/// Record a phase entry. Returns `Ok(true)` on new write, `Ok(false)`
/// when this `(session, task, phase)` was already logged (idempotent
/// noop — the existing entry stands, not overwritten).
pub async fn log_phase(
    ctx: &Context,
    storage: &dyn Storage,
    session_id: &str,
    task_id: &str,
    phase: Phase,
    note: Option<&str>,
) -> Result<bool, LedgerError> {
    validate_id("session_id", session_id)?;
    validate_id("task_id", task_id)?;
    if let Some(n) = note
        && n.len() > NOTE_MAX_BYTES
    {
        return Err(LedgerError::NoteTooLong {
            len: n.len(),
            max: NOTE_MAX_BYTES,
        });
    }
    let entry = LedgerEntry {
        phase,
        logged_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        note: note.map(|s| s.to_string()),
    };
    let body = render_entry_yaml(&entry);
    let key = StorageKey::phase_log(ctx, session_id, task_id, phase.as_str());
    // Create-only: first write wins, re-logs are noop.
    let written = storage
        .put_if_version(&key, Bytes::from(body), None)
        .await?;
    Ok(written)
}

/// List all phases logged for `(session, task)`. Returns entries in
/// the order the storage backend lists them — callers requiring a
/// deterministic order should sort.
pub async fn get_ledger(
    ctx: &Context,
    storage: &dyn Storage,
    session_id: &str,
    task_id: &str,
) -> Result<Vec<LedgerEntry>, LedgerError> {
    validate_id("session_id", session_id)?;
    validate_id("task_id", task_id)?;
    let prefix = StorageKey::phase_ledger_task_prefix(ctx, session_id, task_id);
    let keys = storage.list(&prefix).await?;
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let bytes = storage.get(&key).await?;
        let Some(bytes) = bytes else { continue };
        let entry = parse_entry_yaml(&bytes, key.as_str())?;
        out.push(entry);
    }
    // Deterministic chronological order regardless of backend (Memory
    // → BTreeMap alpha; LocalFs → OS-dependent read_dir). Cheap sort
    // (≤7 entries per (session, task)).
    out.sort_by(|a, b| a.logged_at.cmp(&b.logged_at));
    Ok(out)
}

// ---------------------------------------------------------------------
// YAML render / parse — small enough that pulling in serde_yml would
// be overkill. Three fields, schema-stable, all simple scalars.
// ---------------------------------------------------------------------

fn render_entry_yaml(entry: &LedgerEntry) -> String {
    let mut out = String::with_capacity(128);
    out.push_str("phase: ");
    out.push_str(entry.phase.as_str());
    out.push('\n');
    out.push_str("logged_at: ");
    out.push_str(&entry.logged_at);
    out.push('\n');
    if let Some(note) = &entry.note {
        out.push_str("note: ");
        // Quote + escape so any character in the note survives YAML.
        out.push('"');
        for ch in note.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c => out.push(c),
            }
        }
        out.push('"');
        out.push('\n');
    }
    out
}

fn parse_entry_yaml(bytes: &[u8], key_hint: &str) -> Result<LedgerEntry, LedgerError> {
    let text = std::str::from_utf8(bytes).map_err(|e| LedgerError::MalformedEntry {
        key: key_hint.to_string(),
        detail: format!("not utf-8: {e}"),
    })?;
    let mut phase: Option<Phase> = None;
    let mut logged_at: Option<String> = None;
    let mut note: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("phase: ") {
            phase = Phase::parse(rest.trim());
        } else if let Some(rest) = line.strip_prefix("logged_at: ") {
            logged_at = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("note: ") {
            note = Some(unquote_note(rest.trim()));
        }
    }
    Ok(LedgerEntry {
        phase: phase.ok_or_else(|| LedgerError::MalformedEntry {
            key: key_hint.to_string(),
            detail: "missing or unknown `phase`".into(),
        })?,
        logged_at: logged_at.ok_or_else(|| LedgerError::MalformedEntry {
            key: key_hint.to_string(),
            detail: "missing `logged_at`".into(),
        })?,
        note,
    })
}

/// Unescape the YAML-quoted note string produced by [`render_entry_yaml`].
/// Strips matching surrounding `"` if present, then handles the `\n`,
/// `\r`, `\t`, `\\`, `\"` escapes we emit. Unknown escapes pass through
/// untouched — better to surface garbage than swallow it.
fn unquote_note(s: &str) -> String {
    let inner = if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        &s[1..s.len() - 1]
    } else {
        s
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_roundtrip() {
        for p in [
            Phase::PreResearch,
            Phase::Learn,
            Phase::Code,
            Phase::Test,
            Phase::Audit,
            Phase::PostResearch,
            Phase::Fix,
        ] {
            assert_eq!(Phase::parse(p.as_str()), Some(p));
        }
    }

    #[test]
    fn validate_id_accepts_safe() {
        assert!(validate_id("task_id", "task-127").is_ok());
        assert!(validate_id("task_id", "session_abc_123").is_ok());
        assert!(validate_id("task_id", "A").is_ok());
    }

    #[test]
    fn validate_id_rejects_traversal() {
        assert!(validate_id("task_id", "../etc/passwd").is_err());
        assert!(validate_id("task_id", "task/../").is_err());
        assert!(validate_id("task_id", "task\\sneaky").is_err());
    }

    #[test]
    fn validate_id_rejects_empty_and_over_long() {
        assert!(validate_id("task_id", "").is_err());
        let long = "a".repeat(129);
        assert!(validate_id("task_id", &long).is_err());
    }

    #[test]
    fn render_entry_minimal() {
        let entry = LedgerEntry {
            phase: Phase::Audit,
            logged_at: "2026-05-16T07:42:11.000Z".to_string(),
            note: None,
        };
        let rendered = render_entry_yaml(&entry);
        assert_eq!(
            rendered,
            "phase: audit\nlogged_at: 2026-05-16T07:42:11.000Z\n"
        );
    }

    #[test]
    fn render_entry_with_note_escapes() {
        let entry = LedgerEntry {
            phase: Phase::Fix,
            logged_at: "2026-05-16T07:42:11.000Z".to_string(),
            note: Some("fixed \"H1\" + line 2\nbug".into()),
        };
        let rendered = render_entry_yaml(&entry);
        // Quotes + newline are escaped.
        assert!(rendered.contains(r#"note: "fixed \"H1\" + line 2\nbug""#));
    }

    #[test]
    fn parse_entry_roundtrip() {
        let original = LedgerEntry {
            phase: Phase::PostResearch,
            logged_at: "2026-05-16T07:42:11.000Z".to_string(),
            note: Some("audit findings synthesized\nfollow-up issue filed".into()),
        };
        let rendered = render_entry_yaml(&original);
        let parsed = parse_entry_yaml(rendered.as_bytes(), "test_key").unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn parse_entry_missing_phase_errors() {
        let bad = b"logged_at: 2026-05-16T07:42:11.000Z\n";
        assert!(parse_entry_yaml(bad, "k").is_err());
    }

    #[test]
    fn parse_entry_unknown_phase_errors() {
        let bad = b"phase: bogus_phase\nlogged_at: 2026-05-16T07:42:11.000Z\n";
        assert!(parse_entry_yaml(bad, "k").is_err());
    }
}
