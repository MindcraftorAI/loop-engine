//! Manifest assembly — the structured context bundle the engine surfaces
//! to host LLMs at session start (or on demand).
//!
//! Phase C ships ONE section: `active_lessons` (TS-parity trimmed view +
//! per-lesson wedge gate annotation). Future phases add memories (E),
//! skills/personas/teams (F) as ADDITIVE fields — `Manifest` is
//! `#[non_exhaustive]` so growth is non-breaking.
//!
//! The wedge surfaces here: every active lesson in the manifest carries
//! `gate: Option<GateDecision>` (when `AssembleConfig::annotate_with_gate
//! = true`, default). A backdated lesson shows up in the manifest with
//! `gate: Some(Block { reasons: [TamperedAge, ...] })`, so the LLM
//! consuming the manifest can see the promotion-readiness verdict in the
//! same payload as the lesson body — no separate gate-check round trip.
//!
//! Engine boundary: this module returns `Manifest` only. **No
//! `serde::Serialize`** — adapter crates (the future monolith MCP
//! server) own the wire shape via `From<&Manifest>`. The data type is
//! engine-stable; the wire shape is adapter-stable.

use chrono::{DateTime, Utc};

use crate::engine::context::Context;
use crate::engine::error::EngineError;
use crate::engine::lessons::{GateDecision, PromotionConfig};
use crate::engine::storage::Storage;
use crate::engine::yaml::LessonStatus;

/// The structured context bundle surfaced to host LLMs. Phase C ships
/// `active_lessons` + `assembly_stats`; Phase E adds memories, Phase F
/// adds skills/personas/teams as additive fields.
///
/// `#[non_exhaustive]` so the engine can grow new sections without a
/// SemVer break. External callers should pattern-match with wildcards
/// or use field-access (which IS forward-compatible).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Manifest {
    /// Active lessons in deterministic order (per [`AssembleConfig`]).
    pub active_lessons: Vec<ActiveLesson>,
    /// Diagnostics + summary stats for the assembly pass that produced
    /// this manifest — useful for CLI rendering, debugging, and caller
    /// observability.
    pub assembly_stats: AssemblyStats,
}

/// One lesson in the manifest's `active_lessons` list. TS-parity-trimmed
/// (we do NOT expose the full [`crate::engine::yaml::LessonFrontmatter`]
/// — every counter would become a SemVer hinge) PLUS the engine-side
/// wedge addition `gate`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ActiveLesson {
    pub id: String,
    pub description: String,
    pub status: LessonStatus,
    /// First N chars of the lesson body (post-frontmatter), trimmed.
    /// `N` defaults to `AssembleConfig::body_preview_len = 200`.
    pub body_preview: String,
    pub applied_count: u64,
    /// Last time the lesson surfaced in an assembled manifest. `None`
    /// if never applied OR if the underlying `last_applied_at` YAML
    /// string was malformed (in which case `AssemblyStats::skipped_*`
    /// counters are incremented).
    pub last_applied_at: Option<DateTime<Utc>>,
    pub target_skill: Option<String>,
    /// THE wedge: promotion-gate decision for this lesson. `None` when
    /// `AssembleConfig::annotate_with_gate = false` OR when the storage
    /// backend failed to return metadata (counted in
    /// `AssemblyStats::gate_skip_count`).
    pub gate: Option<GateDecision>,
}

/// Configuration knobs for [`assemble`]. Defaults match the TS
/// reference; `Default` builds the production-ready config.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AssembleConfig {
    /// Which lesson statuses to include. Default `[LessonStatus::Active]`
    /// (TS parity). Set to `[Active, Promoted]` for "everything trusted"
    /// or `[Pending]` for review workflows.
    pub statuses: Vec<LessonStatus>,
    /// Maximum number of lessons to return after ordering. Default 5.
    pub lesson_limit: usize,
    /// Body preview character count (chars, not bytes — multi-byte
    /// UTF-8 safe). Default 200 (TS parity).
    pub body_preview_len: usize,
    /// Run [`crate::engine::lessons::check_promotion_gate`] for each
    /// lesson and attach the result to `ActiveLesson::gate`. Default
    /// true — the wedge demo. Set false for cost-sensitive callers
    /// (large list rendering).
    pub annotate_with_gate: bool,
    /// Increment `applied_count` + `last_applied_at` for every lesson
    /// in the assembled manifest. Default true (TS parity). Set false
    /// for strictly read-only manifest reads.
    pub record_applied: bool,
    /// Promotion config used when `annotate_with_gate = true`. Default
    /// [`PromotionConfig::default()`].
    pub promotion_config: PromotionConfig,
}

impl Default for AssembleConfig {
    fn default() -> Self {
        Self {
            statuses: vec![LessonStatus::Active],
            lesson_limit: 5,
            body_preview_len: 200,
            annotate_with_gate: true,
            record_applied: true,
            promotion_config: PromotionConfig::default(),
        }
    }
}

/// Diagnostics + counters from one [`assemble`] pass. `assembled_at`
/// stamps the wall-clock at the start of assembly (the same value
/// passed in as `now`) — useful for CLI rendering and cache freshness.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AssemblyStats {
    /// Wall-clock at assembly start (the `now` parameter to [`assemble`]).
    pub assembled_at: DateTime<Utc>,
    /// Total number of lessons listed under the configured statuses
    /// BEFORE the limit cutoff. Useful for "X of Y lessons shown".
    pub total_listed: usize,
    /// Lessons that listed successfully but failed to parse / load —
    /// soft-failed per learn-notes D-C8. Logged at WARN, not in the
    /// final `active_lessons` vec.
    pub skipped_count: usize,
    /// Lessons that loaded successfully but where
    /// `Storage::metadata()` failed → gate annotation skipped, lesson
    /// kept with `gate: None`. Only set when `annotate_with_gate=true`.
    pub gate_skip_count: usize,
    /// Lessons whose `record_applied` write failed → swallowed per
    /// D-C8 (manifest delivery is more important than the counter).
    pub record_applied_failures: usize,
}

impl AssemblyStats {
    fn empty(now: DateTime<Utc>) -> Self {
        Self {
            assembled_at: now,
            total_listed: 0,
            skipped_count: 0,
            gate_skip_count: 0,
            record_applied_failures: 0,
        }
    }
}

/// Assemble a manifest from storage. Pure async function per
/// learn-notes D-C3: borrows `storage` + `config`, takes `now` as a
/// clock-injection parameter (Day 16a D4 pattern), returns the typed
/// `EngineError` family.
///
/// Phase C-C1 ships a SKELETON: returns an empty manifest stub. The
/// listing, ordering, gate annotation, and `record_applied`
/// side-effect land in C-C2 and C-C3.
pub async fn assemble(
    ctx: &Context,
    storage: &dyn Storage,
    config: &AssembleConfig,
    now: DateTime<Utc>,
) -> Result<Manifest, EngineError> {
    // C-C1 placeholder — exercise the parameter set (silence
    // unused-arg warnings without `_ctx, _storage, _config`) and
    // return the empty manifest. C-C2 fills in the listing logic.
    let _ = (ctx, storage);
    validate_config(config)?;
    Ok(Manifest {
        active_lessons: Vec::new(),
        assembly_stats: AssemblyStats::empty(now),
    })
}

/// Reject configurations whose `statuses` vec is empty — that's a
/// caller bug, not a "manifest has zero lessons" condition.
fn validate_config(config: &AssembleConfig) -> Result<(), EngineError> {
    if config.statuses.is_empty() {
        return Err(EngineError::ManifestInvalidStatus {
            status: "<empty statuses vec>".to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::test_support::TestHarness;

    #[test]
    fn assemble_config_default_matches_locked_decisions() {
        let c = AssembleConfig::default();
        assert_eq!(c.statuses, vec![LessonStatus::Active]);
        assert_eq!(c.lesson_limit, 5);
        assert_eq!(c.body_preview_len, 200);
        assert!(c.annotate_with_gate);
        assert!(c.record_applied);
    }

    #[test]
    fn assembly_stats_empty_has_zeroed_counters() {
        let now = "2026-05-13T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let s = AssemblyStats::empty(now);
        assert_eq!(s.assembled_at, now);
        assert_eq!(s.total_listed, 0);
        assert_eq!(s.skipped_count, 0);
        assert_eq!(s.gate_skip_count, 0);
        assert_eq!(s.record_applied_failures, 0);
    }

    #[tokio::test]
    async fn assemble_skeleton_returns_empty_manifest() {
        let h = TestHarness::in_memory();
        let now = "2026-05-13T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            &AssembleConfig::default(),
            now,
        )
        .await
        .unwrap();
        assert!(m.active_lessons.is_empty());
        assert_eq!(m.assembly_stats.assembled_at, now);
        assert_eq!(m.assembly_stats.total_listed, 0);
    }

    #[tokio::test]
    async fn assemble_rejects_empty_statuses() {
        let h = TestHarness::in_memory();
        let now = "2026-05-13T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let config = AssembleConfig {
            statuses: vec![],
            ..AssembleConfig::default()
        };
        let result = assemble(&h.ctx, h.storage.as_ref(), &config, now).await;
        match result {
            Err(EngineError::ManifestInvalidStatus { status }) => {
                assert!(status.contains("empty"));
            }
            other => panic!("expected ManifestInvalidStatus, got {other:?}"),
        }
    }

    #[test]
    fn manifest_invalid_status_display_includes_value() {
        let err = EngineError::ManifestInvalidStatus {
            status: "garbage".to_string(),
        };
        let s = format!("{err}");
        assert!(s.contains("manifest"));
        assert!(s.contains("garbage"));
    }
}
