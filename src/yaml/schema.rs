//! Lesson frontmatter schema — mirrors the TS-side
//! `LessonFrontmatter` in `core/src/types/index.ts` exactly.
//!
//! Field order matches TS's LOAD-path order in `core/src/lessons/loader.ts`
//! `tryLoadLessonFile` because that's the order TS emits after any
//! read-modify-write cycle. Audit Day 11 finding A1: the daemon must emit
//! in this order to keep git diffs stable across cross-process mutations.

use serde::{Deserialize, Serialize};

/// 5-status lifecycle from ADR-0010. String-encoded in frontmatter
/// (`status: active` etc); the daemon trusts the file path more than
/// this field for correctness (status = parent dir name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LessonStatus {
    Pending,
    Active,
    Promoted,
    Discarded,
    Superseded,
}

impl LessonStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Promoted => "promoted",
            Self::Discarded => "discarded",
            Self::Superseded => "superseded",
        }
    }
}

/// Confidence ladder. `observed` requires non-empty `evidence_refs`
/// (enforced by the gate + ingest validation on the TS side).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Observed,
    Inferred,
    Speculative,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Inferred => "inferred",
            Self::Speculative => "speculative",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GeneratedBy {
    User,
    Llm,
}

impl GeneratedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Llm => "llm",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalNarrative {
    pub trigger: String,
    pub failure_mode: String,
    pub correction: String,
    pub confidence: Confidence,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    pub generated_by: GeneratedBy,
    pub generated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestSourceType {
    AutoMemory,
    AutoDreamSignal,
    EccInstinct,
    LearningsMd,
}

impl IngestSourceType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AutoMemory => "auto_memory",
            Self::AutoDreamSignal => "auto_dream_signal",
            Self::EccInstinct => "ecc_instinct",
            Self::LearningsMd => "learnings_md",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestProvenance {
    pub source_type: IngestSourceType,
    pub source_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_external_id: Option<String>,
    pub extracted_at: String,
}

/// Full lesson frontmatter. Field order matches TS load-path emit order
/// in `core/src/lessons/loader.ts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LessonFrontmatter {
    // Always-present core (load-path appends these unconditionally)
    pub id: String,
    pub description: String,
    pub status: LessonStatus,
    pub created_at: String,

    // Conditional block 1: narrative + skill + feedback
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causal_narrative: Option<CausalNarrative>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_skill: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_feedback_ids: Option<Vec<i64>>,

    // Counters + signal sources
    #[serde(default)]
    pub applied_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_applied_at: Option<String>,
    #[serde(default)]
    pub thumbs_up_count: u64,
    #[serde(default)]
    pub thumbs_down_count: u64,
    #[serde(default)]
    pub external_signal_sources: Vec<String>,

    // Promotion + supersession
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion_eligible_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_at: Option<String>,

    // Ingest provenance (Day 2 addition)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingest_provenance: Option<IngestProvenance>,

    // Always last
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}
