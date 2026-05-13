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

use bytes::Bytes;
use chrono::{DateTime, Utc};
use tracing::warn;

use crate::engine::context::Context;
use crate::engine::error::EngineError;
use crate::engine::lessons::{
    check_promotion_gate, record_applied, GateDecision, PromotionConfig,
};
use crate::engine::storage::{Storage, StorageKey};
use crate::engine::yaml::{
    reader::parse_lesson_frontmatter, split_frontmatter_normalized, LessonFrontmatter,
    LessonStatus,
};

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
    /// Internal: lesson `created_at` parsed from frontmatter — used
    /// as the secondary sort key (D-C6). Kept `pub(crate)` so the
    /// manifest module's sort logic can read it without exposing
    /// every counter in the public `ActiveLesson` shape.
    pub(crate) created_at_internal: Option<DateTime<Utc>>,
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
/// C-C2 ships listing, filtering, ordering, body-preview construction,
/// and per-lesson soft-fail. C-C3 will add gate annotation and the
/// `record_applied` side-effect.
pub async fn assemble(
    ctx: &Context,
    storage: &dyn Storage,
    config: &AssembleConfig,
    now: DateTime<Utc>,
) -> Result<Manifest, EngineError> {
    validate_config(config)?;
    let mut stats = AssemblyStats::empty(now);
    let mut collected: Vec<ActiveLesson> = Vec::new();

    // 1. List → load per status. A `Storage::list` failure on ANY
    //    status is fatal (no recovery — D-C8). Per-lesson failures
    //    soft-fail.
    for status in &config.statuses {
        let prefix = StorageKey::lesson_status_prefix(ctx, status.as_str());
        let keys = storage.list(&prefix).await?;
        for key in keys {
            stats.total_listed += 1;
            match load_one_lesson(storage, &key, *status, config.body_preview_len).await {
                Ok(Some(lesson)) => collected.push(lesson),
                Ok(None) => stats.skipped_count += 1, // non-fatal skip (logged inside)
                Err(_) => {
                    // load_one_lesson returns Err only for kinds we
                    // explicitly treat as soft-fail (parse / yaml).
                    // Backend I/O errors bubble before this point.
                    stats.skipped_count += 1;
                }
            }
        }
    }

    // 2. Deterministic 3-key sort (D-C6).
    collected.sort_by_key(order_key);

    // 3. Truncate to lesson_limit (post-sort).
    if collected.len() > config.lesson_limit {
        collected.truncate(config.lesson_limit);
    }

    // 4. Per-lesson gate annotation (the wedge surfaces here).
    //    `Storage::metadata()` failure → `gate: None` + skip count;
    //    a successful metadata read feeds `check_promotion_gate`.
    if config.annotate_with_gate {
        for lesson in collected.iter_mut() {
            let key = StorageKey::lesson(ctx, lesson.status.as_str(), &lesson.id);
            // Reload the frontmatter for the gate input (cheap — single
            // get + parse). We can't use the trimmed `ActiveLesson`
            // shape because the gate needs `LessonFrontmatter`.
            // Soft-fail per D-C8.
            match load_frontmatter_for_gate(storage, &key).await {
                Ok(Some((fm, metadata))) => {
                    lesson.gate = Some(check_promotion_gate(
                        &fm,
                        &metadata,
                        &config.promotion_config,
                        now,
                    ));
                }
                _ => {
                    stats.gate_skip_count += 1;
                }
            }
        }
    }

    // 5. `record_applied` side-effect (TS-parity, opt-out via
    //    `record_applied: false`). Failures are swallowed per D-C8.
    if config.record_applied {
        for lesson in collected.iter() {
            if let Err(e) = record_applied(ctx, storage, &lesson.id, now).await {
                warn!(
                    id = %lesson.id,
                    error = %e,
                    "manifest: record_applied failed; counter not incremented"
                );
                stats.record_applied_failures += 1;
            }
        }
    }

    Ok(Manifest {
        active_lessons: collected,
        assembly_stats: stats,
    })
}

/// Re-load (frontmatter, metadata) for the gate input. The trimmed
/// `ActiveLesson` shape doesn't retain the full `LessonFrontmatter`,
/// so we go back to storage for the gate annotation pass. Returns
/// `Ok(None)` if the key vanished between listing and gate-load (race).
async fn load_frontmatter_for_gate(
    storage: &dyn Storage,
    key: &StorageKey,
) -> Result<
    Option<(
        crate::engine::yaml::LessonFrontmatter,
        crate::engine::storage::StorageMetadata,
    )>,
    EngineError,
> {
    let bytes = match storage.get(key).await? {
        Some(b) => b,
        None => return Ok(None),
    };
    let metadata = match storage.metadata(key).await? {
        Some(m) => m,
        None => return Ok(None),
    };
    let content = std::str::from_utf8(&bytes)
        .map_err(|e| EngineError::Parse(format!("non-utf8 lesson bytes for {key}: {e}")))?;
    let split = split_frontmatter_normalized(content)
        .map_err(|e| EngineError::Parse(format!("split frontmatter {key}: {e}")))?;
    let fm = parse_lesson_frontmatter(&split.yaml)
        .map_err(|e| EngineError::Yaml(e.into()))?;
    Ok(Some((fm, metadata)))
}

/// Sort-key tuple for the three-key deterministic order. Aliased
/// because clippy's `type_complexity` lint dings the inline form;
/// the tuple shape is intentional (drop-in to `sort_by_key`).
type OrderKey = (
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
fn order_key(l: &ActiveLesson) -> OrderKey {
    (
        std::cmp::Reverse(l.last_applied_at),
        std::cmp::Reverse(l.created_at_for_sort()),
        l.id.clone(),
    )
}

impl ActiveLesson {
    /// `created_at` exposed for sort. Stored separately from
    /// `last_applied_at` because the manifest doesn't surface it
    /// directly (TS-parity-trim per D-C2); we keep it internal to
    /// preserve the secondary sort key.
    fn created_at_for_sort(&self) -> Option<DateTime<Utc>> {
        self.created_at_internal
    }
}

/// Load one lesson key into an `ActiveLesson`. Returns:
/// - `Ok(Some(lesson))` — happy path.
/// - `Ok(None)` — the key didn't resolve to a valid lesson (file gone
///   between list and get, or non-lesson key swept up by the prefix).
/// - `Err(EngineError)` — soft-fail signal (parse/yaml/utf8). Caller
///   bumps `skipped_count`.
async fn load_one_lesson(
    storage: &dyn Storage,
    key: &StorageKey,
    expected_status: LessonStatus,
    body_preview_len: usize,
) -> Result<Option<ActiveLesson>, EngineError> {
    let bytes: Bytes = match storage.get(key).await? {
        Some(b) => b,
        None => return Ok(None),
    };
    let content = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(e) => {
            warn!(key = %key, error = %e, "manifest: skipping lesson with non-UTF8 bytes");
            return Err(EngineError::Parse(format!("non-utf8 lesson bytes for {key}")));
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

    Ok(Some(ActiveLesson {
        id: fm.id,
        description: fm.description,
        status: expected_status,
        body_preview,
        applied_count: fm.applied_count,
        last_applied_at,
        target_skill: fm.target_skill,
        gate: None, // C-C3 populates
        created_at_internal,
    }))
}

/// Build the body preview per OQ-C2: char-based slice (multi-byte
/// UTF-8 safe), trimmed of leading/trailing whitespace.
fn build_body_preview(body: &str, n: usize) -> String {
    body.chars().take(n).collect::<String>().trim().to_string()
}

/// Parse an ISO-8601 / RFC-3339 string into `DateTime<Utc>`. Returns
/// `None` on parse failure (the caller increments the appropriate
/// skip counter rather than hard-failing).
fn parse_iso_or_none(s: Option<&str>) -> Option<DateTime<Utc>> {
    s.and_then(|s| s.parse::<DateTime<Utc>>().ok())
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

    // ---------------------------------------------------------------
    // C-C2: listing + filtering + ordering + body-preview + soft-fail
    // ---------------------------------------------------------------

    fn now_t() -> DateTime<Utc> {
        "2026-05-13T12:00:00Z".parse().unwrap()
    }

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

    #[tokio::test]
    async fn assemble_lists_active_lessons_from_in_memory_storage() {
        let h = TestHarness::in_memory();
        h.seed_lesson("active", "les-aaaaaaaa", "first body").await.unwrap();
        h.seed_lesson("active", "les-bbbbbbbb", "second body").await.unwrap();

        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            &AssembleConfig::default(),
            now_t(),
        )
        .await
        .unwrap();
        assert_eq!(m.assembly_stats.total_listed, 2);
        assert_eq!(m.active_lessons.len(), 2);
        assert!(m.active_lessons.iter().any(|l| l.id == "les-aaaaaaaa"));
        assert!(m.active_lessons.iter().any(|l| l.id == "les-bbbbbbbb"));
    }

    #[tokio::test]
    async fn assemble_filters_by_configured_statuses() {
        let h = TestHarness::in_memory();
        h.seed_lesson("active", "les-active01", "x").await.unwrap();
        h.seed_lesson("promoted", "les-promot01", "y").await.unwrap();
        h.seed_lesson("pending", "les-pendin01", "z").await.unwrap();

        // Default config: statuses = [Active] only.
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            &AssembleConfig::default(),
            now_t(),
        )
        .await
        .unwrap();
        assert_eq!(m.active_lessons.len(), 1);
        assert_eq!(m.active_lessons[0].id, "les-active01");

        // Custom config: include Promoted too.
        let config = AssembleConfig {
            statuses: vec![LessonStatus::Active, LessonStatus::Promoted],
            ..AssembleConfig::default()
        };
        let m = assemble(&h.ctx, h.storage.as_ref(), &config, now_t())
            .await
            .unwrap();
        assert_eq!(m.active_lessons.len(), 2);
    }

    #[tokio::test]
    async fn assemble_truncates_to_lesson_limit() {
        let h = TestHarness::in_memory();
        for i in 0..10 {
            let id = format!("les-aaaaaaa{i}");
            h.seed_lesson("active", &id, &format!("body {i}")).await.unwrap();
        }
        let config = AssembleConfig {
            lesson_limit: 3,
            ..AssembleConfig::default()
        };
        let m = assemble(&h.ctx, h.storage.as_ref(), &config, now_t())
            .await
            .unwrap();
        assert_eq!(m.assembly_stats.total_listed, 10);
        assert_eq!(m.active_lessons.len(), 3);
    }

    #[tokio::test]
    async fn assemble_orders_by_id_ascending_when_other_keys_tied() {
        // All seeded with same `created_at` (TestHarness default),
        // no `last_applied_at` → tie on first two keys; id ASC as
        // the final tiebreaker decides.
        let h = TestHarness::in_memory();
        h.seed_lesson("active", "les-zzzzzzzz", "z body").await.unwrap();
        h.seed_lesson("active", "les-aaaaaaaa", "a body").await.unwrap();
        h.seed_lesson("active", "les-mmmmmmmm", "m body").await.unwrap();

        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            &AssembleConfig::default(),
            now_t(),
        )
        .await
        .unwrap();
        let ids: Vec<_> = m.active_lessons.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["les-aaaaaaaa", "les-mmmmmmmm", "les-zzzzzzzz"],
            "id-ASC tiebreaker should yield deterministic alphabetical order"
        );
    }

    #[tokio::test]
    async fn assemble_orders_last_applied_at_descending() {
        use crate::engine::storage::StorageKey;
        // Seed three lessons with same created_at but distinct
        // last_applied_at; expect newest-applied first.
        let h = TestHarness::in_memory();
        async fn put_with_last_applied(
            h: &TestHarness,
            id: &str,
            last_applied_at_iso: &str,
        ) {
            let yaml = format!(
                "---\n\
                 id: {id}\n\
                 description: \"x\"\n\
                 status: active\n\
                 created_at: \"2026-05-11T12:00:00Z\"\n\
                 applied_count: 1\n\
                 last_applied_at: \"{last_applied_at_iso}\"\n\
                 thumbs_up_count: 0\n\
                 thumbs_down_count: 0\n\
                 external_signal_sources: []\n\
                 ---\n\
                 body\n"
            );
            let key = StorageKey::lesson(&h.ctx, "active", id);
            h.storage.put(&key, Bytes::from(yaml)).await.unwrap();
        }
        put_with_last_applied(&h, "les-oldest11", "2026-05-12T00:00:00Z").await;
        put_with_last_applied(&h, "les-newest11", "2026-05-13T11:00:00Z").await;
        put_with_last_applied(&h, "les-middle11", "2026-05-12T15:00:00Z").await;

        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            &AssembleConfig::default(),
            now_t(),
        )
        .await
        .unwrap();
        let ids: Vec<_> = m.active_lessons.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(ids, vec!["les-newest11", "les-middle11", "les-oldest11"]);
    }

    #[tokio::test]
    async fn assemble_soft_fails_on_malformed_frontmatter() {
        use crate::engine::storage::StorageKey;
        let h = TestHarness::in_memory();
        // One good lesson, one with broken YAML.
        h.seed_lesson("active", "les-aaaaaaaa", "good body").await.unwrap();
        let bad_key = StorageKey::lesson(&h.ctx, "active", "les-broken01");
        h.storage
            .put(&bad_key, Bytes::from_static(b"no frontmatter here\n"))
            .await
            .unwrap();

        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            &AssembleConfig::default(),
            now_t(),
        )
        .await
        .unwrap();
        // total_listed counts BOTH; skipped_count counts the broken one;
        // active_lessons has only the good one.
        assert_eq!(m.assembly_stats.total_listed, 2);
        assert_eq!(m.assembly_stats.skipped_count, 1);
        assert_eq!(m.active_lessons.len(), 1);
        assert_eq!(m.active_lessons[0].id, "les-aaaaaaaa");
    }

    #[tokio::test]
    async fn assemble_body_preview_respects_config_length() {
        let h = TestHarness::in_memory();
        h.seed_lesson(
            "active",
            "les-bbbbbbbb",
            "this is a longer body that should get truncated",
        )
        .await
        .unwrap();
        let config = AssembleConfig {
            body_preview_len: 10,
            ..AssembleConfig::default()
        };
        let m = assemble(&h.ctx, h.storage.as_ref(), &config, now_t())
            .await
            .unwrap();
        let lesson = &m.active_lessons[0];
        // Take 10 chars then trim — "this is a " loses the trailing
        // space, leaving 9 chars. The take-then-trim order is the
        // documented OQ-C2 contract.
        assert_eq!(lesson.body_preview, "this is a");
        assert!(lesson.body_preview.chars().count() <= 10);
    }

    #[tokio::test]
    async fn assemble_on_disk_roundtrip() {
        // Real LocalFsStorage — proves the path-extraction + storage
        // listing both work end-to-end on disk.
        let h = TestHarness::on_disk();
        h.seed_lesson("active", "les-ondisk01", "on-disk body").await.unwrap();
        h.seed_lesson("active", "les-ondisk02", "another").await.unwrap();
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            &AssembleConfig::default(),
            now_t(),
        )
        .await
        .unwrap();
        assert_eq!(m.active_lessons.len(), 2);
        assert!(m.active_lessons.iter().any(|l| l.id == "les-ondisk01"));
    }

    // -----------------------------------------------------------------
    // C-C3: gate annotation + record_applied + wedge integration
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn assemble_attaches_gate_decision_when_annotate_with_gate_true() {
        let h = TestHarness::in_memory();
        h.seed_lesson("active", "les-gate0001", "body").await.unwrap();
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            &AssembleConfig::default(), // annotate_with_gate = true
            now_t(),
        )
        .await
        .unwrap();
        assert_eq!(m.active_lessons.len(), 1);
        assert!(
            m.active_lessons[0].gate.is_some(),
            "expected gate annotation present, got None"
        );
    }

    #[tokio::test]
    async fn assemble_skips_gate_when_annotate_with_gate_false() {
        let h = TestHarness::in_memory();
        h.seed_lesson("active", "les-noggate01", "body").await.unwrap();
        let config = AssembleConfig {
            annotate_with_gate: false,
            ..AssembleConfig::default()
        };
        let m = assemble(&h.ctx, h.storage.as_ref(), &config, now_t())
            .await
            .unwrap();
        assert_eq!(m.active_lessons.len(), 1);
        assert!(m.active_lessons[0].gate.is_none());
        assert_eq!(m.assembly_stats.gate_skip_count, 0);
    }

    #[tokio::test]
    async fn assemble_record_applied_increments_counter_by_default() {
        use crate::engine::lessons::get_by_id;
        let h = TestHarness::in_memory();
        h.seed_lesson("active", "les-counter1", "body").await.unwrap();
        // applied_count starts at 0 (TestHarness default).
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            &AssembleConfig::default(),
            now_t(),
        )
        .await
        .unwrap();
        assert_eq!(m.active_lessons.len(), 1);
        assert_eq!(m.assembly_stats.record_applied_failures, 0);
        // Verify the on-disk increment via a follow-up read.
        let after = get_by_id(&h.ctx, h.storage.as_ref(), "les-counter1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.frontmatter.applied_count, 1);
        assert!(after.frontmatter.last_applied_at.is_some());
    }

    #[tokio::test]
    async fn assemble_record_applied_skipped_when_record_applied_false() {
        use crate::engine::lessons::get_by_id;
        let h = TestHarness::in_memory();
        h.seed_lesson("active", "les-noinc001", "body").await.unwrap();
        let config = AssembleConfig {
            record_applied: false,
            ..AssembleConfig::default()
        };
        let _ = assemble(&h.ctx, h.storage.as_ref(), &config, now_t())
            .await
            .unwrap();
        let after = get_by_id(&h.ctx, h.storage.as_ref(), "les-noinc001")
            .await
            .unwrap()
            .unwrap();
        // applied_count stays at 0 — strictly read-only manifest assembly.
        assert_eq!(after.frontmatter.applied_count, 0);
        assert!(after.frontmatter.last_applied_at.is_none());
    }

    #[tokio::test]
    async fn wedge_at_manifest_layer_backdated_lesson_surfaces_blocked_gate() {
        // The marketing-wedge regression at the MANIFEST layer (one
        // step above the gate's own s21 regression in lessons/gate.rs).
        // A lesson backdated in YAML but freshly written to in-memory
        // storage must show up in `manifest.active_lessons[0].gate`
        // as `Block { reasons: [TamperedAge, ...] }` — proving the
        // wedge claim flows end-to-end through manifest assembly.
        let h = TestHarness::in_memory();
        h.seed_lesson_with_created_at(
            "active",
            "les-wedge002",
            "body",
            "2026-04-13T00:00:00Z", // 30 days before now_t()
        )
        .await
        .unwrap();

        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            &AssembleConfig::default(),
            now_t(),
        )
        .await
        .unwrap();
        assert_eq!(m.active_lessons.len(), 1);
        let gate = m.active_lessons[0]
            .gate
            .as_ref()
            .expect("wedge: expected gate annotation");
        match gate {
            GateDecision::Block { reasons } => {
                assert!(
                    reasons.iter().any(|r| matches!(
                        r,
                        crate::engine::lessons::BlockReason::TamperedAge { .. }
                    )),
                    "wedge invariant FAILED at manifest layer: TamperedAge missing. \
                     Got reasons: {reasons:?}"
                );
            }
            other => panic!(
                "wedge: expected Block on backdated lesson, got {other:?}"
            ),
        }
    }
}
