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

mod internal;
mod session;

pub use session::SessionState;

use chrono::{DateTime, Utc};
use tracing::warn;

use crate::engine::context::Context;
use crate::engine::embedding::Embedder;
use crate::engine::error::EngineError;
use crate::engine::lessons::{check_promotion_gate, record_applied, GateDecision, PromotionConfig};
use crate::engine::memory::{self, MemoryQuery, MemoryRef};
use crate::engine::storage::{Storage, StorageKey};
use crate::engine::vector::VectorIndex;
use crate::engine::yaml::LessonStatus;

/// The structured context bundle surfaced to host LLMs. Phase C ships
/// `active_lessons` + `assembly_stats`; Phase E adds memories, Phase F
/// adds skills/personas/teams as additive fields.
///
/// `#[non_exhaustive]` so the engine can grow new sections without a
/// SemVer break. External callers should pattern-match with wildcards
/// or use field-access (which IS forward-compatible).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Manifest {
    /// Active lessons in deterministic order (per [`AssembleConfig`]).
    pub active_lessons: Vec<ActiveLesson>,
    /// Phase E C-E4: top-k memories relevant to the configured
    /// [`AssembleConfig::memory_query`]. Empty when `memory_query` is
    /// `None`. Ordered by descending similarity score.
    pub memories: Vec<MemoryRef>,
    /// Phase F C-F4: skills the host marked active via
    /// `SessionState::active_skill_ids`. Empty when no `SessionState`
    /// was supplied OR no matching skills exist.
    pub active_skills: Vec<crate::engine::skills::SkillRef>,
    /// Phase F C-F4: active personas. Same population semantics.
    pub active_personas: Vec<crate::engine::personas::PersonaRef>,
    /// Phase F C-F4: active teams.
    pub active_teams: Vec<crate::engine::teams::TeamRef>,
    /// Diagnostics + summary stats for the assembly pass that produced
    /// this manifest — useful for CLI rendering, debugging, and caller
    /// observability.
    pub assembly_stats: AssemblyStats,
}

/// One lesson in the manifest's `active_lessons` list. TS-parity-trimmed
/// (we do NOT expose the full [`crate::engine::yaml::LessonFrontmatter`]
/// — every counter would become a SemVer hinge) PLUS the engine-side
/// wedge addition `gate`.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    // Lesson `created_at` parsed from frontmatter — secondary sort key
    // (D-C6). Strictly private; the manifest module's sort logic reads
    // it directly. NOT in the public field set per D-C2 (TS-parity-trim).
    created_at_internal: Option<DateTime<Utc>>,
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
    /// Phase E C-E4: memory section query driver. `None` (default)
    /// → no memory search; manifest's `memories` field stays empty.
    /// `Some(MemoryQuery::Text(_))` → engine embeds via the
    /// supplied [`Embedder`]; `Some(MemoryQuery::Vector(_))` → caller
    /// pre-embedded. Set this to populate `Manifest::memories`.
    pub memory_query: Option<MemoryQuery>,
    /// Max number of memories to return in the manifest's `memories`
    /// section. Default 5. Ignored when `memory_query` is `None`.
    pub memory_limit: usize,
    /// Phase F C-F4: optional scope filter on the memory section.
    /// `None` (default) returns memories of any scope. Set e.g.
    /// `Some(MemoryScopeFilter::Kind("team"))` to filter to team-
    /// scoped memories only.
    pub memory_scope_filter: Option<crate::engine::memory::MemoryScopeFilter>,
}

// `SessionState` + `populate_active_session_sections` extracted to
// `manifest/session.rs` per audit-fix close finding B-M1 (file-size
// cap). Re-exported below via `pub use session::SessionState`.

impl Default for AssembleConfig {
    fn default() -> Self {
        Self {
            statuses: vec![LessonStatus::Active],
            lesson_limit: 5,
            body_preview_len: 200,
            annotate_with_gate: true,
            record_applied: true,
            promotion_config: PromotionConfig::default(),
            memory_query: None,
            memory_limit: 5,
            memory_scope_filter: None,
        }
    }
}

/// Diagnostics + counters from one [`assemble`] pass. `assembled_at`
/// stamps the wall-clock at the start of assembly (the same value
/// passed in as `now`) — useful for CLI rendering and cache freshness.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Phase E C-E4: memory search outcome. `None` when no memory
    /// query was configured. `Some(n)` is the count of memories
    /// returned in `Manifest::memories`. `Some(0)` is a successful
    /// search that found nothing.
    pub memories_returned: Option<usize>,
    /// Phase E C-E4: when the memory search call itself errored
    /// (embedder failure, vector index failure), the manifest is
    /// still delivered with empty `memories` and this counter
    /// increments. Soft-fail per D-C8.
    pub memory_search_failures: usize,
    /// Phase F C-F4: number of `SessionState` ids (across skills,
    /// personas, teams) that didn't resolve to on-disk records.
    /// Stale-state signal — host should investigate.
    pub session_section_skips: usize,
}

impl AssemblyStats {
    fn empty(now: DateTime<Utc>) -> Self {
        Self {
            assembled_at: now,
            total_listed: 0,
            skipped_count: 0,
            gate_skip_count: 0,
            record_applied_failures: 0,
            memories_returned: None,
            memory_search_failures: 0,
            session_section_skips: 0,
        }
    }
}

/// Assemble a manifest from storage. Pure async function per
/// learn-notes D-C3: borrows `storage` + `config`, takes `now` as a
/// clock-injection parameter (Day 16a D4 pattern), returns the typed
/// `EngineError` family.
///
/// Phase C-C2 + C-C3 + audit-fix close: single-load pipeline.
///
/// 1. List → load each lesson ONCE (per status). Per-lesson failures
///    soft-fail per D-C8; backend `list` failures are fatal.
/// 2. Deterministic 3-key sort (D-C6) via `sort_by_cached_key` to avoid
///    cloning the `id: String` per comparison.
/// 3. Truncate to `lesson_limit`.
/// 4. Per-lesson gate annotation — uses the CACHED frontmatter from
///    step 1, fetches only fresh `Storage::metadata` (audit A-M3:
///    eliminates the redundant get+parse per lesson).
/// 5. `record_applied` in PARALLEL via `futures::future::join_all`
///    (audit A-M2: was serial, ~250ms at lesson_limit=5; now ~50ms).
/// 6. Phase E C-E4: memory search step. When `config.memory_query`
///    is `Some(_)` AND `embedder` + `vector_index` are supplied, run
///    `memory::search` and populate `Manifest::memories`. Soft-fails
///    on embedder/index errors (manifest delivery > memory section).
///    Pass `embedder = None` + `vector_index = None` for memory-free
///    callers (Phase B/C-style consumers).
#[allow(clippy::too_many_arguments)] // Plumbing for E/F traits + Phase F SessionState
pub async fn assemble(
    ctx: &Context,
    storage: &dyn Storage,
    embedder: Option<&dyn Embedder>,
    vector_index: Option<&dyn VectorIndex>,
    session_state: Option<&SessionState>,
    config: &AssembleConfig,
    now: DateTime<Utc>,
) -> Result<Manifest, EngineError> {
    internal::validate_config(config)?;
    let mut stats = AssemblyStats::empty(now);
    let mut records: Vec<internal::LoadedRecord> = Vec::new();

    // Step 1: list → single-load per lesson. Holding the parsed
    // frontmatter alongside the trimmed `ActiveLesson` means step 4
    // never re-loads (audit A-M3 fix).
    for status in &config.statuses {
        let prefix = StorageKey::lesson_status_prefix(ctx, status.as_str());
        let keys = storage.list(&prefix).await?;
        for key in keys {
            stats.total_listed += 1;
            match internal::load_one_record(storage, &key, *status, config.body_preview_len).await {
                Ok(Some(record)) => records.push(record),
                Ok(None) => stats.skipped_count += 1,
                Err(_) => stats.skipped_count += 1,
            }
        }
    }

    // Step 2: deterministic 3-key sort. `sort_by_cached_key` calls the
    // closure once per element (cached) rather than twice per
    // comparison, so the `id.clone()` cost is O(n) not O(n log n)
    // (audit A-m11 fix).
    records.sort_by_cached_key(|r| internal::order_key(&r.active));

    // Step 3: truncate to lesson_limit.
    if records.len() > config.lesson_limit {
        records.truncate(config.lesson_limit);
    }

    // Step 4: gate annotation. CACHED frontmatter from step 1 + fresh
    // metadata fetch. The two failure cases are now distinguished
    // explicitly per audit A-M4 (lesson-vanished vs metadata-absent).
    if config.annotate_with_gate {
        for r in records.iter_mut() {
            let metadata_result = storage.metadata(&r.key).await;
            match metadata_result {
                Ok(Some(metadata)) => {
                    r.active.gate = Some(check_promotion_gate(
                        &r.fm,
                        &metadata,
                        &config.promotion_config,
                        now,
                    ));
                }
                Ok(None) => {
                    // Lesson vanished between listing and gate-load
                    // (race). Storage.get would have returned bytes
                    // but metadata returns None — keep the lesson in
                    // the manifest, skip its gate annotation.
                    warn!(
                        key = %r.key,
                        "manifest: lesson vanished between list and gate annotation"
                    );
                    stats.gate_skip_count += 1;
                }
                Err(e) => {
                    // Backend I/O error reading metadata. Soft-fail
                    // per D-C8 — manifest delivery is more important
                    // than per-lesson gate visibility.
                    warn!(
                        key = %r.key,
                        error = %e,
                        "manifest: metadata fetch failed for gate annotation"
                    );
                    stats.gate_skip_count += 1;
                }
            }
        }
    }

    // Step 5: `record_applied` in parallel. Each future is independent
    // (different lesson key) so concurrent execution is safe; the CAS
    // discipline inside `record_applied` handles per-key contention.
    // futures::future::join_all polls inline on the current task — no
    // 'static / Send constraints from spawning.
    if config.record_applied {
        let futures = records
            .iter()
            .map(|r| record_applied(ctx, storage, &r.active.id, now));
        let results = futures::future::join_all(futures).await;
        for (r, result) in records.iter().zip(results.iter()) {
            if let Err(e) = result {
                warn!(
                    id = %r.active.id,
                    error = %e,
                    "manifest: record_applied failed; counter not incremented"
                );
                stats.record_applied_failures += 1;
            }
        }
    }

    // Project the internal records to the public `ActiveLesson` shape.
    let active_lessons: Vec<ActiveLesson> = records.into_iter().map(|r| r.active).collect();

    // Step 6: Phase E C-E4 — memory search. Runs only when ALL of
    // (memory_query is Some, embedder is Some, vector_index is Some).
    // Soft-fails on backend errors — manifest delivery is more
    // important than the memory section (D-C8 pattern).
    let memories: Vec<MemoryRef> = match (&config.memory_query, embedder, vector_index) {
        (Some(query), Some(emb), Some(vi)) => {
            // Phase F audit-fix close (C1/C2 wire-up): over-fetch
            // by 2x when a scope filter is set, then post-filter.
            // Approximates "k results matching the filter" without
            // requiring the vector backend to be scope-aware.
            let raw_limit = if config.memory_scope_filter.is_some() {
                config
                    .memory_limit
                    .saturating_mul(2)
                    .max(config.memory_limit)
            } else {
                config.memory_limit
            };
            match memory::search(
                ctx,
                storage,
                emb,
                vi,
                query,
                raw_limit,
                config.body_preview_len,
                // Manifest assembly does its own post-filter via
                // `filter_refs_by_scope` below (preserves the original
                // over-fetch-then-truncate-to-k semantics). Passing
                // `None` here keeps that path identical to pre-v0.3.1
                // behavior.
                None,
            )
            .await
            {
                Ok(mut refs) => {
                    if let Some(filter) = &config.memory_scope_filter {
                        refs = internal::filter_refs_by_scope(ctx, storage, refs, filter).await;
                        refs.truncate(config.memory_limit);
                    }
                    stats.memories_returned = Some(refs.len());
                    refs
                }
                Err(e) => {
                    warn!(error = %e, "manifest: memory search failed; section empty");
                    stats.memory_search_failures += 1;
                    stats.memories_returned = Some(0);
                    Vec::new()
                }
            }
        }
        _ => Vec::new(),
    };

    // Step 7: Phase F C-F4 — populate active_skills/personas/teams
    // from the SessionState's id lists. Soft-fails per-id: missing
    // ids are warn-logged + skipped (host might have stale state
    // that points at archived entries).
    let (active_skills, active_personas, active_teams) = match session_state {
        Some(state) => {
            session::populate_active_session_sections(ctx, storage, state, &mut stats).await?
        }
        None => (Vec::new(), Vec::new(), Vec::new()),
    };

    Ok(Manifest {
        active_lessons,
        memories,
        active_skills,
        active_personas,
        active_teams,
        assembly_stats: stats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::memory::MemoryId;
    use crate::engine::test_support::TestHarness;
    use bytes::Bytes;

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
            None,
            None,
            None,
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
        let result = assemble(&h.ctx, h.storage.as_ref(), None, None, None, &config, now).await;
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

    #[tokio::test]
    async fn assemble_lists_active_lessons_from_in_memory_storage() {
        let h = TestHarness::in_memory();
        h.seed_lesson("active", "les-aaaaaaaa", "first body")
            .await
            .unwrap();
        h.seed_lesson("active", "les-bbbbbbbb", "second body")
            .await
            .unwrap();

        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            None,
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
        h.seed_lesson("promoted", "les-promot01", "y")
            .await
            .unwrap();
        h.seed_lesson("pending", "les-pendin01", "z").await.unwrap();

        // Default config: statuses = [Active] only.
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            None,
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
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            None,
            &config,
            now_t(),
        )
        .await
        .unwrap();
        assert_eq!(m.active_lessons.len(), 2);
    }

    #[tokio::test]
    async fn assemble_truncates_to_lesson_limit() {
        let h = TestHarness::in_memory();
        for i in 0..10 {
            let id = format!("les-aaaaaaa{i}");
            h.seed_lesson("active", &id, &format!("body {i}"))
                .await
                .unwrap();
        }
        let config = AssembleConfig {
            lesson_limit: 3,
            ..AssembleConfig::default()
        };
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            None,
            &config,
            now_t(),
        )
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
        h.seed_lesson("active", "les-zzzzzzzz", "z body")
            .await
            .unwrap();
        h.seed_lesson("active", "les-aaaaaaaa", "a body")
            .await
            .unwrap();
        h.seed_lesson("active", "les-mmmmmmmm", "m body")
            .await
            .unwrap();

        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            None,
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
        async fn put_with_last_applied(h: &TestHarness, id: &str, last_applied_at_iso: &str) {
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
            None,
            None,
            None,
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
        h.seed_lesson("active", "les-aaaaaaaa", "good body")
            .await
            .unwrap();
        let bad_key = StorageKey::lesson(&h.ctx, "active", "les-broken01");
        h.storage
            .put(&bad_key, Bytes::from_static(b"no frontmatter here\n"))
            .await
            .unwrap();

        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            None,
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
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            None,
            &config,
            now_t(),
        )
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
        h.seed_lesson("active", "les-ondisk01", "on-disk body")
            .await
            .unwrap();
        h.seed_lesson("active", "les-ondisk02", "another")
            .await
            .unwrap();
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            None,
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
        h.seed_lesson("active", "les-gate0001", "body")
            .await
            .unwrap();
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            None,
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
        h.seed_lesson("active", "les-noggate01", "body")
            .await
            .unwrap();
        let config = AssembleConfig {
            annotate_with_gate: false,
            ..AssembleConfig::default()
        };
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            None,
            &config,
            now_t(),
        )
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
        h.seed_lesson("active", "les-counter1", "body")
            .await
            .unwrap();
        // applied_count starts at 0 (TestHarness default).
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            None,
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
        h.seed_lesson("active", "les-noinc001", "body")
            .await
            .unwrap();
        let config = AssembleConfig {
            record_applied: false,
            ..AssembleConfig::default()
        };
        let _ = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            None,
            &config,
            now_t(),
        )
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
        // The marketing-wedge regression at the MANIFEST layer.
        // Mirrors lessons/gate.rs::s21 ISOLATION assertion (Phase B
        // audit M3 fix pattern + Phase C audit A-M1 fix): the wedge
        // test must prove TamperedAge is the SOLE block reason — not
        // merely "TamperedAge is among the reasons" — otherwise other
        // rules co-firing could mask wedge regressions.
        //
        // Direct-write a lesson with backdated frontmatter AND every
        // other rule passing in isolation: birthtime (= when the put
        // happens, ie wall-clock now) > frontmatter created_at
        // (2026-04-13) by ~30 days, so TamperedAge fires. All other
        // rules are satisfied by the rounded-out fixture, so the
        // assertion `reasons.len() == 1` proves the wedge specifically
        // caught the backdate.
        use crate::engine::storage::StorageKey;

        let h = TestHarness::in_memory();
        let id = "les-wedge002";
        let backdated = "2026-04-13T00:00:00Z";
        let yaml = format!(
            "---\n\
             id: {id}\n\
             description: \"wedge regression\"\n\
             status: active\n\
             created_at: \"{backdated}\"\n\
             causal_narrative:\n  trigger: \"t\"\n  failure_mode: \"f\"\n  correction: \"c\"\n  confidence: inferred\n  evidence_refs: []\n  generated_by: llm\n  generated_at: \"{backdated}\"\n\
             applied_count: 5\n\
             thumbs_up_count: 2\n\
             thumbs_down_count: 0\n\
             external_signal_sources:\n  - thumbs_up\n\
             ---\n\
             body\n"
        );
        let key = StorageKey::lesson(&h.ctx, "active", id);
        h.storage.put(&key, Bytes::from(yaml)).await.unwrap();

        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            None,
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
                assert_eq!(
                    reasons.len(),
                    1,
                    "wedge regression FAILED at manifest layer: expected exactly 1 \
                     reason (TamperedAge), got {} reasons: {reasons:?}. The wedge \
                     test is over-passing — other rules co-fire, so we can't prove \
                     the wedge specifically caught the backdate.",
                    reasons.len()
                );
                assert!(
                    matches!(
                        reasons[0],
                        crate::engine::lessons::BlockReason::TamperedAge { .. }
                    ),
                    "wedge invariant FAILED: sole block reason should be TamperedAge, \
                     got {:?}",
                    reasons[0]
                );
            }
            other => panic!("wedge: expected Block on backdated lesson, got {other:?}"),
        }

        // Verify cross-cutting behavior: even though the gate blocks
        // promotion eligibility, the manifest still delivered the
        // lesson AND `record_applied` (default true) incremented the
        // counter from 5 → 6. Wedge blocks PROMOTION, not manifest
        // delivery or applied tracking.
        use crate::engine::lessons::get_by_id;
        let after = get_by_id(&h.ctx, h.storage.as_ref(), id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.frontmatter.applied_count, 6);
    }

    // ---------------------------------------------------------------
    // C-E4: memory section integration
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn assemble_memories_empty_when_query_is_none() {
        let h = TestHarness::in_memory();
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            None,
            &AssembleConfig::default(),
            now_t(),
        )
        .await
        .unwrap();
        assert!(m.memories.is_empty());
        assert!(m.assembly_stats.memories_returned.is_none());
        assert_eq!(m.assembly_stats.memory_search_failures, 0);
    }

    #[tokio::test]
    async fn assemble_memories_populated_when_query_text_with_embedder_and_index() {
        use crate::engine::embedding::MockEmbedder;
        use crate::engine::memory;
        use crate::engine::vector::HnswVectorIndex;

        let h = TestHarness::in_memory();
        let dim = 4;
        let embedder = MockEmbedder::new(dim)
            // Insert call: produces a vec aligned along axis 0.
            .with_response(vec![vec![1.0, 0.0, 0.0, 0.0]])
            // Query call: same axis so search returns the inserted memory.
            .with_response(vec![vec![1.0, 0.0, 0.0, 0.0]]);
        let vector_index = HnswVectorIndex::new(dim);

        let mid = MemoryId::new("mem-aaaaaaaa");
        memory::insert(
            &h.ctx,
            h.storage.as_ref(),
            &embedder,
            &vector_index,
            mid.clone(),
            "test memory",
            "memory body",
            now_t(),
        )
        .await
        .unwrap();

        let config = AssembleConfig {
            memory_query: Some(MemoryQuery::Text("query text".to_string())),
            memory_limit: 5,
            ..AssembleConfig::default()
        };

        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            Some(&embedder),
            Some(&vector_index),
            None,
            &config,
            now_t(),
        )
        .await
        .unwrap();
        assert_eq!(m.memories.len(), 1);
        assert_eq!(m.memories[0].id, mid);
        assert_eq!(m.memories[0].description, "test memory");
        assert_eq!(m.assembly_stats.memories_returned, Some(1));
    }

    #[tokio::test]
    async fn assemble_memories_skipped_when_only_partial_plumbing() {
        // memory_query Some but embedder/vector_index None → empty
        // memories, no error.
        let h = TestHarness::in_memory();
        let config = AssembleConfig {
            memory_query: Some(MemoryQuery::Vector(vec![0.0; 4])),
            ..AssembleConfig::default()
        };
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            None,
            &config,
            now_t(),
        )
        .await
        .unwrap();
        assert!(m.memories.is_empty());
        // memories_returned stays None — the query path didn't run
        // because plumbing was incomplete.
        assert!(m.assembly_stats.memories_returned.is_none());
    }

    #[tokio::test]
    async fn wedge_cross_cutting_user_immune_memory_survives_prune_visible_to_search() {
        // THE cross-cutting wedge regression at the memory layer.
        // 1. Insert a memory.
        // 2. Simulate a user-authored lesson citing it (increment counter).
        // 3. Run a prune-everything predicate → memory survives.
        // 4. Assemble manifest → memory still surfaces in the
        //    `memories` section (proves immunity protects DISCOVERY,
        //    not just storage).
        use crate::engine::embedding::MockEmbedder;
        use crate::engine::memory;
        use crate::engine::vector::HnswVectorIndex;

        let h = TestHarness::in_memory();
        let dim = 4;
        let embedder = MockEmbedder::new(dim)
            .with_response(vec![vec![1.0, 0.0, 0.0, 0.0]])
            .with_response(vec![vec![1.0, 0.0, 0.0, 0.0]]);
        let vector_index = HnswVectorIndex::new(dim);
        let mid = MemoryId::new("mem-cited001");
        memory::insert(
            &h.ctx,
            h.storage.as_ref(),
            &embedder,
            &vector_index,
            mid.clone(),
            "user-cited memory",
            "important context",
            now_t(),
        )
        .await
        .unwrap();
        // Simulate user citation.
        memory::increment_citation_count(&h.ctx, h.storage.as_ref(), &mid)
            .await
            .unwrap();

        // Prune-everything predicate.
        let pred: crate::engine::memory::PrunePredicate = Box::new(|_fm| true);
        let stats = memory::prune(&h.ctx, h.storage.as_ref(), &vector_index, pred)
            .await
            .unwrap();
        assert_eq!(stats.pruned, 0, "user-immune memory MUST survive prune");
        assert_eq!(stats.skipped_user_immune, 1);

        // The memory is still in storage AND in the index. Assemble
        // and query — should surface.
        let config = AssembleConfig {
            memory_query: Some(MemoryQuery::Text("query".to_string())),
            ..AssembleConfig::default()
        };
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            Some(&embedder),
            Some(&vector_index),
            None,
            &config,
            now_t(),
        )
        .await
        .unwrap();
        assert_eq!(
            m.memories.len(),
            1,
            "user-immune memory MUST still be discoverable"
        );
        assert_eq!(m.memories[0].id, mid);
    }

    /// Phase E audit A-M3: NEGATIVE control for the wedge-immunity
    /// test above. Cross-phase pattern (B M3 / C M1 / D M1 / E M3 —
    /// four phases of the same pattern). Without this negative
    /// control, the positive test would pass equally in a bug-world
    /// where `prune` is a no-op. This test proves prune DOES evict
    /// uncited memories — so the positive test's survival of an
    /// immune memory is meaningful.
    #[tokio::test]
    async fn wedge_negative_control_uncited_memory_is_pruned() {
        use crate::engine::embedding::MockEmbedder;
        use crate::engine::memory;
        use crate::engine::vector::HnswVectorIndex;

        let h = TestHarness::in_memory();
        let dim = 4;
        let embedder = MockEmbedder::new(dim).with_response(vec![vec![1.0, 0.0, 0.0, 0.0]]);
        let vector_index = HnswVectorIndex::new(dim);
        let mid = MemoryId::new("mem-uncited1");
        memory::insert(
            &h.ctx,
            h.storage.as_ref(),
            &embedder,
            &vector_index,
            mid.clone(),
            "uncited memory",
            "body",
            now_t(),
        )
        .await
        .unwrap();
        // DELIBERATELY do NOT call `increment_citation_count` — the
        // memory has no user-lesson citations, so the immunity guard
        // does NOT apply.

        let pred: crate::engine::memory::PrunePredicate = Box::new(|_fm| true);
        let stats = memory::prune(&h.ctx, h.storage.as_ref(), &vector_index, pred)
            .await
            .unwrap();
        assert_eq!(
            stats.pruned, 1,
            "uncited memory MUST be evicted by a prune-everything predicate"
        );
        assert_eq!(stats.skipped_user_immune, 0);
        // Memory is GONE from storage.
        let after = memory::get_by_id(&h.ctx, h.storage.as_ref(), &mid)
            .await
            .unwrap();
        assert!(after.is_none(), "memory should be deleted from storage");
    }

    /// Phase E audit A-C1 regression: the prune predicate must run
    /// EXACTLY ONCE per memory, even when the memory is user-immune.
    /// The previous implementation re-ran the predicate on a
    /// falsified clone, causing stateful predicates to double-fire.
    #[tokio::test]
    async fn prune_predicate_runs_exactly_once_per_memory() {
        use crate::engine::embedding::MockEmbedder;
        use crate::engine::memory;
        use crate::engine::vector::HnswVectorIndex;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let h = TestHarness::in_memory();
        let dim = 4;
        // Mixed set: 2 immune + 2 uncited.
        for (id_str, cited) in [
            ("mem-immune11", true),
            ("mem-immune22", true),
            ("mem-prune001", false),
            ("mem-prune002", false),
        ] {
            let emb = MockEmbedder::new(dim).with_response(vec![vec![1.0, 0.0, 0.0, 0.0]]);
            let vi = HnswVectorIndex::new(dim);
            let mid = MemoryId::new(id_str);
            memory::insert(
                &h.ctx,
                h.storage.as_ref(),
                &emb,
                &vi,
                mid.clone(),
                "x",
                "y",
                now_t(),
            )
            .await
            .unwrap();
            if cited {
                memory::increment_citation_count(&h.ctx, h.storage.as_ref(), &mid)
                    .await
                    .unwrap();
            }
        }

        // Counting predicate: increments a shared counter per call.
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let pred: crate::engine::memory::PrunePredicate = Box::new(move |_fm| {
            counter_clone.fetch_add(1, Ordering::Relaxed);
            true
        });

        let vector_index = HnswVectorIndex::new(dim);
        let _ = memory::prune(&h.ctx, h.storage.as_ref(), &vector_index, pred)
            .await
            .unwrap();
        let call_count = counter.load(Ordering::Relaxed);
        assert_eq!(
            call_count, 4,
            "predicate must run exactly once per memory; got {call_count} calls for 4 memories"
        );
    }

    // ---------------------------------------------------------------
    // C-F4: SessionState population — Phase F manifest sections.
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn assemble_active_sections_empty_when_session_state_none() {
        let h = TestHarness::in_memory();
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            None,
            &AssembleConfig::default(),
            now_t(),
        )
        .await
        .unwrap();
        assert!(m.active_skills.is_empty());
        assert!(m.active_personas.is_empty());
        assert!(m.active_teams.is_empty());
        assert_eq!(m.assembly_stats.session_section_skips, 0);
    }

    #[tokio::test]
    async fn assemble_populates_active_skills_from_session_state() {
        use crate::engine::skills::{insert as insert_skill, SkillFrontmatter};

        let h = TestHarness::in_memory();
        let fm = SkillFrontmatter::new("formatter", "auto-format on save");
        insert_skill(&h.ctx, h.storage.as_ref(), "skl-fmt00001", fm, "body")
            .await
            .unwrap();

        let session = SessionState {
            active_skill_ids: vec!["skl-fmt00001".to_string()],
            ..SessionState::empty()
        };
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            Some(&session),
            &AssembleConfig::default(),
            now_t(),
        )
        .await
        .unwrap();
        assert_eq!(m.active_skills.len(), 1);
        assert_eq!(m.active_skills[0].id, "skl-fmt00001");
        assert_eq!(m.active_skills[0].name, "formatter");
        assert_eq!(m.assembly_stats.session_section_skips, 0);
    }

    #[tokio::test]
    async fn assemble_skips_missing_session_ids_and_counts_them() {
        let h = TestHarness::in_memory();
        let session = SessionState {
            active_skill_ids: vec!["skl-noexist1".to_string()],
            active_persona_ids: vec!["pers-noexist".to_string()],
            active_team_ids: vec!["tm-noexist".to_string()],
        };
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            Some(&session),
            &AssembleConfig::default(),
            now_t(),
        )
        .await
        .unwrap();
        assert!(m.active_skills.is_empty());
        assert!(m.active_personas.is_empty());
        assert!(m.active_teams.is_empty());
        assert_eq!(m.assembly_stats.session_section_skips, 3);
    }

    #[tokio::test]
    async fn assemble_populates_all_three_sections() {
        use crate::engine::personas::{insert as insert_persona, PersonaFrontmatter};
        use crate::engine::skills::{insert as insert_skill, SkillFrontmatter};
        use crate::engine::teams::{insert as insert_team, TeamFrontmatter};

        let h = TestHarness::in_memory();
        insert_skill(
            &h.ctx,
            h.storage.as_ref(),
            "skl-aaaa",
            SkillFrontmatter::new("skill", "desc"),
            "body",
        )
        .await
        .unwrap();
        insert_persona(
            &h.ctx,
            h.storage.as_ref(),
            "pers-aaaa",
            PersonaFrontmatter::new("pers-aaaa", "Persona", "desc"),
            "body",
        )
        .await
        .unwrap();
        insert_team(
            &h.ctx,
            h.storage.as_ref(),
            "tm-aaaa",
            TeamFrontmatter::new("tm-aaaa", "Team", "desc"),
            "body",
        )
        .await
        .unwrap();

        let session = SessionState {
            active_skill_ids: vec!["skl-aaaa".to_string()],
            active_persona_ids: vec!["pers-aaaa".to_string()],
            active_team_ids: vec!["tm-aaaa".to_string()],
        };
        let m = assemble(
            &h.ctx,
            h.storage.as_ref(),
            None,
            None,
            Some(&session),
            &AssembleConfig::default(),
            now_t(),
        )
        .await
        .unwrap();
        assert_eq!(m.active_skills.len(), 1);
        assert_eq!(m.active_personas.len(), 1);
        assert_eq!(m.active_teams.len(), 1);
    }
}
