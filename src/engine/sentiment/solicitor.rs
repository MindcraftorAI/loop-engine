//! Solicitor — detects stale lessons and surfaces them for review.
//!
//! **Pure async function**, not a Service / Task / Stream consumer. Per
//! `docs/research/day-17-pre-research.md` Q1 + learn-notes D1: the
//! engine does not own its executor; the host (daemon, cron, CLI) calls
//! `solicit_stale_lessons` on whatever cadence policy it chooses.
//!
//! Staleness algorithm (D2):
//! - Lesson AGE comes from `frontmatter.created_at` (NOT filesystem
//!   birthtime — flatter port from TS, and the value is durable across
//!   filesystem moves).
//! - Signal-density proxy is `frontmatter.external_signal_sources.len()`
//!   — counts external sources of evidence that influenced the lesson.
//! - A lesson is STALE when:
//!     - Age >= `min_age_days` (default 7), AND
//!     - `external_signal_sources.len() < min_signals_threshold` (default 1)
//! - Output is bounded by `max_candidates_per_call` (default 1) — one
//!   solicitation per invocation keeps user load low.

use chrono::{DateTime, Utc};

use crate::engine::context::Context;
use crate::engine::error::EngineError;
use crate::engine::storage::{Storage, StorageKey};
use crate::engine::yaml::{reader::parse_lesson_frontmatter, split_frontmatter_normalized};

/// Tunables for `solicit_stale_lessons`. `#[non_exhaustive]` — Day 18+
/// may add density-window or per-status thresholds.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SolicitorConfig {
    /// Minimum lesson age before it can be considered stale (default 7 days).
    pub min_age_days: u64,
    /// A lesson with fewer than this many `external_signal_sources`
    /// counts as low-density (default 1).
    pub min_signals_threshold: usize,
    /// Maximum stale candidates surfaced per invocation (default 1).
    pub max_candidates_per_call: usize,
    /// Status directory to scan (default `"active"`).
    pub scan_status: String,
}

impl Default for SolicitorConfig {
    fn default() -> Self {
        Self {
            min_age_days: 7,
            min_signals_threshold: 1,
            max_candidates_per_call: 1,
            scan_status: "active".to_string(),
        }
    }
}

/// Why a lesson is being surfaced as stale. `#[non_exhaustive]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StaleReason {
    /// Lesson has no external signal sources at all and is older than threshold.
    NoSignalsInWindow,
    /// Lesson has SOME signals but below the density threshold.
    BelowDensityThreshold,
}

/// One stale-lesson candidate.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StaleCandidate {
    pub lesson_id: String,
    pub created_at: DateTime<Utc>,
    pub age_days: u64,
    pub signal_count: usize,
    pub reason: StaleReason,
}

/// Output of `solicit_stale_lessons`. `#[non_exhaustive]`.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct SolicitorOutput {
    pub stale_candidates: Vec<StaleCandidate>,
    pub scanned_count: usize,
    pub skipped_count: usize,
}

/// Scan the lessons directory for stale candidates. Pure async function
/// — no state, no spawned tasks, no executor ownership.
///
/// The host calls this on whatever cadence is appropriate (daily cron,
/// on-demand CLI, in-process timer). The output is purely informational;
/// the caller decides what to do with each candidate (prompt user,
/// archive automatically, queue for review).
pub async fn solicit_stale_lessons(
    ctx: &Context,
    storage: &dyn Storage,
    config: &SolicitorConfig,
    now: DateTime<Utc>,
) -> Result<SolicitorOutput, EngineError> {
    let prefix = StorageKey::lesson_status_prefix(ctx, &config.scan_status);
    let keys = storage.list(&prefix).await?;

    let mut output = SolicitorOutput::default();

    // Day 17 audit M1: respect `max_candidates_per_call = 0` (boundary).
    if config.max_candidates_per_call == 0 {
        // Caller wants no candidates — short-circuit. Still count what
        // we'd have scanned (none) and skipped (none).
        return Ok(output);
    }

    for key in keys {
        output.scanned_count += 1;

        // Skip non-`.md` entries (the prefix is permissive).
        if !key.as_str().ends_with(".md") {
            output.skipped_count += 1;
            continue;
        }

        let Some(bytes) = storage.get(&key).await? else {
            // Race: file existed at list time, gone at get time. Skip.
            output.skipped_count += 1;
            continue;
        };

        let content = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(_) => {
                output.skipped_count += 1;
                continue;
            }
        };

        let split = match split_frontmatter_normalized(content) {
            Ok(s) => s,
            Err(_) => {
                output.skipped_count += 1;
                continue;
            }
        };

        let frontmatter = match parse_lesson_frontmatter(&split.yaml) {
            Ok(fm) => fm,
            Err(_) => {
                output.skipped_count += 1;
                continue;
            }
        };

        let created_at: DateTime<Utc> = match frontmatter.created_at.parse() {
            Ok(t) => t,
            Err(_) => {
                output.skipped_count += 1;
                continue;
            }
        };

        let age = now.signed_duration_since(created_at);
        let age_days = age.num_days().max(0) as u64;
        if age_days < config.min_age_days {
            continue;
        }

        let signal_count = frontmatter.external_signal_sources.len();
        if signal_count >= config.min_signals_threshold {
            continue;
        }

        let reason = if signal_count == 0 {
            StaleReason::NoSignalsInWindow
        } else {
            StaleReason::BelowDensityThreshold
        };

        output.stale_candidates.push(StaleCandidate {
            lesson_id: frontmatter.id,
            created_at,
            age_days,
            signal_count,
            reason,
        });

        if output.stale_candidates.len() >= config.max_candidates_per_call {
            break;
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::storage::MemoryStorage;
    use bytes::Bytes;

    fn lesson_yaml(id: &str, created_at: &str, signals: &[&str]) -> String {
        let signals_yaml = if signals.is_empty() {
            "[]".to_string()
        } else {
            let inner = signals
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{inner}]")
        };
        format!(
            "---\n\
             id: {id}\n\
             description: \"placeholder description\"\n\
             status: active\n\
             created_at: \"{created_at}\"\n\
             applied_count: 0\n\
             thumbs_up_count: 0\n\
             thumbs_down_count: 0\n\
             external_signal_sources: {signals_yaml}\n\
             ---\n\
             body content\n"
        )
    }

    async fn seed(storage: &MemoryStorage, ctx: &Context, id: &str, content: &str) {
        let key = StorageKey::lesson(ctx, "active", id);
        storage
            .put(&key, Bytes::from(content.to_string()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn no_lessons_no_candidates() {
        let storage = MemoryStorage::default();
        let ctx = Context::single_user_local();
        let out = solicit_stale_lessons(
            &ctx,
            &storage,
            &SolicitorConfig::default(),
            Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(out.scanned_count, 0);
        assert!(out.stale_candidates.is_empty());
    }

    #[tokio::test]
    async fn fresh_lesson_is_not_stale() {
        let storage = MemoryStorage::default();
        let ctx = Context::single_user_local();
        let now = Utc::now();
        // Created 3 days ago — under the 7-day threshold.
        let created = (now - chrono::Duration::days(3)).to_rfc3339();
        seed(&storage, &ctx, "les-fresh", &lesson_yaml("les-fresh", &created, &[])).await;
        let out =
            solicit_stale_lessons(&ctx, &storage, &SolicitorConfig::default(), now)
                .await
                .unwrap();
        assert_eq!(out.scanned_count, 1);
        assert!(out.stale_candidates.is_empty());
    }

    #[tokio::test]
    async fn old_lesson_with_no_signals_is_stale_no_signals_in_window() {
        let storage = MemoryStorage::default();
        let ctx = Context::single_user_local();
        let now = Utc::now();
        let created = (now - chrono::Duration::days(30)).to_rfc3339();
        seed(&storage, &ctx, "les-stale", &lesson_yaml("les-stale", &created, &[])).await;
        let out =
            solicit_stale_lessons(&ctx, &storage, &SolicitorConfig::default(), now)
                .await
                .unwrap();
        assert_eq!(out.stale_candidates.len(), 1);
        assert_eq!(out.stale_candidates[0].lesson_id, "les-stale");
        assert_eq!(out.stale_candidates[0].reason, StaleReason::NoSignalsInWindow);
        assert!(out.stale_candidates[0].age_days >= 30);
    }

    #[tokio::test]
    async fn old_lesson_with_some_signals_below_threshold_is_stale() {
        let storage = MemoryStorage::default();
        let ctx = Context::single_user_local();
        let now = Utc::now();
        let created = (now - chrono::Duration::days(30)).to_rfc3339();
        seed(
            &storage,
            &ctx,
            "les-thin",
            &lesson_yaml("les-thin", &created, &["external-source-1"]),
        )
        .await;
        // Default threshold is 1 signal; this lesson has exactly 1 → not stale.
        let out =
            solicit_stale_lessons(&ctx, &storage, &SolicitorConfig::default(), now)
                .await
                .unwrap();
        assert!(out.stale_candidates.is_empty());

        // Raise threshold to 2 — now the lesson is below-density-stale.
        let config = SolicitorConfig {
            min_signals_threshold: 2,
            ..SolicitorConfig::default()
        };
        let out =
            solicit_stale_lessons(&ctx, &storage, &config, now).await.unwrap();
        assert_eq!(out.stale_candidates.len(), 1);
        assert_eq!(
            out.stale_candidates[0].reason,
            StaleReason::BelowDensityThreshold
        );
    }

    #[tokio::test]
    async fn max_candidates_per_call_bounds_output() {
        let storage = MemoryStorage::default();
        let ctx = Context::single_user_local();
        let now = Utc::now();
        let created = (now - chrono::Duration::days(60)).to_rfc3339();
        for i in 0..5 {
            let id = format!("les-many-{i}");
            seed(&storage, &ctx, &id, &lesson_yaml(&id, &created, &[])).await;
        }
        let config = SolicitorConfig {
            max_candidates_per_call: 3,
            ..SolicitorConfig::default()
        };
        let out =
            solicit_stale_lessons(&ctx, &storage, &config, now).await.unwrap();
        assert_eq!(out.stale_candidates.len(), 3);
        assert!(out.scanned_count >= 3);
    }

    /// Day 17 audit M1 regression: `max_candidates_per_call = 0` returns
    /// zero candidates, not one.
    #[tokio::test]
    async fn max_candidates_zero_returns_no_candidates() {
        let storage = MemoryStorage::default();
        let ctx = Context::single_user_local();
        let now = Utc::now();
        let created = (now - chrono::Duration::days(60)).to_rfc3339();
        seed(
            &storage,
            &ctx,
            "les-would-be-stale",
            &lesson_yaml("les-would-be-stale", &created, &[]),
        )
        .await;
        let config = SolicitorConfig {
            max_candidates_per_call: 0,
            ..SolicitorConfig::default()
        };
        let out = solicit_stale_lessons(&ctx, &storage, &config, now)
            .await
            .unwrap();
        assert!(out.stale_candidates.is_empty(), "expected zero candidates");
    }

    #[tokio::test]
    async fn malformed_lesson_increments_skipped() {
        let storage = MemoryStorage::default();
        let ctx = Context::single_user_local();
        let key = StorageKey::lesson(&ctx, "active", "les-bad");
        // Missing the YAML frontmatter — split should fail.
        storage
            .put(&key, Bytes::from_static(b"just body, no frontmatter\n"))
            .await
            .unwrap();
        let out =
            solicit_stale_lessons(&ctx, &storage, &SolicitorConfig::default(), Utc::now())
                .await
                .unwrap();
        assert_eq!(out.scanned_count, 1);
        assert_eq!(out.skipped_count, 1);
        assert!(out.stale_candidates.is_empty());
    }
}
