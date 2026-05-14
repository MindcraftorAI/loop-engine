//! Attribution — five-pass pure function mapping utterance → item.
//!
//! Locked decisions (learn-notes D4):
//! - **Pure function**, not a struct, not a state machine, not a typestate
//! - `attribute_signal(...) -> Option<Attribution>` — abstain is `None`
//! - `attribute_signal_with_fallback<F: FnOnce(...)>(...)` — Pass 4
//!   accepts the orchestrator's classifier-judge closure
//! - Stateless — orchestrator (Day 16) holds session state separately
//!
//! Passes:
//!   1. **DirectMention** — utterance literally names the item id or
//!      a distinctive keyword. Confidence 0.95.
//!   2. **PronounResolved** — utterance is a pronoun ("that", "it",
//!      "this") + the previous assistant turn referenced exactly one
//!      item. Confidence 0.80.
//!   3. **Recency** — only one loaded item was recently referenced
//!      across `recent_turns`. Confidence 0.65.
//!   4. **Salience** — 2-5 candidates; defer to the classifier-judge
//!      closure (only via `_with_fallback`).
//!   5. **Abstain** — return `None`.

use std::collections::HashSet;
use std::sync::LazyLock;

use super::types::{
    AttributionConfidence, AttributionMethod, LoadedItem, LoadedItemId, RecentTurn, TurnRole,
};

/// Minimum classifier-judge confidence for Pass 4 (Salience) to fire.
///
/// Locked by [`docs/research/sentiment-design-rules.md`] — the rule is
/// "classifier-judged salience is only trustworthy at high self-reported
/// confidence." Below this threshold, attribute_signal abstains rather
/// than guessing.
///
/// Day 15 audit M5: extracted from inline `0.8` magic number.
const PASS4_MIN_CONFIDENCE: f32 = 0.8;

/// Output of [`attribute_signal`] and [`attribute_signal_with_fallback`].
///
/// Per learn-notes D4 / pre-research Q4 audit-smells: **abstain is
/// `Option<Attribution>::None`, not a sixth `AttributionMethod` variant**.
#[derive(Debug, Clone, PartialEq)]
pub struct Attribution {
    pub item_id: LoadedItemId,
    pub method: AttributionMethod,
    pub confidence: AttributionConfidence,
}

/// Five-pass attribution without the classifier-judge fallback (Pass 4).
/// Pure function — same inputs always yield the same output.
///
/// Returns `None` to mean "abstained" (Pass 5). No `Result` because there
/// is no error case — abstaining is not an error.
pub fn attribute_signal(
    utterance: &str,
    loaded_items: &[LoadedItem],
    recent_turns: &[RecentTurn],
) -> Option<Attribution> {
    pass1_direct_mention(utterance, loaded_items)
        .or_else(|| pass2_pronoun_anaphor(utterance, recent_turns))
        .or_else(|| pass3_single_recent(recent_turns))
}

/// Five-pass attribution with the classifier-judge fallback (Pass 4).
///
/// Day 15 ships the signature; Day 16 orchestrator wires the closure
/// (per learn-notes OQ1). Until then, callers can pass any closure or
/// simply use the no-fallback variant.
///
/// `F` is `FnOnce` — Pass 4 fires at most once. Generic-monomorphized
/// (no allocation per call).
pub fn attribute_signal_with_fallback<F>(
    utterance: &str,
    loaded_items: &[LoadedItem],
    recent_turns: &[RecentTurn],
    fallback: F,
) -> Option<Attribution>
where
    F: FnOnce(&[LoadedItem]) -> Option<(LoadedItemId, AttributionConfidence)>,
{
    if let Some(a) = attribute_signal(utterance, loaded_items, recent_turns) {
        return Some(a);
    }
    let candidates = recently_referenced_items(loaded_items, recent_turns);
    if (2..=5).contains(&candidates.len()) {
        if let Some((item_id, conf)) = fallback(&candidates) {
            // Threshold: the judge must clear `PASS4_MIN_CONFIDENCE`
            // (0.8) or we abstain rather than guess.
            if conf.value() >= PASS4_MIN_CONFIDENCE {
                return Some(Attribution {
                    item_id,
                    method: AttributionMethod::Salience,
                    confidence: conf,
                });
            }
        }
    }
    None
}

// =====================================================================
// Pass 1 — direct mention (id or distinctive keyword)
// =====================================================================

fn pass1_direct_mention(utterance: &str, loaded_items: &[LoadedItem]) -> Option<Attribution> {
    let lower = utterance.to_lowercase();
    for item in loaded_items {
        if lower.contains(&item.id.as_str().to_lowercase())
            || item
                .keywords
                .iter()
                .any(|kw| !kw.is_empty() && lower.contains(&kw.to_lowercase()))
        {
            return Some(Attribution {
                item_id: item.id.clone(),
                method: AttributionMethod::DirectMention,
                confidence: AttributionConfidence::new(0.95),
            });
        }
    }
    None
}

// =====================================================================
// Pass 2 — pronoun anaphor: utterance starts with a pronoun, prior
// assistant turn referenced exactly one item
// =====================================================================

static PRONOUNS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    let mut s = HashSet::new();
    for p in &["that", "it", "this", "those", "these", "they"] {
        s.insert(*p);
    }
    s
});

fn pass2_pronoun_anaphor(utterance: &str, recent_turns: &[RecentTurn]) -> Option<Attribution> {
    let first_word = first_word_lowercase(utterance)?;
    if !PRONOUNS.contains(first_word.as_str()) {
        return None;
    }
    // Find the most recent assistant turn.
    let prior = recent_turns
        .iter()
        .rev()
        .find(|t| t.role == TurnRole::Assistant)?;
    if prior.referenced_items.len() != 1 {
        return None;
    }
    Some(Attribution {
        item_id: prior.referenced_items[0].clone(),
        method: AttributionMethod::PronounResolved,
        confidence: AttributionConfidence::new(0.80),
    })
}

// =====================================================================
// Pass 3 — single recent: across recent_turns, only one item was referenced
// =====================================================================

fn pass3_single_recent(recent_turns: &[RecentTurn]) -> Option<Attribution> {
    let mut all_referenced: HashSet<LoadedItemId> = HashSet::new();
    for turn in recent_turns {
        for id in &turn.referenced_items {
            all_referenced.insert(id.clone());
        }
    }
    if all_referenced.len() == 1 {
        let id = all_referenced.into_iter().next().expect("len == 1");
        return Some(Attribution {
            item_id: id,
            method: AttributionMethod::Recency,
            confidence: AttributionConfidence::new(0.65),
        });
    }
    None
}

// =====================================================================
// Pass 4 support — collect items referenced in recent turns
// =====================================================================

fn recently_referenced_items(
    loaded_items: &[LoadedItem],
    recent_turns: &[RecentTurn],
) -> Vec<LoadedItem> {
    let referenced_ids: HashSet<LoadedItemId> = recent_turns
        .iter()
        .flat_map(|t| t.referenced_items.iter().cloned())
        .collect();
    loaded_items
        .iter()
        .filter(|i| referenced_ids.contains(&i.id))
        .cloned()
        .collect()
}

fn first_word_lowercase(text: &str) -> Option<String> {
    text.split_whitespace()
        .next()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::sentiment::types::LoadedItemKind;

    fn item(id: &str, label: &str, keywords: &[&str]) -> LoadedItem {
        LoadedItem {
            id: LoadedItemId::new(id),
            kind: LoadedItemKind::Lesson,
            label: label.into(),
            keywords: keywords.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn turn(role: TurnRole, text: &str, refs: &[&str]) -> RecentTurn {
        RecentTurn {
            role,
            text: text.into(),
            referenced_items: refs.iter().map(|s| LoadedItemId::new(*s)).collect(),
        }
    }

    // ---- Pass 1 — direct mention ----

    #[test]
    fn pass1_fires_on_id_mention() {
        let items = vec![item("les-abc123", "Atomic rename", &["atomic", "rename"])];
        let result = attribute_signal("thanks for les-abc123", &items, &[]).unwrap();
        assert_eq!(result.item_id.as_str(), "les-abc123");
        assert_eq!(result.method, AttributionMethod::DirectMention);
        assert_eq!(result.confidence.value(), 0.95);
    }

    #[test]
    fn pass1_fires_on_keyword_mention() {
        let items = vec![item("les-abc123", "Atomic rename", &["atomic", "rename"])];
        let result = attribute_signal("the atomic rename was wrong", &items, &[]).unwrap();
        assert_eq!(result.item_id.as_str(), "les-abc123");
        assert_eq!(result.method, AttributionMethod::DirectMention);
    }

    #[test]
    fn pass1_ignores_unrelated_text() {
        let items = vec![item("les-abc123", "Atomic rename", &["atomic", "rename"])];
        // No id, no keyword, no recent turns → abstain.
        let result = attribute_signal("hello", &items, &[]);
        assert!(result.is_none());
    }

    // ---- Pass 2 — pronoun anaphor ----

    #[test]
    fn pass2_fires_on_pronoun_with_single_referenced_in_prior_assistant_turn() {
        let items = vec![item("les-a", "X", &[])];
        let recents = vec![turn(TurnRole::Assistant, "I applied X", &["les-a"])];
        let result = attribute_signal("that worked", &items, &recents).unwrap();
        assert_eq!(result.item_id.as_str(), "les-a");
        assert_eq!(result.method, AttributionMethod::PronounResolved);
        assert_eq!(result.confidence.value(), 0.80);
    }

    #[test]
    fn pass2_abstains_when_no_prior_assistant_turn() {
        let items = vec![item("les-a", "X", &[])];
        let recents = vec![turn(TurnRole::User, "hi", &[])];
        let result = attribute_signal("that worked", &items, &recents);
        assert!(result.is_none());
    }

    #[test]
    fn pass2_abstains_when_prior_assistant_referenced_multiple_items() {
        let items = vec![item("les-a", "X", &[])];
        let recents = vec![turn(
            TurnRole::Assistant,
            "I applied X and Y",
            &["les-a", "les-b"],
        )];
        // Two referenced items in prior assistant turn → Pass 2 doesn't fire.
        // Pass 3 sees ≥2 distinct items recent → doesn't fire either.
        let result = attribute_signal("that worked", &items, &recents);
        assert!(result.is_none());
    }

    // ---- Pass 3 — single recent ----

    #[test]
    fn pass3_fires_when_only_one_item_in_recent_turns() {
        let items = vec![item("les-x", "X", &[])];
        let recents = vec![
            turn(TurnRole::User, "first", &["les-x"]),
            turn(TurnRole::Assistant, "ack", &["les-x"]),
        ];
        // Utterance doesn't mention "les-x" by id/keyword (Pass 1 misses),
        // doesn't start with a pronoun (Pass 2 misses), but only les-x has
        // been referenced (Pass 3 fires).
        let result = attribute_signal("works", &items, &recents).unwrap();
        assert_eq!(result.item_id.as_str(), "les-x");
        assert_eq!(result.method, AttributionMethod::Recency);
        assert_eq!(result.confidence.value(), 0.65);
    }

    // ---- Pass 4 — _with_fallback signature ----

    #[test]
    fn pass4_with_fallback_fires_when_2_to_5_candidates_and_judge_confident() {
        let items = vec![
            item("les-a", "A", &[]),
            item("les-b", "B", &[]),
            item("les-c", "C", &[]),
        ];
        let recents = vec![turn(
            TurnRole::Assistant,
            "ack",
            &["les-a", "les-b", "les-c"],
        )];
        let result = attribute_signal_with_fallback("hmm", &items, &recents, |candidates| {
            // Stub judge picks the first candidate at high confidence.
            let pick = &candidates[0];
            Some((pick.id.clone(), AttributionConfidence::new(0.9)))
        })
        .unwrap();
        assert_eq!(result.item_id.as_str(), "les-a");
        assert_eq!(result.method, AttributionMethod::Salience);
        assert_eq!(result.confidence.value(), 0.9);
    }

    #[test]
    fn pass4_abstains_when_judge_below_threshold() {
        let items = vec![
            item("les-a", "A", &[]),
            item("les-b", "B", &[]),
            item("les-c", "C", &[]),
        ];
        let recents = vec![turn(
            TurnRole::Assistant,
            "ack",
            &["les-a", "les-b", "les-c"],
        )];
        let result = attribute_signal_with_fallback("hmm", &items, &recents, |candidates| {
            Some((candidates[0].id.clone(), AttributionConfidence::new(0.5)))
        });
        assert!(result.is_none());
    }

    #[test]
    fn pass4_skipped_when_only_one_candidate() {
        // One candidate is Pass 3 territory; Pass 4 only fires for 2-5.
        let items = vec![item("les-a", "A", &[])];
        let recents = vec![turn(TurnRole::Assistant, "ack", &["les-a"])];
        // Pass 3 fires here (single recent); Pass 4 never reached.
        let result =
            attribute_signal_with_fallback("hmm", &items, &recents, |_| panic!("unreachable"))
                .unwrap();
        assert_eq!(result.method, AttributionMethod::Recency);
    }

    // ---- Abstain ----

    #[test]
    fn abstains_when_no_pass_fires() {
        let items = vec![item("les-a", "A", &["alpha"])];
        let recents: Vec<RecentTurn> = vec![];
        let result = attribute_signal("hello world", &items, &recents);
        assert!(result.is_none());
    }
}
