//! Crate-level engine error type.
//!
//! Per Day 16b D5: typed `EngineError` replaces `anyhow::Error` in
//! engine public function returns. Legacy modules (Day 11/12 lessons
//! sync API, lifecycle, pid, buffer) keep `anyhow` for now — the
//! migration is per-module and incremental. New async APIs introduced
//! Day 16b+ use `EngineError`.
//!
//! `#[non_exhaustive]` — variants will grow. No `Clone` impl per
//! OQ-D16b-E (`io::Error` and `StorageError` are not `Clone`).

use std::io;

use thiserror::Error;

use crate::engine::embedding::error::EmbeddingError;
use crate::engine::lessons::gate::BlockReason;
use crate::engine::llm::error::LlmError;
use crate::engine::storage::StorageError;
use crate::engine::vector::error::VectorIndexError;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EngineError {
    /// A lesson lookup failed because no lesson with that id exists in
    /// any status directory.
    #[error("lesson not found: {id}")]
    LessonNotFound { id: String },

    /// Storage backend returned an error.
    #[error("storage error: {0}")]
    Storage(#[source] StorageError),

    /// YAML parse or serialization error. Boxed because the YAML stack
    /// has multiple error types in play (`serde_yml`, our purpose-built
    /// `engine::yaml::reader`). OQ-D16b-A: no typed YAML variants.
    #[error("yaml error: {0}")]
    Yaml(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Generic parse / shape error — body content didn't match the
    /// expected structure.
    #[error("parse error: {0}")]
    Parse(String),

    /// Compare-and-swap retry budget exhausted. `retries` is the count
    /// of failed attempts before giving up.
    #[error("CAS contended on {key} after {retries} retries")]
    CasContended { key: String, retries: u32 },

    /// Underlying I/O error.
    #[error("io error: {0}")]
    Io(#[source] io::Error),

    /// Manifest assembly received an invalid status filter — either
    /// an empty `statuses: vec![]` (Phase C-C1) or a string that
    /// doesn't parse to a known [`crate::engine::yaml::LessonStatus`].
    /// Caller-side validation error.
    #[error("manifest invalid status: {status}")]
    ManifestInvalidStatus { status: String },

    /// LLM call failure surfaced from any [`crate::engine::llm::LlmClient`]
    /// adapter. Phase D D-D4: typed enum for caller pattern-matching
    /// (rate-limit vs invalid-output vs validation-failed all matter
    /// to engine code).
    #[error("llm error: {0}")]
    Llm(#[source] LlmError),

    /// Embedding call failure. Phase D / E surface.
    #[error("embedding error: {0}")]
    Embedding(#[source] EmbeddingError),

    /// Vector index call failure. Phase E surface.
    #[error("vector index error: {0}")]
    VectorIndex(#[source] VectorIndexError),

    /// Engine-initiated delete refused because the memory is cited
    /// by one or more user-authored lessons — the user-immunity
    /// invariant from
    /// `feedback_user_authored_lessons_immune.md`. Surfaces from
    /// `memory::delete(..., force = false)`. Engine-initiated paths
    /// (auto-cleanup, TTL sweep) MUST pass `force = false` and
    /// handle this error. User-initiated paths (explicit "forget")
    /// pass `force = true` to bypass.
    #[error(
        "user-memory immune: memory {id} is cited by {cited_by} user-authored lesson(s) — \
         use force=true to bypass"
    )]
    UserMemoryImmune { id: String, cited_by: u32 },

    /// Phase E2 compression: LLM returned the refusal sentinel
    /// `{"error": "insufficient_input"}` — the input window was too
    /// thin to compose a meaningful summary. Distinct from
    /// `Llm(LlmError::ValidationFailed)` (which is a defect) — this
    /// is a graceful no-op signal.
    #[error("compression refused: insufficient input quality to compose a summary")]
    CompressionInsufficientInput,

    /// Phase E2 compression: cycle or depth-exceeded in the
    /// `derived_from` chain. Includes the chain that triggered the
    /// detection (last 16 ids visited) for debugging.
    #[error("compression cycle or depth exceeded: chain {chain:?}")]
    CompressionCycle { chain: Vec<String> },

    /// Phase E2: delete refused because the memory is referenced as
    /// a predecessor in some compressed memory's `derived_from` AND
    /// that compressed memory has been deleted, leaving the citation
    /// chain in a state where deleting this id would orphan a
    /// user-authored lesson's evidence. Rare audit-trail integrity
    /// case; surfaces during recompute drift checks too.
    #[error(
        "delete would orphan citation chain: memory {id} is a derived_from predecessor in \
         chain(s) with missing successor(s): {in_chain_with:?}"
    )]
    CompressionWouldOrphanCitation {
        id: String,
        in_chain_with: Vec<String>,
    },

    /// Phase F: engine-initiated archive/delete refused because the
    /// skill is user-authored (cascades from
    /// `feedback_user_authored_lessons_immune.md` — only the user
    /// can retire a user-authored skill).
    #[error(
        "user-skill immune: skill {id} is user-authored — use force=true to bypass"
    )]
    UserSkillImmune { id: String },

    /// Phase F: engine-initiated archive/delete refused — user-authored persona.
    #[error("user-persona immune: persona {id} is user-authored — use force=true to bypass")]
    UserPersonaImmune { id: String },

    /// Phase F: engine-initiated archive/delete refused — user-authored team.
    #[error("user-team immune: team {id} is user-authored — use force=true to bypass")]
    UserTeamImmune { id: String },

    /// Phase F + Phase E2: compression refused because predecessors
    /// have mixed `MemoryScope` values — crossing a scope boundary
    /// in a compressed memory would violate the privacy invariant.
    #[error("compression scope mismatch: predecessors span multiple scopes: {window:?}")]
    CompressionScopeMismatch { window: Vec<String> },

    /// `narrative::generate` rejected the LLM output as too thin to
    /// ground (the model returned a refusal indicating the inputs
    /// don't justify any concrete causal narrative). Distinct from
    /// `Llm(LlmError::ValidationFailed)` because there's nothing
    /// wrong with the LLM — there's nothing to say.
    #[error("narrative refused: insufficient context to ground a causal narrative")]
    NarrativeInsufficientContext,

    /// Promotion gate blocked the requested promotion. Added preemptively
    /// in Phase B C-B2 so Phase G `transitions::promote` has a typed
    /// failure to raise. The gate itself returns a `GateDecision` rather
    /// than this error — the error is wrapped at the transition layer
    /// when "must promote" is a precondition.
    ///
    /// The Display string enumerates each reason via
    /// [`BlockReason`]'s `Display` impl, separated by `; `. CLIs can
    /// scrape it or pattern-match the `reasons` field directly for
    /// structured access.
    #[error("promotion blocked: {}", format_reasons(.reasons))]
    PromotionBlocked { reasons: Vec<BlockReason> },

    /// Genuinely uncategorized engine-level error. Use sparingly — adding
    /// a named variant is preferred when the error class repeats.
    #[error("engine error: {0}")]
    Other(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl EngineError {
    pub fn yaml<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Yaml(Box::new(err))
    }

    pub fn other<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Other(Box::new(err))
    }
}

impl From<StorageError> for EngineError {
    fn from(err: StorageError) -> Self {
        Self::Storage(err)
    }
}

/// Render a slice of [`BlockReason`]s as `"reason1; reason2; ..."`.
/// Used by `EngineError::PromotionBlocked`'s thiserror format string.
fn format_reasons(reasons: &[BlockReason]) -> String {
    reasons
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

impl From<io::Error> for EngineError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_includes_kind_and_payload() {
        let err = EngineError::LessonNotFound {
            id: "les-abc".into(),
        };
        let s = format!("{err}");
        assert!(s.contains("lesson not found"));
        assert!(s.contains("les-abc"));
    }

    #[test]
    fn storage_error_converts_via_from() {
        let storage_err = StorageError::NotFound {
            key: "lessons/active/x.md".into(),
        };
        let engine_err: EngineError = storage_err.into();
        assert!(matches!(engine_err, EngineError::Storage(_)));
    }

    #[test]
    fn io_error_converts_via_from() {
        let io_err = io::Error::new(io::ErrorKind::PermissionDenied, "boom");
        let engine_err: EngineError = io_err.into();
        assert!(matches!(engine_err, EngineError::Io(_)));
    }

    #[test]
    fn cas_contended_carries_key_and_retries() {
        let err = EngineError::CasContended {
            key: "lessons/active/x.md".into(),
            retries: 5,
        };
        let s = format!("{err}");
        assert!(s.contains("CAS contended"));
        assert!(s.contains("5"));
    }

    #[test]
    fn yaml_constructor_boxes_arbitrary_error() {
        let inner = io::Error::other("yaml-ish");
        let err = EngineError::yaml(inner);
        assert!(matches!(err, EngineError::Yaml(_)));
    }

    #[test]
    fn promotion_blocked_display_enumerates_each_reason() {
        let err = EngineError::PromotionBlocked {
            reasons: vec![
                BlockReason::MissingCausalNarrative,
                BlockReason::ThumbsDownBlock { count: 2 },
            ],
        };
        let s = format!("{err}");
        assert!(s.contains("missing-causal-narrative"), "got: {s}");
        assert!(s.contains("thumbs-down-block: count=2"), "got: {s}");
        assert!(s.contains("; "), "expected '; ' separator, got: {s}");
    }

    #[test]
    fn promotion_blocked_display_empty_reasons_renders_cleanly() {
        // Empty Vec is constructable (PromotionBlocked is public); the
        // gate would never produce this, but Display must not panic.
        let err = EngineError::PromotionBlocked { reasons: vec![] };
        let _ = format!("{err}"); // must not panic
    }
}
