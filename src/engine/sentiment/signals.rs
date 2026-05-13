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

use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;
use tracing::info;

use crate::engine::context::Context;

use super::types::{
    AttributionMethod, CalibratedConfidence, Hazard, LoadedItemId, Polarity,
};

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
    BelowThreshold { polarity: Polarity, observed: f32, required: f32 },
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
    async fn record(
        &self,
        ctx: &Context,
        signal: &SentimentSignal,
    ) -> Result<(), SignalWriteError>;
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
        *self
            .error
            .lock()
            .expect("MockSignalWriter mutex poisoned") = Some(err);
        self
    }

    pub fn clear_record_error(&self) {
        *self
            .error
            .lock()
            .expect("MockSignalWriter mutex poisoned") = None;
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
}
