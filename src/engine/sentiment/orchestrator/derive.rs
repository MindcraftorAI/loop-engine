//! Pure-function signal derivation — threshold + hazard auto-abstain
//! + attribution cross-check + rate limit.
//!
//! Caller (orchestrator handler) supplies the rate-limit map and `now`.
//! Returns `(emitted_signals, per_item_abstentions)`.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use chrono::Utc;

use crate::engine::sentiment::attribution::attribute_signal;
use crate::engine::sentiment::signals::{AbstainReason, SentimentSignal};
use crate::engine::sentiment::types::{
    CalibratedConfidence, ClassificationRequest, Hazard, LoadedItemId, Polarity, RawClassification,
};

use super::config::OrchestratorConfig;

/// Minimum classifier confidence for a positive signal to emit.
/// Cited to `sentiment-design-rules.md` rule 5.
pub(super) const POSITIVE_MIN: f32 = 0.75;
/// Minimum classifier confidence for a negative signal to emit.
/// Asymmetric — negatives carry higher friction risk; want more certainty.
pub(super) const NEGATIVE_MIN: f32 = 0.85;

/// Auto-abstain hazards (Day 16a D9): Sarcasm | AmbiguousReferent |
/// OutOfDistribution | SelfDirected.
pub(super) const fn is_auto_abstain_hazard(h: Hazard) -> bool {
    matches!(
        h,
        Hazard::Sarcasm
            | Hazard::AmbiguousReferent
            | Hazard::OutOfDistribution
            | Hazard::SelfDirected
    )
}

/// Outcome of [`derive_signals`]: emitted signals + per-item abstentions.
pub(super) struct DeriveOutcome {
    pub signals: Vec<SentimentSignal>,
    pub abstentions: Vec<(Option<LoadedItemId>, AbstainReason)>,
}

pub(super) fn derive_signals(
    raw: &RawClassification,
    request: &ClassificationRequest,
    rate_limit: &HashMap<LoadedItemId, Instant>,
    config: &OrchestratorConfig,
    now: Instant,
    source_event_uuid: &str,
) -> DeriveOutcome {
    let mut signals: Vec<SentimentSignal> = Vec::with_capacity(1);
    let mut abstentions: Vec<(Option<LoadedItemId>, AbstainReason)> = Vec::new();
    let mut seen: HashSet<LoadedItemId> = HashSet::new();

    if raw.is_abstain() {
        abstentions.push((None, AbstainReason::ClassifierAbstained));
        return DeriveOutcome {
            signals,
            abstentions,
        };
    }

    for item in &raw.per_item {
        if seen.contains(&item.item_id) {
            continue;
        }
        seen.insert(item.item_id.clone());

        let threshold = match item.polarity {
            Polarity::Positive => POSITIVE_MIN,
            Polarity::Negative => NEGATIVE_MIN,
            Polarity::Neutral => {
                abstentions.push((Some(item.item_id.clone()), AbstainReason::Neutral));
                continue;
            }
        };
        if item.confidence.value() < threshold {
            abstentions.push((
                Some(item.item_id.clone()),
                AbstainReason::BelowThreshold {
                    polarity: item.polarity,
                    observed: item.confidence.value(),
                    required: threshold,
                },
            ));
            continue;
        }

        let mut auto_abstain: Option<Hazard> = None;
        for h in item
            .hazards
            .iter()
            .chain(raw.global_hazards.iter())
            .copied()
        {
            if is_auto_abstain_hazard(h) {
                auto_abstain = Some(h);
                break;
            }
        }
        if let Some(h) = auto_abstain {
            abstentions.push((Some(item.item_id.clone()), AbstainReason::HazardSet(h)));
            continue;
        }

        let utterance = item.evidence.as_deref().unwrap_or(&request.utterance);
        let attr = attribute_signal(utterance, &request.loaded_items, &request.recent_turns);
        let Some(attr) = attr else {
            abstentions.push((
                Some(item.item_id.clone()),
                AbstainReason::AttributionAbstained,
            ));
            continue;
        };
        if attr.item_id != item.item_id {
            abstentions.push((
                Some(item.item_id.clone()),
                AbstainReason::AttributionMismatch,
            ));
            continue;
        }

        if let Some(&last) = rate_limit.get(&item.item_id)
            && now.duration_since(last) < config.per_lesson_cooldown
        {
            abstentions.push((Some(item.item_id.clone()), AbstainReason::RateLimited));
            continue;
        }

        signals.push(SentimentSignal {
            item_id: item.item_id.clone(),
            polarity: item.polarity,
            calibrated_confidence: CalibratedConfidence::new(item.confidence.value()),
            attribution_method: attr.method,
            detected_hazards: item
                .hazards
                .iter()
                .chain(raw.global_hazards.iter())
                .copied()
                .collect(),
            source_event_uuid: source_event_uuid.to_string(),
            timestamp: Utc::now(),
        });
    }

    DeriveOutcome {
        signals,
        abstentions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::sentiment::types::{
        ClassifierConfidence, ItemClassification, LoadedItem, LoadedItemKind,
    };

    fn item(id: &str) -> LoadedItem {
        LoadedItem {
            id: LoadedItemId::new(id),
            kind: LoadedItemKind::Lesson,
            label: id.into(),
            keywords: vec![],
        }
    }

    fn classification(id: &str, polarity: Polarity, conf: f32) -> ItemClassification {
        ItemClassification {
            item_id: LoadedItemId::new(id),
            polarity,
            confidence: ClassifierConfidence::new(conf),
            evidence: None,
            hazards: vec![],
        }
    }

    fn empty_request_with_text(text: &str, items: Vec<LoadedItem>) -> ClassificationRequest {
        ClassificationRequest {
            utterance: text.into(),
            loaded_items: items,
            recent_turns: vec![],
        }
    }

    #[test]
    fn skips_positive_below_threshold() {
        let raw = RawClassification {
            per_item: vec![classification("a", Polarity::Positive, 0.70)],
            global_hazards: vec![],
        };
        let req = empty_request_with_text("thanks", vec![item("a")]);
        let out = derive_signals(
            &raw,
            &req,
            &HashMap::new(),
            &OrchestratorConfig::default(),
            Instant::now(),
            "evt-1",
        );
        assert!(out.signals.is_empty());
        assert!(matches!(
            out.abstentions[0].1,
            AbstainReason::BelowThreshold { .. }
        ));
    }

    #[test]
    fn skips_neutral_polarity() {
        let raw = RawClassification {
            per_item: vec![classification("a", Polarity::Neutral, 0.99)],
            global_hazards: vec![],
        };
        let req = empty_request_with_text("ok", vec![item("a")]);
        let out = derive_signals(
            &raw,
            &req,
            &HashMap::new(),
            &OrchestratorConfig::default(),
            Instant::now(),
            "evt-1",
        );
        assert!(out.signals.is_empty());
        assert!(matches!(out.abstentions[0].1, AbstainReason::Neutral));
    }

    #[test]
    fn auto_abstains_on_sarcasm() {
        let mut c = classification("a", Polarity::Positive, 0.95);
        c.hazards = vec![Hazard::Sarcasm];
        let raw = RawClassification {
            per_item: vec![c],
            global_hazards: vec![],
        };
        let req = empty_request_with_text("great", vec![item("a")]);
        let out = derive_signals(
            &raw,
            &req,
            &HashMap::new(),
            &OrchestratorConfig::default(),
            Instant::now(),
            "evt-1",
        );
        assert!(out.signals.is_empty());
        assert!(matches!(
            out.abstentions[0].1,
            AbstainReason::HazardSet(Hazard::Sarcasm)
        ));
    }

    #[test]
    fn auto_abstains_on_self_directed() {
        let mut c = classification("a", Polarity::Negative, 0.90);
        c.hazards = vec![Hazard::SelfDirected];
        let raw = RawClassification {
            per_item: vec![c],
            global_hazards: vec![],
        };
        let req = empty_request_with_text("ugh i'm an idiot", vec![item("a")]);
        let out = derive_signals(
            &raw,
            &req,
            &HashMap::new(),
            &OrchestratorConfig::default(),
            Instant::now(),
            "evt-1",
        );
        assert!(out.signals.is_empty());
        assert!(matches!(
            out.abstentions[0].1,
            AbstainReason::HazardSet(Hazard::SelfDirected)
        ));
    }

    #[test]
    fn does_not_auto_abstain_on_low_confidence_hazard() {
        let mut c = classification("a", Polarity::Positive, 0.90);
        c.hazards = vec![Hazard::LowConfidence];
        let raw = RawClassification {
            per_item: vec![c],
            global_hazards: vec![],
        };
        let mut it = item("a");
        it.keywords = vec!["thanks".into()];
        let req = empty_request_with_text("thanks", vec![it]);
        let out = derive_signals(
            &raw,
            &req,
            &HashMap::new(),
            &OrchestratorConfig::default(),
            Instant::now(),
            "evt-1",
        );
        assert_eq!(out.signals.len(), 1);
        assert_eq!(out.signals[0].detected_hazards, vec![Hazard::LowConfidence]);
    }

    #[test]
    fn skips_when_attribution_does_not_match() {
        let raw = RawClassification {
            per_item: vec![classification("les-aardvark", Polarity::Positive, 0.90)],
            global_hazards: vec![],
        };
        let mut item_a = item("les-aardvark");
        item_a.keywords = vec!["zebra-special".into()];
        let mut item_b = item("les-beaver");
        item_b.keywords = vec!["quokka-special".into()];
        let req = empty_request_with_text("thanks for quokka-special", vec![item_a, item_b]);
        let out = derive_signals(
            &raw,
            &req,
            &HashMap::new(),
            &OrchestratorConfig::default(),
            Instant::now(),
            "evt-1",
        );
        assert!(out.signals.is_empty());
        assert!(matches!(
            out.abstentions[0].1,
            AbstainReason::AttributionMismatch
        ));
    }

    #[test]
    fn rate_limits_recent_signal() {
        let raw = RawClassification {
            per_item: vec![classification("a", Polarity::Positive, 0.90)],
            global_hazards: vec![],
        };
        let mut it = item("a");
        it.keywords = vec!["thanks".into()];
        let req = empty_request_with_text("thanks", vec![it]);
        let mut rate = HashMap::new();
        let now = Instant::now();
        rate.insert(
            LoadedItemId::new("a"),
            now - std::time::Duration::from_secs(1),
        );
        let out = derive_signals(
            &raw,
            &req,
            &rate,
            &OrchestratorConfig::default(),
            now,
            "evt-1",
        );
        assert!(out.signals.is_empty());
        assert!(matches!(out.abstentions[0].1, AbstainReason::RateLimited));
    }

    #[test]
    fn allows_signal_after_cooldown_elapses() {
        let raw = RawClassification {
            per_item: vec![classification("a", Polarity::Positive, 0.90)],
            global_hazards: vec![],
        };
        let mut it = item("a");
        it.keywords = vec!["thanks".into()];
        let req = empty_request_with_text("thanks", vec![it]);
        let mut rate = HashMap::new();
        let now = Instant::now();
        rate.insert(
            LoadedItemId::new("a"),
            now - std::time::Duration::from_secs(120),
        );
        let out = derive_signals(
            &raw,
            &req,
            &rate,
            &OrchestratorConfig::default(),
            now,
            "evt-1",
        );
        assert_eq!(out.signals.len(), 1);
    }

    #[test]
    fn returns_abstain_when_classifier_abstained() {
        let raw = RawClassification::abstain();
        let req = empty_request_with_text("", vec![]);
        let out = derive_signals(
            &raw,
            &req,
            &HashMap::new(),
            &OrchestratorConfig::default(),
            Instant::now(),
            "evt-1",
        );
        assert!(out.signals.is_empty());
        assert!(matches!(
            out.abstentions[0].1,
            AbstainReason::ClassifierAbstained
        ));
    }
}
