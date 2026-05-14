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

pub mod id;
pub mod store;

pub use id::MemoryId;
// `decrement_citation_count` is `pub(crate)` — Phase G consumes from
// within the engine; not part of the external API.
pub use store::{
    delete, get_by_id, get_by_id_with_embedding, increment_citation_count, insert, prune,
    recompute_citation_counts, search, RecomputeStats,
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
}

impl MemoryFrontmatter {
    /// Construct a new frontmatter with required fields. `updated_at`,
    /// counters, and `derived_from` default to None/0/empty.
    pub fn new(
        id: MemoryId,
        description: impl Into<String>,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            description: description.into(),
            created_at: created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            updated_at: None,
            consumed_by_user_lessons: 0,
            derived_from: Vec::new(),
        }
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
    /// Cosine similarity to the manifest query, range [0.0, 1.0].
    pub similarity: f32,
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
        let fm = MemoryFrontmatter::new(
            MemoryId::new("mem-zzzzzzzz"),
            "x",
            Utc::now(),
        );
        let m = Memory::new(fm, "body");
        assert_eq!(m.content, "body");
        assert!(m.embedding.is_none());
    }

    #[test]
    fn memory_with_embedding_builder() {
        let fm = MemoryFrontmatter::new(
            MemoryId::new("mem-zzzzzzzz"),
            "x",
            Utc::now(),
        );
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
