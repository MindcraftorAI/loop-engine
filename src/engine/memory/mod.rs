//! Memory store — raw, embedded recall fodder. Phase E.
//!
//! Distinct from lessons (the structured wedge-gated layer):
//!   - **Memory**: insert-mostly, embedded for vector search, surfaced
//!     to the LLM as raw context. No promotion gate. No causal_narrative.
//!     Engine accepts whatever the host writes — adversarial concerns
//!     (PII redaction, poisoning detection) are host-layer.
//!   - **Lesson**: structured, validated, gated, lifecycle-managed.
//!     `causal_narrative.evidence_refs` can cite a memory by `MemoryId`
//!     (typed reference via [`crate::engine::yaml::EvidenceRef::Memory`]).
//!
//! User-authored lessons that cite a memory mark it eviction-immune via
//! [`MemoryFrontmatter::consumed_by_user_lessons`] — see
//! `feedback_user_authored_lessons_immune.md` for the principle.
//!
//! Phase E C-E1 ships TYPES + SKELETON. Persistence, CRUD, search, and
//! prune semantics land in C-E2 + C-E3.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod compress;
pub(crate) mod cycle;
pub mod id;
pub mod lifecycle;
pub mod store;

pub use compress::{CompressionConfig, CompressionWindow, compress};
pub use id::MemoryId;
pub use origin::MemoryOrigin;
pub use scope::{MemoryScope, MemoryScopeFilter};

pub mod origin;
pub mod scope;
// Phase E2 audit B-M2 extraction: chase + recompute live in
// `lifecycle.rs`. Re-exported here so existing call sites continue
// to work via `memory::recompute_citation_counts` etc.
pub use lifecycle::{RecomputeStats, get_by_id_chasing_derived_from, recompute_citation_counts};
// `decrement_citation_count` is `pub(crate)` — Phase G consumes from
// within the engine; not part of the external API.
pub(crate) use store::decrement_citation_count;
pub use store::{
    RehydrateStats, delete, get_by_id, get_by_id_with_embedding, hybrid_search,
    increment_citation_count, insert, insert_scoped, insert_with_provenance, prune,
    rehydrate_vector_index, search, text_search, update,
};

/// YAML frontmatter for a memory file on disk. Mirrors
/// [`crate::engine::yaml::LessonFrontmatter`] for symmetry.
///
/// **NOT `#[non_exhaustive]`** — deliberately, per D-E1. This type
/// IS the serialized YAML shape, and `#[non_exhaustive]` would block
/// struct-literal construction from integration tests that need to
/// hand-build fixtures. Backwards-compatible growth is achieved
/// instead via `#[serde(default)]` on additive fields — TS-shaped
/// memories without a new field deserialize cleanly. The audit
/// trade-off (B-m2) accepts looser SemVer durability on this shape
/// in exchange for the test ergonomics; future cycles that need
/// stricter SemVer can revisit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFrontmatter {
    pub id: MemoryId,
    pub description: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// How many user-authored lessons cite this memory in their
    /// `evidence_refs`. When `> 0`, [`prune`](super) REFUSES to evict
    /// this memory regardless of predicate. The engine enforces this
    /// guard internally — host code can't bypass even by accident
    /// (D-E9 invariant).
    #[serde(default)]
    pub consumed_by_user_lessons: u32,
    /// Memories derived from compression of one or more raw memories.
    /// Empty for raw memories. Reserved for Phase F/G compression
    /// (D-E1 — shape ships in Phase E, consumer ships later).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derived_from: Vec<MemoryId>,
    /// Phase F D-F8: scope tag. Default `MemoryScope::User`. The
    /// user-immunity invariant is SCOPE-ORTHOGONAL (cited memories
    /// stay immune regardless of scope).
    #[serde(default)]
    pub scope: MemoryScope,
    /// Phase G D-G1 (v0.4): provenance metadata — host, session_id,
    /// model, cwd_basename, written_at. `None` for v0.3.1-era memories
    /// (the field is missing from the YAML); future cycles can promote
    /// the field to required if all live data has it. The wedge gate's
    /// `origin_diverse` signal (v0.4+) reads `session_id` to count
    /// cross-session reproducibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MemoryOrigin>,
}

impl MemoryFrontmatter {
    /// Construct a new frontmatter with required fields. `updated_at`,
    /// counters, and `derived_from` default to None/0/empty. `scope`
    /// defaults to [`MemoryScope::User`] — set via [`Self::with_scope`].
    pub fn new(id: MemoryId, description: impl Into<String>, created_at: DateTime<Utc>) -> Self {
        Self {
            id,
            description: description.into(),
            created_at: created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            updated_at: None,
            consumed_by_user_lessons: 0,
            derived_from: Vec::new(),
            scope: MemoryScope::default(),
            origin: None,
        }
    }

    /// Builder: set the [`MemoryScope`]. Phase F audit-fix close —
    /// the write half of the scope-aware manifest filter (the read
    /// half was already wired). Without this, scope can only be set
    /// by editing the on-disk YAML directly.
    #[must_use]
    pub fn with_scope(mut self, scope: MemoryScope) -> Self {
        self.scope = scope;
        self
    }

    /// Builder: set the [`MemoryOrigin`]. Phase G D-G1 (v0.4): the
    /// write half of provenance metadata. Skipped on `None` /
    /// `is_empty()` so v0.3.1 callers and hosts that detect no
    /// provenance round-trip clean.
    #[must_use]
    pub fn with_origin(mut self, origin: MemoryOrigin) -> Self {
        self.origin = if origin.is_empty() {
            None
        } else {
            Some(origin)
        };
        self
    }
}

/// In-memory view of a Memory: frontmatter + body + (optionally) the
/// embedding vector. `embedding` is `None` for "bare" loads that don't
/// consult the vector index (audit-only paths); `Some(_)` after
/// `memory::get_by_id` with `include_embedding=true` (C-E3) OR after
/// `memory::insert` returns the just-embedded value.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Memory {
    pub frontmatter: MemoryFrontmatter,
    pub content: String,
    pub embedding: Option<Vec<f32>>,
}

impl Memory {
    /// Construct from required fields. `embedding` defaults to `None`.
    pub fn new(frontmatter: MemoryFrontmatter, content: impl Into<String>) -> Self {
        Self {
            frontmatter,
            content: content.into(),
            embedding: None,
        }
    }

    /// Attach an embedding (builder).
    #[must_use]
    pub fn with_embedding(mut self, embedding: Vec<f32>) -> Self {
        self.embedding = Some(embedding);
        self
    }

    /// Phase E2 D-Cx5: `true` if this memory was produced by
    /// compression (i.e., `derived_from` is non-empty). `false` for
    /// raw memories.
    pub fn is_compressed(&self) -> bool {
        !self.frontmatter.derived_from.is_empty()
    }
}

/// Which search path produced a [`MemoryRef`]. v0.5 hybrid-recall
/// addition: when the hybrid path runs, a memory can surface from
/// the semantic neighborhood, the text-match scan, or both. Callers
/// (CLI / opensquid recall preview) can render this for transparency.
/// `None` on refs from pre-v0.5 code paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HitSource {
    /// Vector-index nearest-neighbor lookup (`memory::search`).
    Semantic,
    /// Text-match scan (`memory::text_search`, scores via
    /// [`crate::engine::scoring::score_text_match`]).
    Text,
    /// Surfaced from BOTH paths — strongest signal; the v0.5 hybrid
    /// RRF gives these refs a dual-source score boost.
    Both,
}

/// Trimmed view of a memory for the manifest's `memories` section.
/// Mirrors [`crate::engine::manifest::ActiveLesson`] trim pattern —
/// caller gets enough to render + an ID to fetch the full Memory on
/// demand.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct MemoryRef {
    pub id: MemoryId,
    pub description: String,
    pub body_preview: String,
    /// Similarity score, range `[0.0, 1.0]`. Interpretation depends
    /// on `source`:
    /// - `Semantic` → cosine similarity from the embedder.
    /// - `Text` → token-overlap + substring score from
    ///   [`crate::engine::scoring::score_text_match`].
    /// - `Both` → RRF-fused score (sum of `1/(60+rank)` from each
    ///   source); not directly comparable to single-source scores
    ///   but always strictly higher than either alone.
    pub similarity: f32,
    /// Which search path produced this ref (v0.5 hybrid addition).
    /// `None` for pre-v0.5 callers that don't set it. JSON-serialized
    /// by serve.rs handlers (memory.search response) using snake_case
    /// variant names; skipped when None.
    pub source: Option<HitSource>,
}

/// Query driver for the manifest's memory section. Set on
/// [`crate::engine::manifest::AssembleConfig::memory_query`].
///
/// `#[non_exhaustive]` — `LessonContext` lands in Phase F when skill
/// content provides session-level query material.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum MemoryQuery {
    /// Caller-supplied text. Engine embeds via the supplied
    /// [`crate::engine::Embedder`] before searching.
    Text(String),
    /// Caller pre-embedded the query. Engine skips the embedder call.
    Vector(Vec<f32>),
}

/// Predicate type for [`prune`](super). Boxed + Send + Sync so the
/// engine can iterate memories on a worker task.
pub type PrunePredicate = Box<dyn Fn(&MemoryFrontmatter) -> bool + Send + Sync>;

/// Result of a [`prune`](super) sweep. `skipped_user_immune` is the
/// load-bearing audit signal — when host-supplied predicates would
/// have matched but the user-immunity counter blocked the eviction.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PruneStats {
    pub examined: usize,
    pub pruned: usize,
    /// Memories the predicate matched but the engine refused to evict
    /// because `consumed_by_user_lessons > 0`. Host can observe these
    /// to surface "your prune would have removed N user-cited
    /// memories — manually retire the citing lessons first."
    pub skipped_user_immune: usize,
}

/// Internal helper used by C-E3 — wraps user-supplied predicates with
/// the engine-enforced user-lesson-immunity guard. Public for
/// crate-internal modules; not part of the public API. C-E1 ships the
/// helper + its test so the immunity invariant is locked from
/// commit-one; C-E3 wires it into `prune`.
#[allow(dead_code)] // Consumed by C-E3's prune impl
pub(crate) fn guarded_predicate(
    user: PrunePredicate,
) -> impl Fn(&MemoryFrontmatter) -> bool + Send + Sync {
    move |fm: &MemoryFrontmatter| user(fm) && fm.consumed_by_user_lessons == 0
}

// Test-only Arc<str> hash key helper to silence the unused-import
// warning on the early skeleton — removed in C-E3 when the store
// API consumes it.
#[allow(dead_code)]
fn _silence_arc_str(_x: Arc<str>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_frontmatter_new_populates_defaults() {
        let id = MemoryId::new("mem-abc12345");
        let created = "2026-05-14T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let fm = MemoryFrontmatter::new(id.clone(), "test memory", created);
        assert_eq!(fm.id, id);
        assert_eq!(fm.description, "test memory");
        assert!(fm.created_at.contains("2026-05-14"));
        assert!(fm.updated_at.is_none());
        assert_eq!(fm.consumed_by_user_lessons, 0);
        assert!(fm.derived_from.is_empty());
    }

    #[test]
    fn memory_new_no_embedding_by_default() {
        let fm = MemoryFrontmatter::new(MemoryId::new("mem-zzzzzzzz"), "x", Utc::now());
        let m = Memory::new(fm, "body");
        assert_eq!(m.content, "body");
        assert!(m.embedding.is_none());
    }

    #[test]
    fn memory_with_embedding_builder() {
        let fm = MemoryFrontmatter::new(MemoryId::new("mem-zzzzzzzz"), "x", Utc::now());
        let m = Memory::new(fm, "body").with_embedding(vec![0.1, 0.2, 0.3]);
        assert_eq!(m.embedding.as_deref().map(|v| v.len()), Some(3));
    }

    #[test]
    fn guarded_predicate_blocks_user_immune_memories() {
        let user_pred: PrunePredicate = Box::new(|_fm: &MemoryFrontmatter| true);
        let guarded = guarded_predicate(user_pred);
        let id = MemoryId::new("mem-aaaaaaaa");
        let now = Utc::now();
        let immune = MemoryFrontmatter {
            consumed_by_user_lessons: 1,
            ..MemoryFrontmatter::new(id.clone(), "immune", now)
        };
        let prunable = MemoryFrontmatter::new(id, "prunable", now);
        assert!(!guarded(&immune), "user-immune memory must be skipped");
        assert!(guarded(&prunable), "uncited memory must be prunable");
    }

    #[test]
    fn memory_frontmatter_with_scope_builder_overrides_default() {
        let fm = MemoryFrontmatter::new(MemoryId::new("mem-scope0001"), "x", Utc::now())
            .with_scope(MemoryScope::Team("team-eng".into()));
        assert_eq!(fm.scope, MemoryScope::Team("team-eng".into()));
    }

    /// Phase F audit-fix close A-M7: a legacy YAML without the
    /// `scope` field MUST deserialize cleanly and default to
    /// `MemoryScope::User`. Validates the `#[serde(default)]` chain
    /// for the back-compat guarantee called out in the docstring.
    #[test]
    fn memory_frontmatter_legacy_yaml_without_scope_defaults_to_user() {
        let yaml = r#"
id: mem-legacy001
description: legacy memory
created_at: "2026-05-14T00:00:00.000Z"
consumed_by_user_lessons: 0
"#;
        let fm: MemoryFrontmatter = serde_yml::from_str(yaml).unwrap();
        assert_eq!(fm.scope, MemoryScope::User);
        assert_eq!(fm.id.as_str(), "mem-legacy001");
        assert_eq!(fm.consumed_by_user_lessons, 0);
    }

    #[test]
    fn prune_stats_eq() {
        let a = PruneStats {
            examined: 10,
            pruned: 3,
            skipped_user_immune: 2,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }
}
