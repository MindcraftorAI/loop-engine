//! Sentiment classifier — sealed async trait.
//!
//! Locked decisions (learn-notes D3 + D8):
//! - Sealed via `sealed::Sealed`
//! - `async_trait` macro (consistency with Day 14 Storage + EventSource)
//! - Object-safe — held as `Arc<dyn SentimentClassifier>`
//! - Takes `&Context` for forward-feed (per-tenant overrides etc.)
//! - Takes `&ClassificationRequest` (owned, bounded, ships across `.await`)
//! - Returns `Result<RawClassification, ClassifierError>` — named error enum
//! - Production impls live in host adapters; engine ships only
//!   [`MockSentimentClassifier`] (behind `test-fixtures` feature) here

#[cfg(any(test, feature = "test-fixtures"))]
use std::collections::VecDeque;
#[cfg(any(test, feature = "test-fixtures"))]
use std::sync::Mutex;
#[cfg(any(test, feature = "test-fixtures"))]
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use thiserror::Error;

use crate::engine::context::Context;

use super::types::{ClassificationRequest, RawClassification};

/// Errors a [`SentimentClassifier`] may return.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClassifierError {
    #[error("classifier transport error: {0}")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("classifier returned unparseable output: {0}")]
    InvalidOutput(String),

    #[error("classifier rate-limited; retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u32 },

    #[error("classifier timeout after {elapsed_ms}ms")]
    Timeout { elapsed_ms: u64 },
}

impl ClassifierError {
    pub fn transport<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Transport(Box::new(err))
    }
}

/// Sentiment classifier — sealed async trait. Production impls live in
/// host adapters (e.g. `host::claude_code::haiku_client`). The engine
/// ships only the [`MockSentimentClassifier`] test fixture (behind
/// the `test-fixtures` Cargo feature).
#[async_trait]
pub trait SentimentClassifier: Send + Sync + std::fmt::Debug + sealed::Sealed {
    /// Classify the curated request. Returns
    /// [`RawClassification::abstain`] when the classifier had nothing
    /// confident to report (preferred over throwing for the no-signal case).
    async fn classify(
        &self,
        ctx: &Context,
        request: &ClassificationRequest,
    ) -> Result<RawClassification, ClassifierError>;

    /// Diagnostic name used in logs and rate-limit accounting.
    fn name(&self) -> &'static str;
}

pub(crate) mod sealed {
    pub trait Sealed {}
}

// =====================================================================
// MockSentimentClassifier — test fixture (D6)
// =====================================================================

/// Test/development fixture. Returns canned [`RawClassification`]s in
/// the order they were enqueued; once the queue is exhausted, returns
/// `RawClassification::abstain()`.
///
/// Builder-chain API (per OQ2):
/// ```ignore
/// let mock = MockSentimentClassifier::default()
///     .with_response(RawClassification::abstain())
///     .with_response(some_classification);
/// ```
#[cfg(any(test, feature = "test-fixtures"))]
#[derive(Debug, Default)]
pub struct MockSentimentClassifier {
    responses: Mutex<VecDeque<Result<RawClassification, ClassifierError>>>,
    call_count: AtomicUsize,
}

#[cfg(any(test, feature = "test-fixtures"))]
impl MockSentimentClassifier {
    /// Enqueue a classification result. Returned in FIFO order.
    pub fn with_response(self, response: RawClassification) -> Self {
        self.responses
            .lock()
            .expect("MockSentimentClassifier mutex poisoned")
            .push_back(Ok(response));
        self
    }

    /// Enqueue an error. Returned in FIFO order with `with_response`.
    pub fn with_error(self, error: ClassifierError) -> Self {
        self.responses
            .lock()
            .expect("MockSentimentClassifier mutex poisoned")
            .push_back(Err(error));
        self
    }

    /// Number of times [`SentimentClassifier::classify`] has been called.
    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::Relaxed)
    }
}

#[cfg(any(test, feature = "test-fixtures"))]
impl sealed::Sealed for MockSentimentClassifier {}

#[cfg(any(test, feature = "test-fixtures"))]
#[async_trait]
impl SentimentClassifier for MockSentimentClassifier {
    async fn classify(
        &self,
        _ctx: &Context,
        _request: &ClassificationRequest,
    ) -> Result<RawClassification, ClassifierError> {
        self.call_count.fetch_add(1, Ordering::Relaxed);
        let next = self
            .responses
            .lock()
            .expect("MockSentimentClassifier mutex poisoned")
            .pop_front();
        match next {
            Some(result) => result,
            None => Ok(RawClassification::abstain()),
        }
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::sentiment::types::{
        ClassifierConfidence, Hazard, ItemClassification, LoadedItemId, Polarity,
        RawClassification,
    };

    fn empty_request() -> ClassificationRequest {
        ClassificationRequest {
            utterance: String::new(),
            loaded_items: vec![],
            recent_turns: vec![],
        }
    }

    #[tokio::test]
    async fn mock_returns_abstain_when_queue_empty() {
        let mock = MockSentimentClassifier::default();
        let ctx = Context::single_user_local();
        let result = mock.classify(&ctx, &empty_request()).await.unwrap();
        assert!(result.is_abstain());
        assert_eq!(mock.call_count(), 1);
    }

    #[tokio::test]
    async fn mock_returns_canned_responses_in_order() {
        let canned = RawClassification {
            per_item: vec![ItemClassification {
                item_id: LoadedItemId::new("les-a"),
                polarity: Polarity::Positive,
                confidence: ClassifierConfidence::new(0.9),
                evidence: Some("thanks".into()),
                hazards: vec![],
            }],
            global_hazards: vec![],
        };
        let mock = MockSentimentClassifier::default()
            .with_response(canned.clone())
            .with_response(RawClassification::abstain());

        let ctx = Context::single_user_local();
        let first = mock.classify(&ctx, &empty_request()).await.unwrap();
        let second = mock.classify(&ctx, &empty_request()).await.unwrap();
        let third = mock.classify(&ctx, &empty_request()).await.unwrap();

        assert_eq!(first, canned);
        assert!(second.is_abstain());
        assert!(third.is_abstain()); // queue exhausted; fallback to abstain
        assert_eq!(mock.call_count(), 3);
    }

    #[tokio::test]
    async fn mock_returns_canned_error() {
        let mock = MockSentimentClassifier::default()
            .with_error(ClassifierError::RateLimited { retry_after_secs: 5 });
        let ctx = Context::single_user_local();
        let result = mock.classify(&ctx, &empty_request()).await;
        assert!(matches!(
            result,
            Err(ClassifierError::RateLimited { retry_after_secs: 5 })
        ));
    }

    #[test]
    fn mock_name_is_stable() {
        let mock = MockSentimentClassifier::default();
        assert_eq!(mock.name(), "mock");
    }

    #[test]
    fn classifier_trait_is_object_safe() {
        // Compile-time check: if `SentimentClassifier` is object-unsafe
        // this won't build.
        let mock: std::sync::Arc<dyn SentimentClassifier> =
            std::sync::Arc::new(MockSentimentClassifier::default());
        assert_eq!(mock.name(), "mock");
    }

    #[test]
    fn hazard_variants_are_distinguishable() {
        let h1 = Hazard::Sarcasm;
        let h2 = Hazard::AmbiguousReferent;
        assert_ne!(h1, h2);
    }
}
