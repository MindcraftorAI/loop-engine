//! Lesson frontmatter schema — mirrors the TS-side
//! `LessonFrontmatter` in `core/src/types/index.ts` exactly.
//!
//! Field order matches TS's LOAD-path order in `core/src/lessons/loader.ts`
//! `tryLoadLessonFile` because that's the order TS emits after any
//! read-modify-write cycle. Audit Day 11 finding A1: the daemon must emit
//! in this order to keep git diffs stable across cross-process mutations.

use std::fmt;

use serde::de::{Error as DeError, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::engine::memory::MemoryId;

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
    /// Phase E D-E10: typed evidence references. Reads accept BOTH the
    /// legacy `Vec<String>` form (each string wraps as
    /// `EvidenceRef::Quote(_)`) AND the typed tagged form (`{quote:
    /// ...}` or `{memory: mem-id}`). Writes always emit the typed
    /// form. After one load+save cycle, all on-disk lessons converge.
    #[serde(default)]
    pub evidence_refs: Vec<EvidenceRef>,
    pub generated_by: GeneratedBy,
    pub generated_at: String,
}

/// One element of [`CausalNarrative::evidence_refs`]. Phase E D-E10
/// makes evidence typed so a user-authored lesson can cite a
/// [`MemoryId`] directly — enabling the engine-enforced user-immunity
/// counter on memories.
///
/// **Serialization**: tagged form (`{quote: "..."}` / `{memory:
/// "mem-..."}`). NOT `#[serde(untagged)]` — Phase D audit A-M4 burned
/// us on untagged silent variant-selection.
///
/// **Deserialization**: custom impl accepts both plain strings (legacy
/// TS-shaped lessons) AND the tagged form. Plain strings wrap as
/// `Quote(_)`. See [`EvidenceRef`]'s `Deserialize` impl.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EvidenceRef {
    /// Free-text quote. Legacy form. What Phase D's narrative
    /// generation produces (the LLM emits strings; the parser wraps
    /// each as `Quote(_)`).
    Quote(String),
    /// Typed reference into the memory store. Engine resolves via
    /// [`crate::engine::memory::get_by_id`]. Citation increments the
    /// memory's user-immunity counter when the lesson is
    /// user-authored.
    Memory(MemoryId),
}

impl<'de> Deserialize<'de> for EvidenceRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct EvidenceRefVisitor;

        impl<'de> Visitor<'de> for EvidenceRefVisitor {
            type Value = EvidenceRef;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "a string (legacy quote form) or a map with single key \
                     `quote` or `memory`",
                )
            }

            fn visit_str<E: DeError>(self, v: &str) -> Result<EvidenceRef, E> {
                Ok(EvidenceRef::Quote(v.to_string()))
            }

            fn visit_string<E: DeError>(self, v: String) -> Result<EvidenceRef, E> {
                Ok(EvidenceRef::Quote(v))
            }

            fn visit_map<A>(self, mut map: A) -> Result<EvidenceRef, A::Error>
            where
                A: MapAccess<'de>,
            {
                let key: String = map.next_key()?.ok_or_else(|| {
                    A::Error::invalid_length(
                        0,
                        &"map with exactly one key (quote|memory)",
                    )
                })?;
                match key.as_str() {
                    "quote" => {
                        let v: String = map.next_value()?;
                        Ok(EvidenceRef::Quote(v))
                    }
                    "memory" => {
                        let v: String = map.next_value()?;
                        Ok(EvidenceRef::Memory(MemoryId::new(v)))
                    }
                    other => Err(A::Error::unknown_variant(other, &["quote", "memory"])),
                }
            }
        }

        deserializer.deserialize_any(EvidenceRefVisitor)
    }
}

impl EvidenceRef {
    /// Return the underlying string representation regardless of
    /// variant. Used by Phase D `narrative::validate_invariants` for
    /// char-count caps + Phase B `gate::check_promotion_gate` for the
    /// empty-evidence check.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Quote(s) => s.as_str(),
            Self::Memory(id) => id.as_str(),
        }
    }

    /// True when this ref is a typed `MemoryId` (not a free-text quote).
    /// Used by Phase E's citation-counter increment hook.
    pub fn is_memory(&self) -> bool {
        matches!(self, Self::Memory(_))
    }

    /// If this is a memory ref, return the underlying `MemoryId`.
    pub fn as_memory_id(&self) -> Option<&MemoryId> {
        match self {
            Self::Memory(id) => Some(id),
            Self::Quote(_) => None,
        }
    }
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

/// Phase E D-E11: who authored this lesson? The load-bearing variant
/// is `User` — user-authored lessons are eviction-immune from any
/// engine-initiated cleanup path (see
/// `feedback_user_authored_lessons_immune.md`).
///
/// `#[non_exhaustive]` + default = `Llm` for backwards-compat with
/// TS-shaped lessons predating this field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Authorship {
    /// User explicitly authored / endorsed this lesson. Eviction-
    /// immune. Citing memories increments their immunity counter.
    User,
    /// LLM-generated (Phase D `narrative::generate` etc). Default for
    /// legacy / TS-shaped lessons missing the field.
    #[default]
    Llm,
    /// Captured by the auto-memory ingest pipeline.
    AutoMemory,
    /// Imported from an ECC instinct file.
    EccInstinct,
    /// Authorship unknown — explicit placeholder. Engine never
    /// produces this; accepted on input for explicit-unknown YAML.
    Unknown,
}

impl Authorship {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Llm => "llm",
            Self::AutoMemory => "auto_memory",
            Self::EccInstinct => "ecc_instinct",
            Self::Unknown => "unknown",
        }
    }

    /// True when authorship is user-driven — triggers the eviction-
    /// immunity invariant.
    pub fn is_user(self) -> bool {
        matches!(self, Self::User)
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

    // Phase E D-E11 addition: authorship of this lesson. User-authored
    // lessons are eviction-immune. Default `Llm` for back-compat with
    // TS-shaped lessons predating this field.
    #[serde(default)]
    pub authored_by: Authorship,

    // Always last
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- EvidenceRef migrate-on-load + tagged-write round-trip ----

    #[test]
    fn evidence_ref_deserialize_legacy_plain_string_becomes_quote() {
        let json = r#""some free-text quote""#;
        let r: EvidenceRef = serde_json::from_str(json).unwrap();
        match r {
            EvidenceRef::Quote(s) => assert_eq!(s, "some free-text quote"),
            other => panic!("expected Quote variant, got {other:?}"),
        }
    }

    #[test]
    fn evidence_ref_deserialize_tagged_quote_form() {
        let json = r#"{"quote": "tagged quote text"}"#;
        let r: EvidenceRef = serde_json::from_str(json).unwrap();
        match r {
            EvidenceRef::Quote(s) => assert_eq!(s, "tagged quote text"),
            other => panic!("expected Quote variant, got {other:?}"),
        }
    }

    #[test]
    fn evidence_ref_deserialize_tagged_memory_form() {
        let json = r#"{"memory": "mem-aaaaaaaa"}"#;
        let r: EvidenceRef = serde_json::from_str(json).unwrap();
        match r {
            EvidenceRef::Memory(id) => assert_eq!(id.as_str(), "mem-aaaaaaaa"),
            other => panic!("expected Memory variant, got {other:?}"),
        }
    }

    #[test]
    fn evidence_ref_serialize_quote_emits_tagged_form() {
        let r = EvidenceRef::Quote("hello".to_string());
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"quote":"hello"}"#);
    }

    #[test]
    fn evidence_ref_serialize_memory_emits_tagged_form() {
        let r = EvidenceRef::Memory(crate::engine::memory::MemoryId::new("mem-abc12345"));
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"memory":"mem-abc12345"}"#);
    }

    #[test]
    fn evidence_ref_round_trip_quote() {
        let original = EvidenceRef::Quote("a quote with \"nested\" marks".to_string());
        let s = serde_json::to_string(&original).unwrap();
        let back: EvidenceRef = serde_json::from_str(&s).unwrap();
        assert_eq!(original, back);
    }

    #[test]
    fn evidence_ref_round_trip_memory() {
        let original =
            EvidenceRef::Memory(crate::engine::memory::MemoryId::new("mem-roundtrip"));
        let s = serde_json::to_string(&original).unwrap();
        let back: EvidenceRef = serde_json::from_str(&s).unwrap();
        assert_eq!(original, back);
    }

    #[test]
    fn evidence_ref_deserialize_unknown_tag_errors() {
        let json = r#"{"unknown_variant": "value"}"#;
        let r: Result<EvidenceRef, _> = serde_json::from_str(json);
        assert!(r.is_err(), "unknown variant tag must reject");
    }

    #[test]
    fn evidence_ref_legacy_vec_strings_in_causal_narrative() {
        // The migrate-on-load path: a TS-shaped lesson with
        // `evidence_refs: ["quote1", "quote2"]` must deserialize
        // cleanly as Vec<EvidenceRef> with each string wrapped as
        // Quote(_).
        let yaml = r#"
trigger: "t"
failure_mode: "fm"
correction: "c"
confidence: "inferred"
evidence_refs:
  - "first quote"
  - "second quote"
generated_by: "llm"
generated_at: "2026-05-14T00:00:00Z"
"#;
        let cn: CausalNarrative = serde_yml::from_str(yaml).unwrap();
        assert_eq!(cn.evidence_refs.len(), 2);
        assert_eq!(cn.evidence_refs[0], EvidenceRef::Quote("first quote".into()));
        assert_eq!(cn.evidence_refs[1], EvidenceRef::Quote("second quote".into()));
    }

    #[test]
    fn evidence_ref_as_str_works_for_both_variants() {
        let q = EvidenceRef::Quote("hi".to_string());
        let m = EvidenceRef::Memory(crate::engine::memory::MemoryId::new("mem-x"));
        assert_eq!(q.as_str(), "hi");
        assert_eq!(m.as_str(), "mem-x");
    }

    #[test]
    fn evidence_ref_helpers_is_memory_and_as_memory_id() {
        let q = EvidenceRef::Quote("x".into());
        let m = EvidenceRef::Memory(crate::engine::memory::MemoryId::new("mem-a"));
        assert!(!q.is_memory());
        assert!(m.is_memory());
        assert!(q.as_memory_id().is_none());
        assert_eq!(m.as_memory_id().map(|i| i.as_str()), Some("mem-a"));
    }

    // ---- Authorship default + serde ----

    #[test]
    fn authorship_default_is_llm() {
        assert_eq!(Authorship::default(), Authorship::Llm);
    }

    #[test]
    fn authorship_is_user_predicate() {
        assert!(Authorship::User.is_user());
        assert!(!Authorship::Llm.is_user());
        assert!(!Authorship::AutoMemory.is_user());
        assert!(!Authorship::EccInstinct.is_user());
        assert!(!Authorship::Unknown.is_user());
    }

    #[test]
    fn authorship_serde_uses_snake_case() {
        let cases = [
            (Authorship::User, r#""user""#),
            (Authorship::Llm, r#""llm""#),
            (Authorship::AutoMemory, r#""auto_memory""#),
            (Authorship::EccInstinct, r#""ecc_instinct""#),
            (Authorship::Unknown, r#""unknown""#),
        ];
        for (variant, expected) in cases {
            let s = serde_json::to_string(&variant).unwrap();
            assert_eq!(s, expected, "serialize {variant:?}");
            let back: Authorship = serde_json::from_str(expected).unwrap();
            assert_eq!(back, variant, "deserialize {expected}");
        }
    }

    #[test]
    fn authorship_as_str_matches_serde_form() {
        assert_eq!(Authorship::User.as_str(), "user");
        assert_eq!(Authorship::Llm.as_str(), "llm");
        assert_eq!(Authorship::AutoMemory.as_str(), "auto_memory");
        assert_eq!(Authorship::EccInstinct.as_str(), "ecc_instinct");
        assert_eq!(Authorship::Unknown.as_str(), "unknown");
    }

    #[test]
    fn lesson_frontmatter_default_authored_by_when_field_missing() {
        // TS-shaped lessons predate `authored_by`. `#[serde(default)]`
        // should fall back to `Authorship::Llm`.
        let yaml = r#"
id: "les-test1234"
description: "test"
status: "active"
created_at: "2026-05-14T00:00:00Z"
"#;
        let fm: LessonFrontmatter = serde_yml::from_str(yaml).unwrap();
        assert_eq!(fm.authored_by, Authorship::Llm);
    }
}
