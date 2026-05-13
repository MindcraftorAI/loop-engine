//! Sentiment orchestrator — per-session state machine + classifier wiring.
//!
//! Locked decisions (`docs/research/day-16a-learn-notes.md`):
//! - D2: `Arc<DashMap<SessionId, Mutex<SessionState>>>` keyed mutable state
//! - D4: hand-rolled per-(session, lesson) rate limit
//! - D5: `std::sync::Mutex` — critical sections NEVER `.await`
//! - D9: hazard auto-abstain = Sarcasm | AmbiguousReferent | OutOfDistribution | SelfDirected
//! - D10: polarity-asymmetric thresholds in `derive`
//! - D11: attribution cross-check via Day 15 `attribute_signal`
//! - D12: correction-window mining on `UserInterrupt`
//! - D13: `SignalWriter` trait sink (16a: Logging/Mock; 16b: StorageBacked)
//!
//! Critical-section discipline (smell S22 / S23 + audit C1):
//! ```text
//!     lock → snapshot/mutate → DROP → await classifier → re-lock → apply
//! ```
//! Enforced by `#![deny(clippy::await_holding_lock)]` below.
//!
//! Audit-fix discipline:
//! - **C1 (race)**: critical section 2 uses `if let Some(entry) = ...` and
//!   abstains with `SessionRecycled` if the session was concurrently
//!   removed via `SessionEnded`. No panic.
//! - **C2 + M3 + M4**: the orchestrator can emit signals once a caller
//!   wires manifest items via [`Orchestrator::update_manifest`] and
//!   assistant turns via [`Orchestrator::push_assistant_turn`].
//!   Integration tests below exercise this path end-to-end.

#![deny(clippy::await_holding_lock)]
#![warn(clippy::significant_drop_in_scrutinee)]
#![warn(clippy::mut_mutex_lock)]

mod config;
mod derive;
mod state;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::Utc;
use dashmap::DashMap;
use tracing::warn;

use crate::engine::context::{Context, SessionId};
use crate::engine::events::EngineEvent;

use super::classifier::{ClassifierError, SentimentClassifier};
use super::signals::{
    AbstainReason, OrchestratorOutput, SentimentSignal, SignalWriteError, SignalWriter,
};
use super::types::{
    AttributionMethod, CalibratedConfidence, ClassificationRequest, LoadedItem, LoadedItemId,
    Polarity, RecentTurn, TurnRole,
};

pub use config::{HostVersionAction, HostVersionPolicy, OrchestratorConfig};
use derive::derive_signals;
use state::{push_turn, SessionPhase, SessionState};

// ---------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------

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

    /// Inject (or replace) the active manifest for a session. Called by
    /// the manifest-assembly layer (Day 16b+) when items load/unload.
    /// Tests use this to seed `loaded_items` before exercising the
    /// signal-emit path.
    pub fn update_manifest(&self, session_id: &SessionId, items: Vec<LoadedItem>) {
        let entry = self.inner.sessions.entry(session_id.clone()).or_default();
        let mut state = entry.lock().expect("session state mutex poisoned");
        state.loaded_items = items;
    }

    /// Record an assistant turn for the session. Called by the manifest +
    /// reasoning loop (Day 16b+) when the assistant emits a turn that
    /// references items. Updates `recent_turns` so correction-window
    /// mining can find proximal assistant references.
    pub fn push_assistant_turn(
        &self,
        session_id: &SessionId,
        text: String,
        referenced_items: Vec<LoadedItemId>,
    ) {
        let entry = self.inner.sessions.entry(session_id.clone()).or_default();
        let mut state = entry.lock().expect("session state mutex poisoned");
        push_turn(
            &mut state,
            self.inner.config.recent_turn_capacity,
            RecentTurn {
                role: TurnRole::Assistant,
                text,
                referenced_items,
            },
        );
    }

    /// Process one engine event. Dispatches by variant.
    pub async fn process_event(
        &self,
        ctx: &Context,
        event: &EngineEvent,
    ) -> OrchestratorOutput {
        match event {
            EngineEvent::UserTurn { .. } => self.handle_user_turn(ctx, event).await,
            EngineEvent::UserInterrupt { .. } => self.handle_user_interrupt(ctx, event).await,
            EngineEvent::SessionEnded { session_id } => {
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
            event_uuid,
            text,
            host_version,
            ..
        } = event
        else {
            unreachable!("dispatch guarantees UserTurn here");
        };

        // Day 17 D4: HostVersion tripwire. Fires BEFORE the classifier
        // call so we don't pay the LLM-latency cost on an out-of-range
        // host version we don't trust. When policy is off (default),
        // this is a no-op.
        if let Some(hv) = host_version {
            let policy = &self.inner.config.host_version_policy;
            if policy.is_out_of_range(hv.as_str()) {
                match policy.action {
                    HostVersionAction::Warn => {
                        warn!(
                            host_version = %hv,
                            "host version outside tested range (warn-only)"
                        );
                    }
                    HostVersionAction::Abstain => {
                        warn!(
                            host_version = %hv,
                            event = %event_uuid,
                            "host version outside tested range; abstaining"
                        );
                        return OrchestratorOutput {
                            signals: vec![],
                            abstentions: vec![(None, AbstainReason::UntestedHostVersion)],
                        };
                    }
                }
            }
        }

        // === Critical section 1: append turn, snapshot manifest + request ===
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
            ClassificationRequest {
                utterance: text.clone(),
                loaded_items: state.loaded_items.clone(),
                recent_turns: state.recent_turns.iter().cloned().collect(),
            }
        }; // <-- lock dropped here

        // === Async classifier call OUTSIDE the lock ===
        let raw = match self.inner.classifier.classify(ctx, &request).await {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    classifier = self.inner.classifier.name(),
                    err = %DisplayClassifierError(&e),
                    "classifier error; abstaining for this turn",
                );
                self.reset_phase_idle(&ctx.session_id);
                return OrchestratorOutput {
                    signals: vec![],
                    abstentions: vec![(None, AbstainReason::ClassifierAbstained)],
                };
            }
        };

        // === Critical section 2: derive signals (RACE-SAFE per audit C1) ===
        let (signals, abstentions) = if let Some(entry) =
            self.inner.sessions.get(&ctx.session_id)
        {
            let mut state = entry.lock().expect("session state mutex poisoned");
            let now = Instant::now();
            let outcome = derive_signals(
                &raw,
                &request,
                &state.rate_limit,
                &self.inner.config,
                now,
                event_uuid,
            );
            for sig in &outcome.signals {
                state.rate_limit.insert(sig.item_id.clone(), now);
            }
            state.phase = SessionPhase::Idle;
            state.turn_count += 1;
            (outcome.signals, outcome.abstentions)
        } else {
            // Audit C1 fix: session was removed via SessionEnded while
            // the classifier call was in flight. Abstain rather than
            // resurrect state for an ended session.
            (vec![], vec![(None, AbstainReason::SessionRecycled)])
        };

        for sig in &signals {
            if let Err(e) = self.inner.writer.record(ctx, sig).await {
                warn!(
                    item = %sig.item_id,
                    err = %DisplaySignalWriteError(&e),
                    "signal writer error; signal dropped",
                );
            }
        }

        OrchestratorOutput { signals, abstentions }
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

            let proximal = state
                .recent_turns
                .iter()
                .rev()
                .find(|t| t.role == TurnRole::Assistant && !t.referenced_items.is_empty());

            let Some(turn) = proximal else {
                state.turn_count += 1;
                return OrchestratorOutput {
                    signals: vec![],
                    abstentions: vec![(None, AbstainReason::NoProximalReference)],
                };
            };

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

            let item_ids: Vec<LoadedItemId> = turn.referenced_items.clone();
            let mut signals = Vec::with_capacity(item_ids.len());
            let mut abstentions = Vec::new();
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
                warn!(
                    item = %sig.item_id,
                    err = %DisplaySignalWriteError(&e),
                    "signal writer error; signal dropped",
                );
            }
        }

        OrchestratorOutput { signals, abstentions }
    }

    fn reset_phase_idle(&self, session_id: &SessionId) {
        if let Some(entry) = self.inner.sessions.get(session_id) {
            let mut state = entry.lock().expect("session state mutex poisoned");
            state.phase = SessionPhase::Idle;
        }
    }

    /// Test/debug helper: number of live sessions tracked.
    /// Gated behind `cfg(test)` / `test-fixtures` per audit m9 — not
    /// part of the production surface. `#[allow(dead_code)]` since
    /// the only caller is `#[cfg(test)]` inline tests; clippy's
    /// dead-code lint doesn't see those as users.
    #[cfg(any(test, feature = "test-fixtures"))]
    #[allow(dead_code)]
    pub(crate) fn session_count(&self) -> usize {
        self.inner.sessions.len()
    }
}

// Display wrappers — tracing's `err = %<x>` needs `Display`, and the
// underlying error types impl `Display` already, but the wrappers
// document intent.
struct DisplayClassifierError<'a>(&'a ClassifierError);
impl std::fmt::Display for DisplayClassifierError<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self.0, f)
    }
}
struct DisplaySignalWriteError<'a>(&'a SignalWriteError);
impl std::fmt::Display for DisplaySignalWriteError<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self.0, f)
    }
}

// =====================================================================
// Integration tests — exercise the full signal-emit path
// (audit C2 + M3 fix)
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::sentiment::classifier::MockSentimentClassifier;
    use crate::engine::sentiment::signals::MockSignalWriter;
    use crate::engine::sentiment::types::{
        ClassifierConfidence, Hazard, ItemClassification, LoadedItem, LoadedItemKind,
        RawClassification,
    };

    fn orchestrator_with_mocks() -> (
        Orchestrator,
        Arc<MockSentimentClassifier>,
        Arc<MockSignalWriter>,
    ) {
        let classifier = Arc::new(MockSentimentClassifier::default());
        let writer = Arc::new(MockSignalWriter::default());
        let orch = Orchestrator::new(
            classifier.clone() as Arc<dyn SentimentClassifier>,
            writer.clone() as Arc<dyn SignalWriter>,
            OrchestratorConfig::default(),
        );
        (orch, classifier, writer)
    }

    fn item(id: &str, keywords: &[&str]) -> LoadedItem {
        LoadedItem {
            id: LoadedItemId::new(id),
            kind: LoadedItemKind::Lesson,
            label: id.into(),
            keywords: keywords.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn positive_classification(id: &str, conf: f32) -> RawClassification {
        RawClassification {
            per_item: vec![ItemClassification {
                item_id: LoadedItemId::new(id),
                polarity: Polarity::Positive,
                confidence: ClassifierConfidence::new(conf),
                evidence: None,
                hazards: vec![],
            }],
            global_hazards: vec![],
        }
    }

    fn user_turn_event(session_id: &SessionId, uuid: &str, text: &str) -> EngineEvent {
        EngineEvent::UserTurn {
            session_id: session_id.clone(),
            event_uuid: uuid.into(),
            parent_event_uuid: None,
            text: text.into(),
            timestamp: Utc::now(),
            cwd: None,
            host_version: None,
            project_tag: None,
        }
    }

    // ---- end-to-end: signal IS emitted when full path passes ----

    #[tokio::test]
    async fn emits_signal_when_manifest_classifier_and_attribution_all_agree() {
        let (orch, classifier_arc, writer) = {
            // Build a classifier with a canned positive hit on "les-quokka-special".
            let classifier = MockSentimentClassifier::default().with_response(
                positive_classification("les-quokka-special", 0.92),
            );
            let classifier = Arc::new(classifier);
            let writer = Arc::new(MockSignalWriter::default());
            let orch = Orchestrator::new(
                classifier.clone() as Arc<dyn SentimentClassifier>,
                writer.clone() as Arc<dyn SignalWriter>,
                OrchestratorConfig::default(),
            );
            (orch, classifier, writer)
        };

        let ctx = Context::single_user_local();

        // Seed the manifest so attribution can match.
        orch.update_manifest(
            &ctx.session_id,
            vec![item("les-quokka-special", &["quokka-special"])],
        );

        let out = orch
            .process_event(
                &ctx,
                &user_turn_event(&ctx.session_id, "evt-1", "thanks for quokka-special"),
            )
            .await;

        assert_eq!(out.signals.len(), 1, "expected one signal emitted");
        assert_eq!(out.signals[0].item_id.as_str(), "les-quokka-special");
        assert_eq!(out.signals[0].polarity, Polarity::Positive);
        assert!(out.signals[0].calibrated_confidence.value() >= 0.75);
        assert_eq!(classifier_arc.call_count(), 1);

        let captured = writer.captured();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].item_id.as_str(), "les-quokka-special");
    }

    #[tokio::test]
    async fn emits_correction_window_negative_after_assistant_then_interrupt() {
        let (orch, _classifier, writer) = orchestrator_with_mocks();
        let ctx = Context::single_user_local();
        // Simulate an assistant turn referencing "les-zebra-unique".
        orch.push_assistant_turn(
            &ctx.session_id,
            "I applied the les-zebra-unique fix".into(),
            vec![LoadedItemId::new("les-zebra-unique")],
        );
        // Now the user interrupts within the correction window.
        let interrupt = EngineEvent::UserInterrupt {
            session_id: ctx.session_id.clone(),
            event_uuid: "i1".into(),
            parent_event_uuid: None,
            timestamp: Utc::now(),
        };
        let out = orch.process_event(&ctx, &interrupt).await;
        assert_eq!(out.signals.len(), 1, "expected one negative signal");
        assert_eq!(out.signals[0].polarity, Polarity::Negative);
        assert_eq!(out.signals[0].item_id.as_str(), "les-zebra-unique");
        assert_eq!(writer.captured().len(), 1);
    }

    #[tokio::test]
    async fn auto_abstain_hazard_suppresses_signal_even_at_high_confidence() {
        let mut classification = positive_classification("les-quokka-special", 0.99);
        classification.per_item[0].hazards = vec![Hazard::Sarcasm];
        let classifier = Arc::new(MockSentimentClassifier::default().with_response(classification));
        let writer = Arc::new(MockSignalWriter::default());
        let orch = Orchestrator::new(
            classifier.clone() as Arc<dyn SentimentClassifier>,
            writer.clone() as Arc<dyn SignalWriter>,
            OrchestratorConfig::default(),
        );

        let ctx = Context::single_user_local();
        orch.update_manifest(
            &ctx.session_id,
            vec![item("les-quokka-special", &["quokka-special"])],
        );
        let out = orch
            .process_event(
                &ctx,
                &user_turn_event(&ctx.session_id, "evt-1", "thanks for quokka-special"),
            )
            .await;
        assert!(out.signals.is_empty());
        assert!(writer.captured().is_empty());
        assert!(matches!(
            out.abstentions[0].1,
            AbstainReason::HazardSet(Hazard::Sarcasm)
        ));
    }

    // ---- existing behavior (preserved) ----

    #[tokio::test]
    async fn session_ended_drops_state() {
        let (orch, _, _) = orchestrator_with_mocks();
        let ctx = Context::single_user_local();
        orch.process_event(&ctx, &user_turn_event(&ctx.session_id, "e1", "hello"))
            .await;
        assert_eq!(orch.session_count(), 1);
        let end = EngineEvent::SessionEnded {
            session_id: ctx.session_id.clone(),
        };
        orch.process_event(&ctx, &end).await;
        assert_eq!(orch.session_count(), 0);
    }

    #[tokio::test]
    async fn abstains_when_classifier_returns_abstain() {
        let (orch, _, writer) = orchestrator_with_mocks();
        let ctx = Context::single_user_local();
        let out = orch
            .process_event(&ctx, &user_turn_event(&ctx.session_id, "e1", "thanks"))
            .await;
        assert!(out.signals.is_empty());
        assert!(matches!(
            out.abstentions[0].1,
            AbstainReason::ClassifierAbstained
        ));
        assert!(writer.captured().is_empty());
    }

    #[tokio::test]
    async fn user_interrupt_no_proximal_assistant_abstains() {
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
        assert!(matches!(
            out.abstentions[0].1,
            AbstainReason::NoProximalReference
        ));
    }

    // ---- Day 17 D4: HostVersion tripwire ----

    fn user_turn_event_with_host_version(
        session_id: &SessionId,
        version: &str,
    ) -> EngineEvent {
        EngineEvent::UserTurn {
            session_id: session_id.clone(),
            event_uuid: "evt-tripwire".into(),
            parent_event_uuid: None,
            text: "thanks".into(),
            timestamp: Utc::now(),
            cwd: None,
            host_version: Some(crate::engine::events::HostVersion::new(version.to_string())),
            project_tag: None,
        }
    }

    #[tokio::test]
    async fn tripwire_off_by_default_no_abstain() {
        let (orch, _, _) = orchestrator_with_mocks();
        let ctx = Context::single_user_local();
        // Default policy is off — even an "obviously bad" version passes through.
        let out = orch
            .process_event(&ctx, &user_turn_event_with_host_version(&ctx.session_id, "0.0.0"))
            .await;
        // Mock classifier abstains (empty queue) — verifies tripwire didn't fire its own abstain.
        assert!(out.abstentions.iter().all(|(_, r)| !matches!(r, AbstainReason::UntestedHostVersion)));
    }

    #[tokio::test]
    async fn tripwire_warn_action_does_not_abstain() {
        let classifier: Arc<dyn SentimentClassifier> =
            Arc::new(MockSentimentClassifier::default());
        let writer: Arc<dyn SignalWriter> = Arc::new(MockSignalWriter::default());
        let config = OrchestratorConfig {
            host_version_policy: HostVersionPolicy {
                tested_range: Some("2.0.0".to_string()..="2.1.999".to_string()),
                action: HostVersionAction::Warn,
            },
            ..OrchestratorConfig::default()
        };
        let orch = Orchestrator::new(classifier, writer, config);
        let ctx = Context::single_user_local();
        // "1.0.0" is below the tested range — Warn action means we still process.
        let out = orch
            .process_event(&ctx, &user_turn_event_with_host_version(&ctx.session_id, "1.0.0"))
            .await;
        assert!(
            out.abstentions
                .iter()
                .all(|(_, r)| !matches!(r, AbstainReason::UntestedHostVersion)),
            "warn-action should NOT abstain"
        );
    }

    #[tokio::test]
    async fn tripwire_abstain_action_skips_turn_for_out_of_range_version() {
        let classifier: Arc<dyn SentimentClassifier> =
            Arc::new(MockSentimentClassifier::default());
        let writer: Arc<dyn SignalWriter> = Arc::new(MockSignalWriter::default());
        let config = OrchestratorConfig {
            host_version_policy: HostVersionPolicy {
                tested_range: Some("2.0.0".to_string()..="2.1.999".to_string()),
                action: HostVersionAction::Abstain,
            },
            ..OrchestratorConfig::default()
        };
        let orch = Orchestrator::new(classifier, writer, config);
        let ctx = Context::single_user_local();
        let out = orch
            .process_event(&ctx, &user_turn_event_with_host_version(&ctx.session_id, "9.9.9"))
            .await;
        assert!(out.signals.is_empty());
        assert_eq!(out.abstentions.len(), 1);
        assert!(matches!(out.abstentions[0].1, AbstainReason::UntestedHostVersion));
    }

    #[tokio::test]
    async fn tripwire_abstain_action_passes_through_in_range_version() {
        let classifier: Arc<dyn SentimentClassifier> =
            Arc::new(MockSentimentClassifier::default());
        let writer: Arc<dyn SignalWriter> = Arc::new(MockSignalWriter::default());
        let config = OrchestratorConfig {
            host_version_policy: HostVersionPolicy {
                tested_range: Some("2.0.0".to_string()..="2.1.999".to_string()),
                action: HostVersionAction::Abstain,
            },
            ..OrchestratorConfig::default()
        };
        let orch = Orchestrator::new(classifier, writer, config);
        let ctx = Context::single_user_local();
        let out = orch
            .process_event(&ctx, &user_turn_event_with_host_version(&ctx.session_id, "2.1.139"))
            .await;
        // No UntestedHostVersion abstain — mock classifier's empty-queue
        // ClassifierAbstained is the only abstention here.
        assert!(out
            .abstentions
            .iter()
            .all(|(_, r)| !matches!(r, AbstainReason::UntestedHostVersion)));
    }
}
