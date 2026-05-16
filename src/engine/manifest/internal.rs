//! Internal helpers for [`crate::engine::manifest::assemble`].
//!
//! Extracted from `manifest/mod.rs` per audit-fix close finding
//! B-M1 (file-size cap). Nothing here is part of the public API —
//! `pub(super)` boundary keeps callers inside the manifest module.

use bytes::Bytes;
use chrono::{DateTime, Utc};
use tracing::warn;

use crate::engine::context::Context;
use crate::engine::error::EngineError;
use crate::engine::manifest::{ActiveLesson, AssembleConfig};
use crate::engine::memory::MemoryRef;
use crate::engine::storage::{Storage, StorageKey};
use crate::engine::yaml::{
    LessonFrontmatter, LessonStatus, reader::parse_lesson_frontmatter, split_frontmatter_normalized,
};

/// Internal: per-lesson record carrying both the public-facing
/// `ActiveLesson` AND the cached frontmatter + StorageKey. Lets the
/// gate-annotation pass reuse the parsed frontmatter from the listing
/// pass (audit A-M3 fix — eliminates the per-lesson redundant
/// get+parse).
pub(super) struct LoadedRecord {
    pub(super) active: ActiveLesson,
    pub(super) fm: LessonFrontmatter,
    pub(super) key: StorageKey,
}

/// Load one lesson key into a `LoadedRecord`. Caller-side soft-fail
/// semantics: returns `Ok(None)` for missing-on-fetch (race), `Err`
/// for parse/yaml/utf8 failures.
pub(super) async fn load_one_record(
    storage: &dyn Storage,
    key: &StorageKey,
    expected_status: LessonStatus,
    body_preview_len: usize,
) -> Result<Option<LoadedRecord>, EngineError> {
    let bytes: Bytes = match storage.get(key).await? {
        Some(b) => b,
        None => return Ok(None),
    };
    let content = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(e) => {
            warn!(key = %key, error = %e, "manifest: skipping lesson with non-UTF8 bytes");
            return Err(EngineError::Parse(format!(
                "non-utf8 lesson bytes for {key}"
            )));
        }
    };
    let split = match split_frontmatter_normalized(content) {
        Ok(s) => s,
        Err(e) => {
            warn!(key = %key, error = %e, "manifest: skipping lesson with bad frontmatter split");
            return Err(EngineError::Parse(format!("split frontmatter {key}: {e}")));
        }
    };
    let fm: LessonFrontmatter = match parse_lesson_frontmatter(&split.yaml) {
        Ok(fm) => fm,
        Err(e) => {
            warn!(key = %key, error = %e, "manifest: skipping lesson with unparseable frontmatter");
            return Err(EngineError::Yaml(e.into()));
        }
    };

    let body_preview = build_body_preview(&split.body, body_preview_len);
    let last_applied_at = parse_iso_or_none(fm.last_applied_at.as_deref());
    let created_at_internal = parse_iso_or_none(Some(&fm.created_at));

    let active = ActiveLesson {
        id: fm.id.clone(),
        description: fm.description.clone(),
        status: expected_status,
        body_preview,
        applied_count: fm.applied_count,
        last_applied_at,
        target_skill: fm.target_skill.clone(),
        gate: None, // gate-annotation pass populates
        created_at_internal,
    };
    Ok(Some(LoadedRecord {
        active,
        fm,
        key: key.clone(),
    }))
}

/// Sort-key tuple for the three-key deterministic order. Aliased
/// because clippy's `type_complexity` lint dings the inline form;
/// the tuple shape is intentional (drop-in to `sort_by_key`).
pub(super) type OrderKey = (
    std::cmp::Reverse<Option<DateTime<Utc>>>,
    std::cmp::Reverse<Option<DateTime<Utc>>>,
    String,
);

/// Three-key deterministic sort key (D-C6). Sort priority:
/// 1. `last_applied_at` DESC (`None` LAST — `Reverse(Option)` puts
///    `None` at the maximum end)
/// 2. `created_at` DESC (`None` LAST, same trick)
/// 3. `id` ASC (final tiebreaker — guarantees deterministic output
///    across runs for git-diff-friendly CLI rendering)
pub(super) fn order_key(l: &ActiveLesson) -> OrderKey {
    (
        std::cmp::Reverse(l.last_applied_at),
        std::cmp::Reverse(l.created_at_internal),
        l.id.clone(),
    )
}

/// Build the body preview per OQ-C2: char-based slice (multi-byte
/// UTF-8 safe), trimmed of leading/trailing whitespace.
pub(super) fn build_body_preview(body: &str, n: usize) -> String {
    body.chars().take(n).collect::<String>().trim().to_string()
}

/// Parse an ISO-8601 / RFC-3339 string into `DateTime<Utc>`. Returns
/// `None` on parse failure (the caller increments the appropriate
/// skip counter rather than hard-failing).
pub(super) fn parse_iso_or_none(s: Option<&str>) -> Option<DateTime<Utc>> {
    s.and_then(|s| s.parse::<DateTime<Utc>>().ok())
}

/// Reject configurations whose `statuses` vec is empty — that's a
/// caller bug, not a "manifest has zero lessons" condition.
pub(super) fn validate_config(config: &AssembleConfig) -> Result<(), EngineError> {
    if config.statuses.is_empty() {
        return Err(EngineError::ManifestInvalidStatus {
            status: "<empty statuses vec>".to_string(),
        });
    }
    Ok(())
}

/// Phase F audit-fix close: filter a vector of `MemoryRef` down to
/// those whose underlying `MemoryFrontmatter::scope` satisfies the
/// filter. Loads each candidate's frontmatter via storage; soft-
/// fails on missing keys (the search result must have raced with a
/// concurrent delete — drop the ref).
pub(super) async fn filter_refs_by_scope(
    ctx: &Context,
    storage: &dyn Storage,
    refs: Vec<MemoryRef>,
    filter: &crate::engine::memory::MemoryScopeFilter,
) -> Vec<MemoryRef> {
    let mut out = Vec::with_capacity(refs.len());
    for r in refs {
        // Missing or load-failure → drop silently (race window
        // against a concurrent delete; not an error).
        if let Ok(Some(mem)) = crate::engine::memory::get_by_id(ctx, storage, &r.id).await
            && filter.matches(&mem.frontmatter.scope)
        {
            out.push(r);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_body_preview_handles_multibyte_utf8() {
        // 5 Japanese chars (each 3 bytes) — char-based slice must take
        // exactly 3 chars; byte-based slice would panic on a non-char
        // boundary (the smoke test for S120).
        let body = "あいうえお rest of body";
        let preview = build_body_preview(body, 3);
        assert_eq!(preview, "あいう");
    }

    #[test]
    fn build_body_preview_trims_whitespace() {
        let body = "   hello world   ";
        let preview = build_body_preview(body, 50);
        assert_eq!(preview, "hello world");
    }

    #[test]
    fn build_body_preview_caps_at_n_chars() {
        let body = "abcdefghij";
        let preview = build_body_preview(body, 4);
        assert_eq!(preview, "abcd");
    }

    #[test]
    fn parse_iso_or_none_handles_valid_and_invalid() {
        assert!(parse_iso_or_none(Some("2026-05-13T00:00:00Z")).is_some());
        assert!(parse_iso_or_none(Some("garbage")).is_none());
        assert!(parse_iso_or_none(None).is_none());
    }
}
