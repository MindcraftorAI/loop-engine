//! Sentiment signal emission — output side of the orchestrator.
//!
//! 16a ships:
//! - [`SentimentSignal`] — the orchestrator's per-(session,lesson) output value
//! - [`OrchestratorOutput`] — structured per-event output (signals + abstention)
//! - [`SignalWriter`] trait + [`LoggingSignalWriter`] (writes via `tracing`)
//! - `MockSignalWriter` behind `test-fixtures`
//! - [`SignalWriteError`], [`AbstainReason`]
//!
//! 16b replaces `LoggingSignalWriter` with `StorageBackedSignalWriter`
//! that calls `lessons::record_sentiment_signal` via
//! `Storage::put_if_version`.

use std::sync::Arc;
#[cfg(any(test, feature = "test-fixtures"))]
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use thiserror::Error;
use tracing::info;

use crate::engine::context::Context;
use crate::engine::storage::{Storage, StorageKey};

use super::types::{AttributionMethod, CalibratedConfidence, Hazard, LoadedItemId, Polarity};

/// A sentiment signal — the orchestrator's per-(item,event) decision to
/// emit positive/negative feedback for a loaded item.
///
/// `#[non_exhaustive]`: future fields (e.g. attribution-evidence span,
/// classifier-name attribution) may grow.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SentimentSignal {
    pub item_id: LoadedItemId,
    pub polarity: Polarity,
    pub calibrated_confidence: CalibratedConfidence,
    pub attribution_method: AttributionMethod,
    pub detected_hazards: Vec<Hazard>,
    pub source_event_uuid: String,
    pub timestamp: DateTime<Utc>,
}

/// Reasons the orchestrator abstained from emitting a signal.
///
/// One per skipped-item OR one global. `#[non_exhaustive]` — Day 17
/// calibration may add reasons.
///
/// `PartialEq` only (NOT `Eq`) because the `BelowThreshold` variant
/// carries `f32`-valued thresholds and `f32` is not `Eq`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum AbstainReason {
    /// Pretrigger didn't fire — utterance had no sentiment-actionable signal.
    PretriggerNotFired,
    /// Classifier returned empty `per_item` (RawClassification::is_abstain).
    ClassifierAbstained,
    /// Confidence below polarity-asymmetric threshold.
    BelowThreshold {
        polarity: Polarity,
        observed: f32,
        required: f32,
    },
    /// One of the auto-abstain hazards fired (Sarcasm / AmbiguousReferent /
    /// OutOfDistribution / SelfDirected).
    HazardSet(Hazard),
    /// Attribution disagreed with the classifier's named item.
    AttributionMismatch,
    /// Attribution abstained (no pass fired).
    AttributionAbstained,
    /// Rate-limit cooldown still active for this (session, lesson).
    RateLimited,
    /// `Polarity::Neutral` is never emitted.
    Neutral,
    /// Day 16a D12 — UserInterrupt arrived but no proximal assistant turn
    /// referenced a loaded item (sentiment-design-rules rule 15).
    NoProximalReference,
    /// Day 16a audit C1 — session was concurrently removed (via
    /// `SessionEnded`) while a classifier call was in flight. The
    /// orchestrator skips the signal rather than panicking or
    /// resurrecting state for an ended session.
    SessionRecycled,
    /// Day 17 D4 — incoming `EngineEvent::UserTurn` carried a
    /// `host_version` outside the orchestrator's configured tested
    /// range AND policy `action = Abstain`. The whole turn is skipped.
    UntestedHostVersion,
}

/// Output of [`Orchestrator::process_event`] for one input event.
///
/// `#[non_exhaustive]`: future calibration may add fields.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct OrchestratorOutput {
    /// Emitted signals (may be multiple per event for correction-window
    /// negative-signal fan-out).
    pub signals: Vec<SentimentSignal>,
    /// Per-skipped-item abstention reasons (debugging + Day 17 calibration).
    pub abstentions: Vec<(Option<LoadedItemId>, AbstainReason)>,
}

impl OrchestratorOutput {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.signals.is_empty() && self.abstentions.is_empty()
    }
}

/// Errors from [`SignalWriter::record`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SignalWriteError {
    #[error("signal write backend error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl SignalWriteError {
    pub fn backend<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Backend(Box::new(err))
    }
}

/// Output sink for [`SentimentSignal`]s. The orchestrator calls this
/// to persist (or log, or test-capture) every emitted signal.
///
/// 16a ships:
///   - [`LoggingSignalWriter`] (writes to `tracing::info`)
///   - `MockSignalWriter` (test-fixtures feature)
///
/// 16b will add `StorageBackedSignalWriter` that calls
/// `lessons::record_sentiment_signal` via `Storage::put_if_version`.
#[async_trait]
pub trait SignalWriter: Send + Sync + std::fmt::Debug {
    async fn record(&self, ctx: &Context, signal: &SentimentSignal)
        -> Result<(), SignalWriteError>;
}

/// `tracing`-backed `SignalWriter`. Writes each signal as an INFO event
/// with structured fields. Used by the daemon in 16a before the
/// `StorageBackedSignalWriter` lands in 16b.
#[derive(Debug, Default)]
pub struct LoggingSignalWriter;

#[async_trait]
impl SignalWriter for LoggingSignalWriter {
    async fn record(
        &self,
        ctx: &Context,
        signal: &SentimentSignal,
    ) -> Result<(), SignalWriteError> {
        info!(
            tenant = %ctx.tenant_id,
            user = %ctx.user_id,
            session = %ctx.session_id,
            item = %signal.item_id,
            polarity = ?signal.polarity,
            confidence = signal.calibrated_confidence.value(),
            method = ?signal.attribution_method,
            hazards = ?signal.detected_hazards,
            source_event = %signal.source_event_uuid,
            "sentiment.signal"
        );
        Ok(())
    }
}

// =====================================================================
// StorageBackedSignalWriter (Day 16b D7) — persists each signal as a
// standalone YAML file under `signals/<session>/<event-uuid>.yaml`.
// Lesson-array aggregation deferred to Day 17.
// =====================================================================

/// Persists [`SentimentSignal`]s via the engine's [`Storage`] abstraction.
///
/// Phase A C7 — two writes per signal:
///  1. **Standalone file** at `signals/<session>/<event-uuid>.yaml` —
///     per-event audit-trail ledger (Day 16b behavior preserved). The
///     standalone file is rendered via `render_signal_yaml` and carries
///     the rich signal data (polarity, confidence, attribution method,
///     hazards, etc.) that the lesson YAML's signal array does NOT.
///  2. **Lesson append** via `lessons::record_signal` — adds the
///     source tag (`sentiment_positive` / `sentiment_negative`) to the
///     lesson's `external_signal_sources: Vec<String>`. This is the
///     deduplicated source-tag set the promotion gate consumes
///     (TS-parity per Phase A D3 / OQ-A7).
///
/// Polarity translation (D3 + OQ-A?):
/// - `Polarity::Positive` → `SignalPolarity::Positive` → tag `sentiment_positive`
/// - `Polarity::Negative` → `SignalPolarity::Negative` → tag `sentiment_negative`
/// - `Polarity::Neutral` → no aggregation (standalone write still happens
///   for audit trail, but the lesson's source set is not updated —
///   neutral isn't a directional signal the promotion gate cares about)
///
/// Failure handling: the standalone write happens FIRST so the audit
/// trail is preserved even if aggregation fails. Aggregation failures
/// (LessonNotFound, CAS budget exhaustion, etc.) bubble as
/// `SignalWriteError::backend` per Phase A OQ-A3.
#[derive(Debug, Clone)]
pub struct StorageBackedSignalWriter {
    storage: Arc<dyn Storage>,
}

impl StorageBackedSignalWriter {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl SignalWriter for StorageBackedSignalWriter {
    async fn record(
        &self,
        ctx: &Context,
        signal: &SentimentSignal,
    ) -> Result<(), SignalWriteError> {
        // === Write 1: standalone audit-trail file (always) ===
        let standalone_key =
            StorageKey::sentiment_signal(ctx, ctx.session_id.as_str(), &signal.source_event_uuid);
        let body = render_signal_yaml(signal);
        // Create-only: dedupe-as-success per Phase A OQ-A2 — duplicate
        // event_uuid returns Ok(false), which is fine; we treat the
        // first write as canonical and idempotently noop on the second.
        let _ok = self
            .storage
            .put_if_version(&standalone_key, Bytes::from(body), None)
            .await
            .map_err(SignalWriteError::backend)?;

        // === Write 2: lesson-array aggregation (Phase A C7) ===
        let agg_polarity = match signal.polarity {
            crate::engine::sentiment::types::Polarity::Positive => {
                Some(crate::engine::lessons::SignalPolarity::Positive)
            }
            crate::engine::sentiment::types::Polarity::Negative => {
                Some(crate::engine::lessons::SignalPolarity::Negative)
            }
            // Neutral signals don't update the lesson's source set —
            // they're not directional and the promotion gate ignores them.
            crate::engine::sentiment::types::Polarity::Neutral => None,
        };
        if let Some(polarity) = agg_polarity {
            crate::engine::lessons::record_signal(
                ctx,
                self.storage.as_ref(),
                signal.item_id.as_str(),
                polarity,
            )
            .await
            .map_err(|e| {
                SignalWriteError::backend(std::io::Error::other(format!(
                    "lesson aggregation failed for {}: {e}",
                    signal.item_id
                )))
            })?;
        }
        Ok(())
    }
}

/// Render a minimal YAML representation of a [`SentimentSignal`] for
/// persistence. Schema:
/// ```yaml
/// item_id: les-xxx
/// polarity: Positive
/// calibrated_confidence: 0.92
/// attribution_method: DirectMention
/// detected_hazards: [LowConfidence]
/// source_event_uuid: evt-1
/// timestamp: 2026-05-13T18:00:00Z
/// ```
fn render_signal_yaml(s: &SentimentSignal) -> String {
    // Phase A C2 (Day 16b L3 fix): Display impls in `types.rs` emit
    // snake_case strings. Schema-stable, not Debug-format-as-data.
    let hazards = if s.detected_hazards.is_empty() {
        "[]".to_string()
    } else {
        format!(
            "[{}]",
            s.detected_hazards
                .iter()
                .map(|h| h.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "item_id: {item}\n\
         polarity: {polarity}\n\
         calibrated_confidence: {conf}\n\
         attribution_method: {method}\n\
         detected_hazards: {hazards}\n\
         source_event_uuid: {uuid}\n\
         timestamp: {ts}\n",
        item = s.item_id,
        polarity = s.polarity,
        conf = s.calibrated_confidence.value(),
        method = s.attribution_method,
        hazards = hazards,
        uuid = s.source_event_uuid,
        ts = s.timestamp.to_rfc3339(),
    )
}

// =====================================================================
// MockSignalWriter — test fixture (D14)
// =====================================================================

/// Test/development fixture. Records every signal into an in-memory
/// vector for later assertion. Locked behind `test-fixtures`.
///
/// Builder-chain configurable to return errors for fault-injection
/// tests:
/// ```ignore
/// let mock = MockSignalWriter::default().with_record_error(SignalWriteError::backend(...));
/// ```
#[cfg(any(test, feature = "test-fixtures"))]
#[derive(Debug, Default)]
pub struct MockSignalWriter {
    captured: Mutex<Vec<SentimentSignal>>,
    /// If set, every `record` call returns this error instead of capturing.
    error: Mutex<Option<SignalWriteError>>,
}

#[cfg(any(test, feature = "test-fixtures"))]
impl MockSignalWriter {
    /// Snapshot the captured signals so far. Returns a clone (the mock
    /// continues to capture after this call).
    pub fn captured(&self) -> Vec<SentimentSignal> {
        self.captured
            .lock()
            .expect("MockSignalWriter mutex poisoned")
            .clone()
    }

    /// Set a one-shot record error. Subsequent calls fail with a clone
    /// of the error until `clear_record_error` is called.
    pub fn with_record_error(self, err: SignalWriteError) -> Self {
        *self.error.lock().expect("MockSignalWriter mutex poisoned") = Some(err);
        self
    }

    pub fn clear_record_error(&self) {
        *self.error.lock().expect("MockSignalWriter mutex poisoned") = None;
    }
}

#[cfg(any(test, feature = "test-fixtures"))]
#[async_trait]
impl SignalWriter for MockSignalWriter {
    async fn record(
        &self,
        _ctx: &Context,
        signal: &SentimentSignal,
    ) -> Result<(), SignalWriteError> {
        // If a one-shot error is set, return it. We don't clone the
        // error (SignalWriteError doesn't impl Clone) so we move it
        // out, leaving the slot empty for subsequent calls.
        let err = self
            .error
            .lock()
            .expect("MockSignalWriter mutex poisoned")
            .take();
        if let Some(e) = err {
            return Err(e);
        }
        self.captured
            .lock()
            .expect("MockSignalWriter mutex poisoned")
            .push(signal.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::sentiment::types::{
        AttributionMethod, CalibratedConfidence, Hazard, LoadedItemId, Polarity,
    };

    fn fake_signal(id: &str) -> SentimentSignal {
        SentimentSignal {
            item_id: LoadedItemId::new(id),
            polarity: Polarity::Positive,
            calibrated_confidence: CalibratedConfidence::new(0.9),
            attribution_method: AttributionMethod::DirectMention,
            detected_hazards: vec![Hazard::LowConfidence],
            source_event_uuid: "evt-1".into(),
            timestamp: Utc::now(),
        }
    }

    #[tokio::test]
    async fn logging_writer_returns_ok() {
        let w = LoggingSignalWriter;
        let ctx = Context::single_user_local();
        w.record(&ctx, &fake_signal("les-a")).await.unwrap();
    }

    #[tokio::test]
    async fn mock_writer_captures_signals_in_order() {
        let w = MockSignalWriter::default();
        let ctx = Context::single_user_local();
        w.record(&ctx, &fake_signal("les-a")).await.unwrap();
        w.record(&ctx, &fake_signal("les-b")).await.unwrap();
        let captured = w.captured();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].item_id.as_str(), "les-a");
        assert_eq!(captured[1].item_id.as_str(), "les-b");
    }

    #[tokio::test]
    async fn mock_writer_returns_one_shot_error_then_resumes() {
        let w = MockSignalWriter::default()
            .with_record_error(SignalWriteError::backend(std::io::Error::other("boom")));
        let ctx = Context::single_user_local();

        let err_result = w.record(&ctx, &fake_signal("les-a")).await;
        assert!(matches!(err_result, Err(SignalWriteError::Backend(_))));
        // Second call captures normally (one-shot).
        w.record(&ctx, &fake_signal("les-b")).await.unwrap();
        assert_eq!(w.captured().len(), 1);
        assert_eq!(w.captured()[0].item_id.as_str(), "les-b");
    }

    #[test]
    fn output_empty_helpers() {
        let empty = OrchestratorOutput::empty();
        assert!(empty.is_empty());
        let with_signals = OrchestratorOutput {
            signals: vec![fake_signal("a")],
            abstentions: vec![],
        };
        assert!(!with_signals.is_empty());
    }

    // ---- StorageBackedSignalWriter (Day 16b D7) ----

    use crate::engine::storage::MemoryStorage;

    /// Seed a minimal valid lesson so the C7 aggregation path can find
    /// it. Tests that don't pre-seed will get LessonNotFound bubbled
    /// out of `StorageBackedSignalWriter::record` (which is the
    /// intentional Phase A behavior per OQ-A3).
    async fn seed_minimum_lesson(storage: &Arc<dyn Storage>, ctx: &Context, id: &str) {
        let key = StorageKey::lesson(ctx, "active", id);
        let yaml = format!(
            "---\n\
             id: {id}\n\
             description: \"test\"\n\
             status: active\n\
             created_at: \"2026-05-13T00:00:00.000Z\"\n\
             applied_count: 0\n\
             thumbs_up_count: 0\n\
             thumbs_down_count: 0\n\
             external_signal_sources: []\n\
             ---\n\
             body\n"
        );
        storage.put(&key, Bytes::from(yaml)).await.unwrap();
    }

    #[tokio::test]
    async fn storage_backed_writer_persists_signal_to_storage() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let writer = StorageBackedSignalWriter::new(storage.clone());
        let ctx = Context::single_user_local();
        seed_minimum_lesson(&storage, &ctx, "les-quokka-special").await;
        let signal = fake_signal("les-quokka-special");

        writer.record(&ctx, &signal).await.unwrap();

        // Write 1: standalone audit-trail file.
        let key =
            StorageKey::sentiment_signal(&ctx, ctx.session_id.as_str(), &signal.source_event_uuid);
        let stored = storage.get(&key).await.unwrap().unwrap();
        let body = std::str::from_utf8(&stored).unwrap();
        assert!(body.contains("item_id: les-quokka-special"));
        assert!(body.contains("polarity: positive"));
        assert!(body.contains("attribution_method: direct_mention"));
        assert!(body.contains("detected_hazards: [low_confidence]"));
        assert!(body.contains("source_event_uuid: evt-1"));

        // Write 2: lesson aggregation appended `sentiment_positive`.
        let lesson =
            crate::engine::lessons::get_by_id(&ctx, storage.as_ref(), "les-quokka-special")
                .await
                .unwrap()
                .expect("lesson should still exist");
        assert_eq!(
            lesson.frontmatter.external_signal_sources,
            vec!["sentiment_positive".to_string()]
        );
    }

    #[tokio::test]
    async fn storage_backed_writer_aggregates_negative_polarity() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let writer = StorageBackedSignalWriter::new(storage.clone());
        let ctx = Context::single_user_local();
        seed_minimum_lesson(&storage, &ctx, "les-neg-agg").await;
        let mut signal = fake_signal("les-neg-agg");
        signal.polarity = Polarity::Negative;

        writer.record(&ctx, &signal).await.unwrap();

        let lesson = crate::engine::lessons::get_by_id(&ctx, storage.as_ref(), "les-neg-agg")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            lesson.frontmatter.external_signal_sources,
            vec!["sentiment_negative".to_string()]
        );
    }

    #[tokio::test]
    async fn storage_backed_writer_skips_aggregation_for_neutral_polarity() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let writer = StorageBackedSignalWriter::new(storage.clone());
        let ctx = Context::single_user_local();
        seed_minimum_lesson(&storage, &ctx, "les-neut-agg").await;
        let mut signal = fake_signal("les-neut-agg");
        signal.polarity = Polarity::Neutral;

        writer.record(&ctx, &signal).await.unwrap();

        let key =
            StorageKey::sentiment_signal(&ctx, ctx.session_id.as_str(), &signal.source_event_uuid);
        assert!(storage.get(&key).await.unwrap().is_some());
        let lesson = crate::engine::lessons::get_by_id(&ctx, storage.as_ref(), "les-neut-agg")
            .await
            .unwrap()
            .unwrap();
        assert!(lesson.frontmatter.external_signal_sources.is_empty());
    }

    #[tokio::test]
    async fn storage_backed_writer_bubbles_when_lesson_missing() {
        // OQ-A3: aggregation failure on LessonNotFound bubbles as
        // SignalWriteError. Standalone file IS still written.
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let writer = StorageBackedSignalWriter::new(storage.clone());
        let ctx = Context::single_user_local();
        let signal = fake_signal("les-nofile99");
        let result = writer.record(&ctx, &signal).await;
        assert!(matches!(result, Err(SignalWriteError::Backend(_))));
        let key =
            StorageKey::sentiment_signal(&ctx, ctx.session_id.as_str(), &signal.source_event_uuid);
        assert!(storage.get(&key).await.unwrap().is_some());
    }

    #[test]
    fn polarity_display_is_snake_case() {
        use crate::engine::sentiment::types::Polarity;
        assert_eq!(Polarity::Positive.to_string(), "positive");
        assert_eq!(Polarity::Negative.to_string(), "negative");
        assert_eq!(Polarity::Neutral.to_string(), "neutral");
    }

    #[test]
    fn hazard_display_is_snake_case() {
        assert_eq!(Hazard::Sarcasm.to_string(), "sarcasm");
        assert_eq!(Hazard::AmbiguousReferent.to_string(), "ambiguous_referent");
        assert_eq!(Hazard::SelfDirected.to_string(), "self_directed");
        assert_eq!(Hazard::OutOfDistribution.to_string(), "out_of_distribution");
    }

    #[test]
    fn attribution_method_display_is_snake_case() {
        use crate::engine::sentiment::types::AttributionMethod;
        assert_eq!(
            AttributionMethod::DirectMention.to_string(),
            "direct_mention"
        );
        assert_eq!(
            AttributionMethod::PronounResolved.to_string(),
            "pronoun_resolved"
        );
        assert_eq!(AttributionMethod::Recency.to_string(), "recency");
        assert_eq!(AttributionMethod::Salience.to_string(), "salience");
    }

    #[tokio::test]
    async fn storage_backed_writer_dedups_on_same_event_uuid() {
        // Same source_event_uuid → standalone-file dedup (put_if_version
        // with expected=None returns Ok(false) when file exists).
        // Phase A C7: both signal IDs need lessons seeded so the
        // aggregation path doesn't bubble LessonNotFound.
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let writer = StorageBackedSignalWriter::new(storage.clone());
        let ctx = Context::single_user_local();
        seed_minimum_lesson(&storage, &ctx, "les-a").await;
        seed_minimum_lesson(&storage, &ctx, "les-b").await;
        let first = fake_signal("les-a");
        let mut second = fake_signal("les-b");
        second.source_event_uuid = first.source_event_uuid.clone();

        writer.record(&ctx, &first).await.unwrap();
        writer.record(&ctx, &second).await.unwrap();

        let key =
            StorageKey::sentiment_signal(&ctx, ctx.session_id.as_str(), &first.source_event_uuid);
        let stored = storage.get(&key).await.unwrap().unwrap();
        let body = std::str::from_utf8(&stored).unwrap();
        // First write wins (item les-a, not les-b).
        assert!(body.contains("item_id: les-a"));
    }
}
