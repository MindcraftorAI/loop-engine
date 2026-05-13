//! Sentiment-layer value types.
//!
//! Closed enums for fixed concepts (`Polarity`), `#[non_exhaustive]` enums
//! for growth concepts (`Hazard`, `AttributionMethod`, `LoadedItemKind`).
//! Three distinct confidence newtypes around `f32` with construction-time
//! clamp (audit smell S3: f32 confidences without bounds).
//!
//! Per learn-notes D9 / D10 / D11.

use std::sync::Arc;

// =====================================================================
// Polarity — closed (audit S15: never parsed from string in engine)
// =====================================================================

/// The three sentiment polarities. CLOSED — adding a fourth is a
/// breaking change by design; the design rules lock this set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Polarity {
    Positive,
    Negative,
    Neutral,
}

// =====================================================================
// Hazard — non_exhaustive (audit S6: never Vec<String>)
// =====================================================================

/// Reasons a classification carries uncertainty or risk. Used both
/// per-item (within [`ItemClassification`]) and globally (within
/// [`RawClassification::global_hazards`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Hazard {
    /// Detected sarcasm — surface polarity may be inverted.
    Sarcasm,
    /// Multiple loaded items could match the utterance; attribution is
    /// ambiguous.
    AmbiguousReferent,
    /// Hyperbolic register — signal may overstate true sentiment.
    Hyperbole,
    /// Model self-reported low confidence.
    LowConfidence,
    /// Detected potential PII / secret material — emit cautiously.
    PrivacyConcern,
    /// Classifier deemed the input out of distribution.
    OutOfDistribution,
}

// =====================================================================
// AttributionMethod — non_exhaustive; abstain is Option::None not a variant
// =====================================================================

/// Which attribution pass produced the [`Attribution`].
///
/// Audit smell S15/S16: no `Abstained` variant — abstention is
/// `Option<Attribution>::None`, not a sixth enum value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AttributionMethod {
    /// Pass 1: the utterance literally names the item (id or distinctive keyword).
    DirectMention,
    /// Pass 2: pronoun resolved against the preceding assistant turn that
    /// mentioned the item.
    PronounResolved,
    /// Pass 3: only one loaded item was recently referenced; assume it.
    Recency,
    /// Pass 4: classifier judged top-K candidates and pointed at one.
    Salience,
}

// =====================================================================
// LoadedItemKind — non_exhaustive
// =====================================================================

/// What kind of artifact a [`LoadedItem`] represents in the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LoadedItemKind {
    Lesson,
    Memory,
    Persona,
    Skill,
    Team,
}

// =====================================================================
// Confidence newtypes — three distinct types (D9)
// =====================================================================

macro_rules! impl_confidence_newtype {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        ///
        /// Wrapped `f32`, clamped to `[0.0, 1.0]` at construction. Distinct
        /// type from sibling confidence newtypes — the type signature names
        /// the meaning so callers can't accidentally pass a classifier
        /// confidence where an attribution confidence is required.
        #[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
        pub struct $name(f32);

        impl $name {
            /// Construct, clamping to `[0.0, 1.0]`.
            pub fn new(v: f32) -> Self {
                Self(v.clamp(0.0, 1.0))
            }

            pub fn value(self) -> f32 {
                self.0
            }
        }
    };
}

impl_confidence_newtype!(
    AttributionConfidence,
    "Confidence in an [`Attribution`] — set by the attribution algorithm."
);
impl_confidence_newtype!(
    ClassifierConfidence,
    "Raw confidence emitted by a [`super::SentimentClassifier`]."
);
impl_confidence_newtype!(
    CalibratedConfidence,
    "Orchestrator-calibrated confidence used for promotion thresholds (Day 16+)."
);

// =====================================================================
// LoadedItemId — Arc<str> newtype matching SessionId pattern (D11)
// =====================================================================

/// Identifier for an item loaded into the current manifest (lesson,
/// memory, persona, skill, team). `Arc<str>` newtype — cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LoadedItemId(Arc<str>);

impl LoadedItemId {
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for LoadedItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// =====================================================================
// LoadedItem — a manifest item the engine can attribute signals to
// =====================================================================

/// A manifest item the engine has loaded for the current session.
/// Sentiment attribution maps user utterances onto these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedItem {
    pub id: LoadedItemId,
    pub kind: LoadedItemKind,
    /// Short human-readable label (e.g. lesson title) for attribution
    /// keyword matching.
    pub label: String,
    /// Optional distinctive keywords the attribution algorithm may
    /// look for. The TS algorithm uses these for Pass 1 direct-mention
    /// scoring.
    pub keywords: Vec<String>,
}

// =====================================================================
// RecentTurn — recent conversation history for pronoun-anaphor (Pass 2)
// =====================================================================

/// One recent conversation turn — used by attribution Pass 2 to resolve
/// pronouns ("that worked great" → which item was the previous turn
/// about?). Generally the most recent 4-6 turns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentTurn {
    /// Whether the turn was authored by the user or the assistant.
    pub role: TurnRole,
    /// Verbatim text of the turn.
    pub text: String,
    /// Items that were active during this turn (for Pass 2's
    /// "what was the last turn about?" lookup).
    pub referenced_items: Vec<LoadedItemId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TurnRole {
    User,
    Assistant,
}

// =====================================================================
// ClassificationRequest — owned, ships across .await (OQ6)
// =====================================================================

/// Input to [`super::SentimentClassifier::classify`]. Owned (not
/// borrowed) per learn-notes OQ6: bounded size, ships across `.await`
/// trivially, aligns with the `Arc<str>` cheap-clone philosophy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationRequest {
    /// The user utterance that triggered classification.
    pub utterance: String,
    /// Manifest items eligible for attribution. Bounded ~20 items by
    /// manifest assembly upstream.
    pub loaded_items: Vec<LoadedItem>,
    /// Recent conversation context — last 4-6 turns typically.
    pub recent_turns: Vec<RecentTurn>,
}

// =====================================================================
// ItemClassification + RawClassification — classifier output shape
// =====================================================================

/// Per-item classification result.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemClassification {
    pub item_id: LoadedItemId,
    pub polarity: Polarity,
    pub confidence: ClassifierConfidence,
    /// Optional supporting evidence quoted from the utterance.
    pub evidence: Option<String>,
    pub hazards: Vec<Hazard>,
}

/// Raw classifier output. May be empty (abstain — classifier had nothing
/// confident to say about any loaded item).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RawClassification {
    pub per_item: Vec<ItemClassification>,
    pub global_hazards: Vec<Hazard>,
}

impl RawClassification {
    /// Explicit abstention constructor — the classifier had nothing
    /// confident to report. Empty `per_item` + empty `global_hazards`.
    ///
    /// Per learn-notes OQ7: explicit abstain is more readable than
    /// `RawClassification::default()`.
    pub fn abstain() -> Self {
        Self::default()
    }

    /// True when no per-item classifications and no global hazards.
    pub fn is_abstain(&self) -> bool {
        self.per_item.is_empty() && self.global_hazards.is_empty()
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidences_clamp_to_unit_interval() {
        assert_eq!(AttributionConfidence::new(-0.5).value(), 0.0);
        assert_eq!(AttributionConfidence::new(0.5).value(), 0.5);
        assert_eq!(AttributionConfidence::new(1.5).value(), 1.0);
        assert_eq!(ClassifierConfidence::new(0.85).value(), 0.85);
        assert_eq!(CalibratedConfidence::new(2.0).value(), 1.0);
    }

    #[test]
    fn confidences_are_distinct_types() {
        // This test exists to document the compile-time guarantee.
        // The following would NOT compile, which is the point:
        //   let a: AttributionConfidence = ClassifierConfidence::new(0.5);
        // Each is a separate newtype.
        let _ = AttributionConfidence::new(0.5);
        let _ = ClassifierConfidence::new(0.5);
        let _ = CalibratedConfidence::new(0.5);
    }

    #[test]
    fn loaded_item_id_round_trip() {
        let id = LoadedItemId::new("les-abc123");
        assert_eq!(id.as_str(), "les-abc123");
        let cloned = id.clone();
        assert_eq!(id, cloned);
    }

    #[test]
    fn raw_classification_abstain_is_distinguishable() {
        let abstain = RawClassification::abstain();
        assert!(abstain.is_abstain());
        let non_abstain = RawClassification {
            per_item: vec![],
            global_hazards: vec![Hazard::Sarcasm],
        };
        assert!(!non_abstain.is_abstain());
    }
}
