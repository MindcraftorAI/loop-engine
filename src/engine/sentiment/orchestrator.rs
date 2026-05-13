//! Sentiment orchestrator — per-session state machine + classifier wiring.
//!
//! Locked decisions (`docs/research/day-16a-learn-notes.md`):
//! - D2: `Arc<DashMap<SessionId, Mutex<SessionState>>>` keyed mutable state
//! - D4: hand-rolled per-(session, lesson) rate-limit `HashMap<LoadedItemId, Instant>`
//! - D5: `std::sync::Mutex` — critical sections NEVER `.await`
//! - D9: hazard auto-abstain = Sarcasm | AmbiguousReferent | OutOfDistribution | SelfDirected
//! - D10: polarity-asymmetric thresholds (POSITIVE_MIN 0.75, NEGATIVE_MIN 0.85)
//! - D11: attribution cross-check via Day 15 `attribute_signal`
//! - D12: correction-window mining on `UserInterrupt`
//! - D13: `SignalWriter` trait sink (16a: Logging/Mock; 16b: StorageBacked)
//!
//! Critical-section discipline (smell S22 / S23):
//! ```text
//!     lock → snapshot/mutate → DROP → await classifier → re-lock → apply
//! ```
//! Enforced by the module-scoped `clippy::await_holding_lock = deny` below.

#![deny(clippy::await_holding_lock)]
#![warn(clippy::significant_drop_in_scrutinee)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use dashmap::DashMap;
use tracing::warn;

use crate::engine::context::{Context, SessionId};
use crate::engine::events::EngineEvent;

use super::attribution::attribute_signal;
use super::classifier::{ClassifierError, SentimentClassifier};
use super::signals::{
    AbstainReason, OrchestratorOutput, SentimentSignal, SignalWriteError, SignalWriter,
};
use super::types::{
    AttributionMethod, CalibratedConfidence, ClassificationRequest, Hazard, LoadedItemId, Polarity,
    RawClassification, RecentTurn, TurnRole,
};

// ---------------------------------------------------------------------
// Locked thresholds — per sentiment-design-rules.md rule 5 (Day 16a D10)
// ---------------------------------------------------------------------

/// Minimum classifier confidence for a positive signal to emit.
const POSITIVE_MIN: f32 = 0.75;
/// Minimum classifier confidence for a negative signal to emit (asymmetric
/// — negatives carry higher friction risk, so we want more certainty).
const NEGATIVE_MIN: f32 = 0.85;

// ---------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------

/// Tunables for the orchestrator. Module-local per Day 16a OQ-D16a-4.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct OrchestratorConfig {
    /// Maximum recent turns retained for attribution + correction-window
    /// mining. Default 6 per design rules (4-6).
    pub recent_turn_capacity: usize,
    /// Minimum gap between sentiment signals for the same (session, lesson).
    /// Default 60s (audit-A2 rate-limit lineage).
    pub per_lesson_cooldown: Duration,
    /// Window inside which a UserInterrupt is considered a correction
    /// of the prior assistant turn. Default 30s (half of per_lesson_cooldown
    /// so a real interrupt-then-frustration sequence isn't suppressed).
    pub correction_window: Duration,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            recent_turn_capacity: 6,
            per_lesson_cooldown: Duration::from_secs(60),
            correction_window: Duration::from_secs(30),
        }
    }
}

// ---------------------------------------------------------------------
// Per-session state (D2 + D3)
// ---------------------------------------------------------------------

/// Per-session in-memory state.
///
/// Guarded by `std::sync::Mutex` (D5). Critical sections never `.await`;
/// the orchestrator's `lock → snapshot → drop → await → re-lock` pattern
/// is enforced by the `clippy::await_holding_lock = deny` module lint.
#[derive(Debug)]
#[non_exhaustive]
pub struct SessionState {
    pub recent_turns: VecDeque<RecentTurn>,
    pub rate_limit: HashMap<LoadedItemId, Instant>,
    pub phase: SessionPhase,
    pub turn_count: u64,
    /// Wall-clock of the most-recent observed assistant turn — used by
    /// correction-window mining to decide whether a `UserInterrupt` is
    /// "proximal" to a referencing assistant turn.
    pub last_assistant_turn_at: Option<Instant>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            recent_turns: VecDeque::new(),
            rate_limit: HashMap::new(),
            phase: SessionPhase::Idle,
            turn_count: 0,
            last_assistant_turn_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionPhase {
    Idle,
    /// A classifier call is in flight for this session.
    AwaitingClassifier {
        utterance: String,
        started_at: Instant,
    },
}

// ---------------------------------------------------------------------
// Orchestrator — the public surface
// ---------------------------------------------------------------------

/// Engine sentiment orchestrator.
///
/// Cheap to `Clone` (single `Arc` deref). The clone shares the same
/// `DashMap` of sessions, the same classifier, and the same writer —
/// designed to be passed by value into spawned tasks.
#[derive(Clone)]
pub struct Orchestrator {
    inner: Arc<OrchestratorInner>,
}

struct OrchestratorInner {
    classifier: Arc<dyn SentimentClassifier>,
    writer: Arc<dyn SignalWriter>,
    sessions: DashMap<SessionId, Mutex<SessionState>>,
    config: OrchestratorConfig,
}

impl std::fmt::Debug for Orchestrator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Orchestrator")
            .field("classifier", &self.inner.classifier.name())
            .field("config", &self.inner.config)
            .field("session_count", &self.inner.sessions.len())
            .finish_non_exhaustive()
    }
}

impl Orchestrator {
    pub fn new(
        classifier: Arc<dyn SentimentClassifier>,
        writer: Arc<dyn SignalWriter>,
        config: OrchestratorConfig,
    ) -> Self {
        Self {
            inner: Arc::new(OrchestratorInner {
                classifier,
                writer,
                sessions: DashMap::new(),
                config,
            }),
        }
    }

    /// Process one engine event. Dispatches by variant. Non-relevant
    /// variants (SessionStarted, SessionEnded) return empty output.
    pub async fn process_event(
        &self,
        ctx: &Context,
        event: &EngineEvent,
    ) -> OrchestratorOutput {
        match event {
            EngineEvent::UserTurn { .. } => self.handle_user_turn(ctx, event).await,
            EngineEvent::UserInterrupt { .. } => self.handle_user_interrupt(ctx, event).await,
            EngineEvent::SessionEnded { session_id } => {
                // Drop session state on end — cleanup the rate-limit map.
                self.inner.sessions.remove(session_id);
                OrchestratorOutput::empty()
            }
            _ => OrchestratorOutput::empty(),
        }
    }

    async fn handle_user_turn(
        &self,
        ctx: &Context,
        event: &EngineEvent,
    ) -> OrchestratorOutput {
        let EngineEvent::UserTurn {
            event_uuid, text, ..
        } = event
        else {
            unreachable!("dispatch guarantees UserTurn here");
        };

        // === Critical section 1: append turn, set phase, snapshot request ===
        let request = {
            let entry = self.inner.sessions.entry(ctx.session_id.clone()).or_default();
            let mut state = entry.lock().expect("session state mutex poisoned");
            push_turn(
                &mut state,
                self.inner.config.recent_turn_capacity,
                RecentTurn {
                    role: TurnRole::User,
                    text: text.clone(),
                    referenced_items: Vec::new(),
                },
            );
            state.phase = SessionPhase::AwaitingClassifier {
                utterance: text.clone(),
                started_at: Instant::now(),
            };
            // Build the request from the current state. NOTE: loaded_items
            // is empty for 16a — manifest assembly lives elsewhere and is
            // wired in 16b+ alongside lessons migration.
            ClassificationRequest {
                utterance: text.clone(),
                loaded_items: Vec::new(),
                recent_turns: state.recent_turns.iter().cloned().collect(),
            }
        }; // <-- lock dropped here

        // === Async classifier call OUTSIDE the lock ===
        let raw = match self.inner.classifier.classify(ctx, &request).await {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    classifier = self.inner.classifier.name(),
                    err = %ClassifierErrorDisplay(&e),
                    "classifier error; abstaining for this turn",
                );
                // Reset phase before returning.
                self.reset_phase_idle(&ctx.session_id);
                return OrchestratorOutput {
                    signals: vec![],
                    abstentions: vec![(None, AbstainReason::ClassifierAbstained)],
                };
            }
        };

        // === Critical section 2: derive signals, update rate limit, persist ===
        let (signals, abstentions) = {
            let entry = self
                .inner
                .sessions
                .get(&ctx.session_id)
                .expect("session must exist after critical section 1");
            let mut state = entry.lock().expect("session state mutex poisoned");
            let now = Instant::now();
            let result =
                derive_signals(&raw, &request, &state.rate_limit, &self.inner.config, now, event_uuid);
            for sig in &result.0 {
                state.rate_limit.insert(sig.item_id.clone(), now);
            }
            state.phase = SessionPhase::Idle;
            state.turn_count += 1;
            result
        };

        // Persist signals OUTSIDE the lock.
        for sig in &signals {
            if let Err(e) = self.inner.writer.record(ctx, sig).await {
                warn!(
                    item = %sig.item_id,
                    err = %SignalWriteErrorDisplay(&e),
                    "signal writer error; signal dropped",
                );
            }
        }

        OrchestratorOutput {
            signals,
            abstentions,
        }
    }

    async fn handle_user_interrupt(
        &self,
        ctx: &Context,
        event: &EngineEvent,
    ) -> OrchestratorOutput {
        let EngineEvent::UserInterrupt { event_uuid, .. } = event else {
            unreachable!("dispatch guarantees UserInterrupt here");
        };

        let (signals, abstentions) = {
            let entry = self.inner.sessions.entry(ctx.session_id.clone()).or_default();
            let mut state = entry.lock().expect("session state mutex poisoned");
            let now = Instant::now();
            let cooldown = self.inner.config.per_lesson_cooldown;

            // Find the most-recent assistant turn that referenced items.
            let proximal = state
                .recent_turns
                .iter()
                .rev()
                .find(|t| t.role == TurnRole::Assistant && !t.referenced_items.is_empty());

            let Some(turn) = proximal else {
                state.turn_count += 1;
                // Rule 15 — no proximal assistant reference, abstain.
                return OrchestratorOutput {
                    signals: vec![],
                    abstentions: vec![(None, AbstainReason::NoProximalReference)],
                };
            };

            // Check correction-window timing.
            let within_window = state
                .last_assistant_turn_at
                .map(|ts| now.duration_since(ts) <= self.inner.config.correction_window)
                .unwrap_or(false);
            if !within_window {
                state.turn_count += 1;
                return OrchestratorOutput {
                    signals: vec![],
                    abstentions: vec![(None, AbstainReason::NoProximalReference)],
                };
            }

            // Build negative signals for each referenced item, subject to rate limit.
            let mut signals = Vec::with_capacity(turn.referenced_items.len());
            let mut abstentions = Vec::new();
            let item_ids: Vec<LoadedItemId> = turn.referenced_items.clone();
            for item_id in item_ids {
                if let Some(&last) = state.rate_limit.get(&item_id) {
                    if now.duration_since(last) < cooldown {
                        abstentions.push((Some(item_id.clone()), AbstainReason::RateLimited));
                        continue;
                    }
                }
                let sig = SentimentSignal {
                    item_id: item_id.clone(),
                    polarity: Polarity::Negative,
                    calibrated_confidence: CalibratedConfidence::new(0.9),
                    attribution_method: AttributionMethod::Recency,
                    detected_hazards: vec![],
                    source_event_uuid: event_uuid.clone(),
                    timestamp: Utc::now(),
                };
                state.rate_limit.insert(item_id, now);
                signals.push(sig);
            }
            state.turn_count += 1;
            (signals, abstentions)
        };

        for sig in &signals {
            if let Err(e) = self.inner.writer.record(ctx, sig).await {
                warn!(item = %sig.item_id, err = %SignalWriteErrorDisplay(&e), "signal writer error");
            }
        }

        OrchestratorOutput {
            signals,
            abstentions,
        }
    }

    fn reset_phase_idle(&self, session_id: &SessionId) {
        if let Some(entry) = self.inner.sessions.get(session_id) {
            let mut state = entry.lock().expect("session state mutex poisoned");
            state.phase = SessionPhase::Idle;
        }
    }

    /// Test/debug helper: number of live sessions tracked.
    #[doc(hidden)]
    pub fn session_count(&self) -> usize {
        self.inner.sessions.len()
    }
}

// ---------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------

fn push_turn(state: &mut SessionState, capacity: usize, turn: RecentTurn) {
    if turn.role == TurnRole::Assistant {
        state.last_assistant_turn_at = Some(Instant::now());
    }
    state.recent_turns.push_back(turn);
    while state.recent_turns.len() > capacity {
        state.recent_turns.pop_front();
    }
}

/// Apply threshold + hazard auto-abstain + attribution cross-check +
/// rate limit to each per-item classification. Pure function — caller
/// supplies the rate-limit map and the current time.
fn derive_signals(
    raw: &RawClassification,
    request: &ClassificationRequest,
    rate_limit: &HashMap<LoadedItemId, Instant>,
    config: &OrchestratorConfig,
    now: Instant,
    source_event_uuid: &str,
) -> (Vec<SentimentSignal>, Vec<(Option<LoadedItemId>, AbstainReason)>) {
    let mut signals: Vec<SentimentSignal> = Vec::with_capacity(1);
    let mut abstentions: Vec<(Option<LoadedItemId>, AbstainReason)> = Vec::new();
    let mut seen: HashSet<LoadedItemId> = HashSet::new();

    if raw.is_abstain() {
        abstentions.push((None, AbstainReason::ClassifierAbstained));
        return (signals, abstentions);
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
        for h in item.hazards.iter().chain(raw.global_hazards.iter()).copied() {
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
            abstentions.push((Some(item.item_id.clone()), AbstainReason::AttributionAbstained));
            continue;
        };
        if attr.item_id != item.item_id {
            abstentions.push((Some(item.item_id.clone()), AbstainReason::AttributionMismatch));
            continue;
        }

        if let Some(&last) = rate_limit.get(&item.item_id) {
            if now.duration_since(last) < config.per_lesson_cooldown {
                abstentions.push((Some(item.item_id.clone()), AbstainReason::RateLimited));
                continue;
            }
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

    (signals, abstentions)
}

fn is_auto_abstain_hazard(h: Hazard) -> bool {
    matches!(
        h,
        Hazard::Sarcasm
            | Hazard::AmbiguousReferent
            | Hazard::OutOfDistribution
            | Hazard::SelfDirected
    )
}

// Display wrappers — Tracing requires `Display`; the underlying error
// types impl `Display` already, but we want fields on the event record
// to be `Display`-formatted, not `Debug`-formatted, for grep-ability.
struct ClassifierErrorDisplay<'a>(&'a ClassifierError);
impl std::fmt::Display for ClassifierErrorDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self.0, f)
    }
}
struct SignalWriteErrorDisplay<'a>(&'a SignalWriteError);
impl std::fmt::Display for SignalWriteErrorDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self.0, f)
    }
}

// =====================================================================
// Tests — pure-function unit tests for derive_signals + integration
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::sentiment::classifier::MockSentimentClassifier;
    use crate::engine::sentiment::signals::MockSignalWriter;
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

    // ---- derive_signals — threshold gating ----

    #[test]
    fn derive_skips_positive_below_threshold() {
        let raw = RawClassification {
            per_item: vec![classification("a", Polarity::Positive, 0.70)],
            global_hazards: vec![],
        };
        let req = empty_request_with_text("thanks", vec![item("a")]);
        let (sigs, abst) = derive_signals(
            &raw,
            &req,
            &HashMap::new(),
            &OrchestratorConfig::default(),
            Instant::now(),
            "evt-1",
        );
        assert!(sigs.is_empty());
        assert!(matches!(
            abst[0].1,
            AbstainReason::BelowThreshold { .. }
        ));
    }

    #[test]
    fn derive_skips_negative_below_threshold() {
        let raw = RawClassification {
            per_item: vec![classification("a", Polarity::Negative, 0.80)],
            global_hazards: vec![],
        };
        let req = empty_request_with_text("broken", vec![item("a")]);
        let (sigs, _) = derive_signals(
            &raw,
            &req,
            &HashMap::new(),
            &OrchestratorConfig::default(),
            Instant::now(),
            "evt-1",
        );
        assert!(sigs.is_empty());
    }

    #[test]
    fn derive_skips_neutral_polarity() {
        let raw = RawClassification {
            per_item: vec![classification("a", Polarity::Neutral, 0.99)],
            global_hazards: vec![],
        };
        let req = empty_request_with_text("ok", vec![item("a")]);
        let (sigs, abst) = derive_signals(
            &raw,
            &req,
            &HashMap::new(),
            &OrchestratorConfig::default(),
            Instant::now(),
            "evt-1",
        );
        assert!(sigs.is_empty());
        assert!(matches!(abst[0].1, AbstainReason::Neutral));
    }

    // ---- derive_signals — hazard auto-abstain ----

    #[test]
    fn derive_auto_abstains_on_sarcasm() {
        let mut c = classification("a", Polarity::Positive, 0.95);
        c.hazards = vec![Hazard::Sarcasm];
        let raw = RawClassification {
            per_item: vec![c],
            global_hazards: vec![],
        };
        let req = empty_request_with_text("great", vec![item("a")]);
        let (sigs, abst) = derive_signals(
            &raw,
            &req,
            &HashMap::new(),
            &OrchestratorConfig::default(),
            Instant::now(),
            "evt-1",
        );
        assert!(sigs.is_empty());
        assert!(matches!(abst[0].1, AbstainReason::HazardSet(Hazard::Sarcasm)));
    }

    #[test]
    fn derive_auto_abstains_on_self_directed() {
        let mut c = classification("a", Polarity::Negative, 0.90);
        c.hazards = vec![Hazard::SelfDirected];
        let raw = RawClassification {
            per_item: vec![c],
            global_hazards: vec![],
        };
        let req = empty_request_with_text("ugh i'm an idiot", vec![item("a")]);
        let (sigs, abst) = derive_signals(
            &raw,
            &req,
            &HashMap::new(),
            &OrchestratorConfig::default(),
            Instant::now(),
            "evt-1",
        );
        assert!(sigs.is_empty());
        assert!(matches!(
            abst[0].1,
            AbstainReason::HazardSet(Hazard::SelfDirected)
        ));
    }

    #[test]
    fn derive_does_not_auto_abstain_on_low_confidence_hazard() {
        // LowConfidence is informational, NOT an auto-abstain hazard (D9).
        let mut c = classification("a", Polarity::Positive, 0.90);
        c.hazards = vec![Hazard::LowConfidence];
        let raw = RawClassification {
            per_item: vec![c],
            global_hazards: vec![],
        };
        // Pass attribution by giving the item keyword that the utterance contains.
        let mut it = item("a");
        it.keywords = vec!["thanks".into()];
        let req = empty_request_with_text("thanks", vec![it]);
        let (sigs, _) = derive_signals(
            &raw,
            &req,
            &HashMap::new(),
            &OrchestratorConfig::default(),
            Instant::now(),
            "evt-1",
        );
        assert_eq!(sigs.len(), 1);
        assert_eq!(sigs[0].detected_hazards, vec![Hazard::LowConfidence]);
    }

    // ---- derive_signals — attribution cross-check ----

    #[test]
    fn derive_skips_when_attribution_does_not_match() {
        // Classifier names item "les-aardvark", but utterance contains
        // "les-beaver"'s keyword. Pass 1 attribution will match les-beaver,
        // not les-aardvark → AttributionMismatch.
        let raw = RawClassification {
            per_item: vec![classification("les-aardvark", Polarity::Positive, 0.90)],
            global_hazards: vec![],
        };
        let mut item_a = item("les-aardvark");
        item_a.keywords = vec!["zebra-special".into()];
        let mut item_b = item("les-beaver");
        item_b.keywords = vec!["quokka-special".into()];
        let req = empty_request_with_text("thanks for quokka-special", vec![item_a, item_b]);
        let (sigs, abst) = derive_signals(
            &raw,
            &req,
            &HashMap::new(),
            &OrchestratorConfig::default(),
            Instant::now(),
            "evt-1",
        );
        assert!(sigs.is_empty());
        assert!(matches!(abst[0].1, AbstainReason::AttributionMismatch));
    }

    // ---- derive_signals — rate limit ----

    #[test]
    fn derive_rate_limits_recent_signal() {
        let raw = RawClassification {
            per_item: vec![classification("a", Polarity::Positive, 0.90)],
            global_hazards: vec![],
        };
        let mut it = item("a");
        it.keywords = vec!["thanks".into()];
        let req = empty_request_with_text("thanks", vec![it]);
        let mut rate = HashMap::new();
        let now = Instant::now();
        // Last signal was 1 second ago — within the 60s cooldown.
        rate.insert(LoadedItemId::new("a"), now - Duration::from_secs(1));
        let (sigs, abst) = derive_signals(
            &raw,
            &req,
            &rate,
            &OrchestratorConfig::default(),
            now,
            "evt-1",
        );
        assert!(sigs.is_empty());
        assert!(matches!(abst[0].1, AbstainReason::RateLimited));
    }

    #[test]
    fn derive_allows_signal_after_cooldown_elapses() {
        let raw = RawClassification {
            per_item: vec![classification("a", Polarity::Positive, 0.90)],
            global_hazards: vec![],
        };
        let mut it = item("a");
        it.keywords = vec!["thanks".into()];
        let req = empty_request_with_text("thanks", vec![it]);
        let mut rate = HashMap::new();
        let now = Instant::now();
        rate.insert(LoadedItemId::new("a"), now - Duration::from_secs(120));
        let (sigs, _) = derive_signals(
            &raw,
            &req,
            &rate,
            &OrchestratorConfig::default(),
            now,
            "evt-1",
        );
        assert_eq!(sigs.len(), 1);
    }

    #[test]
    fn derive_returns_abstain_when_classifier_abstained() {
        let raw = RawClassification::abstain();
        let req = empty_request_with_text("", vec![]);
        let (sigs, abst) = derive_signals(
            &raw,
            &req,
            &HashMap::new(),
            &OrchestratorConfig::default(),
            Instant::now(),
            "evt-1",
        );
        assert!(sigs.is_empty());
        assert!(matches!(abst[0].1, AbstainReason::ClassifierAbstained));
    }

    // ---- Orchestrator integration tests ----

    fn orchestrator_with_mocks() -> (Orchestrator, Arc<MockSentimentClassifier>, Arc<MockSignalWriter>)
    {
        let classifier = Arc::new(MockSentimentClassifier::default());
        let writer = Arc::new(MockSignalWriter::default());
        let orch = Orchestrator::new(
            classifier.clone() as Arc<dyn SentimentClassifier>,
            writer.clone() as Arc<dyn SignalWriter>,
            OrchestratorConfig::default(),
        );
        (orch, classifier, writer)
    }

    #[tokio::test]
    async fn orchestrator_session_ended_drops_state() {
        let (orch, _, _) = orchestrator_with_mocks();
        let ctx = Context::single_user_local();
        // Build session by processing a turn first.
        let turn = EngineEvent::UserTurn {
            session_id: ctx.session_id.clone(),
            event_uuid: "e1".into(),
            parent_event_uuid: None,
            text: "hello".into(),
            timestamp: Utc::now(),
            cwd: None,
            host_version: None,
            project_tag: None,
        };
        orch.process_event(&ctx, &turn).await;
        assert_eq!(orch.session_count(), 1);

        let end = EngineEvent::SessionEnded {
            session_id: ctx.session_id.clone(),
        };
        orch.process_event(&ctx, &end).await;
        assert_eq!(orch.session_count(), 0);
    }

    #[tokio::test]
    async fn orchestrator_abstains_when_classifier_returns_abstain() {
        let (orch, _classifier, writer) = orchestrator_with_mocks();
        // Mock has no canned responses → returns abstain.
        let ctx = Context::single_user_local();
        let turn = EngineEvent::UserTurn {
            session_id: ctx.session_id.clone(),
            event_uuid: "e1".into(),
            parent_event_uuid: None,
            text: "thanks".into(),
            timestamp: Utc::now(),
            cwd: None,
            host_version: None,
            project_tag: None,
        };
        let out = orch.process_event(&ctx, &turn).await;
        assert!(out.signals.is_empty());
        assert_eq!(out.abstentions.len(), 1);
        assert!(matches!(out.abstentions[0].1, AbstainReason::ClassifierAbstained));
        assert!(writer.captured().is_empty());
    }

    #[tokio::test]
    async fn orchestrator_user_interrupt_no_proximal_assistant_abstains() {
        let (orch, _, _) = orchestrator_with_mocks();
        let ctx = Context::single_user_local();
        let interrupt = EngineEvent::UserInterrupt {
            session_id: ctx.session_id.clone(),
            event_uuid: "i1".into(),
            parent_event_uuid: None,
            timestamp: Utc::now(),
        };
        let out = orch.process_event(&ctx, &interrupt).await;
        assert!(out.signals.is_empty());
        assert!(matches!(out.abstentions[0].1, AbstainReason::NoProximalReference));
    }
}
