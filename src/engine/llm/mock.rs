//! `MockLlmClient` — test fixture / dev shim behind `test-fixtures`.
//!
//! Phase D D-D8: builder-chain matching `MockSentimentClassifier`.
//! `default()` is constructable; `with_response(Generation)` and
//! `with_error(LlmError)` are FIFO-queued. Empty queue falls back to a
//! debug-stub `Generation::Text` so tests don't panic on overflow —
//! observable via `call_count()`.

#![cfg(any(test, feature = "test-fixtures"))]

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use crate::engine::context::Context;
use crate::engine::llm::error::LlmError;
use crate::engine::llm::request::GenerateRequest;
use crate::engine::llm::response::{FinishReason, Generation};
use crate::engine::llm::{LlmClient, sealed::Sealed};

/// Phase D audit A-M2 fix: aligned with `MockSentimentClassifier`
/// precedent — `VecDeque` for O(1) FIFO drain (was `Vec::remove(0)`
/// at O(n)) and `AtomicUsize` for the counter (was `Mutex<usize>` —
/// no reason to lock for a single increment).
#[derive(Debug, Default)]
pub struct MockLlmClient {
    responses: Mutex<VecDeque<Result<Generation, LlmError>>>,
    call_count: AtomicUsize,
}

impl MockLlmClient {
    /// Queue a success response. Calls drain in FIFO order.
    pub fn with_response(self, generation: Generation) -> Self {
        self.responses
            .lock()
            .expect("MockLlmClient mutex poisoned")
            .push_back(Ok(generation));
        self
    }

    /// Queue an error response.
    pub fn with_error(self, err: LlmError) -> Self {
        self.responses
            .lock()
            .expect("MockLlmClient mutex poisoned")
            .push_back(Err(err));
        self
    }

    /// How many times has `generate` been called? Observable AFTER the
    /// queue is drained — assertions can verify exact call count even
    /// when the test stages 0 responses (silent-fallback mode).
    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::Relaxed)
    }
}

impl Sealed for MockLlmClient {}

#[async_trait]
impl LlmClient for MockLlmClient {
    async fn generate(
        &self,
        _ctx: &Context,
        _request: &GenerateRequest,
    ) -> Result<Generation, LlmError> {
        self.call_count.fetch_add(1, Ordering::Relaxed);
        let mut queue = self.responses.lock().expect("MockLlmClient mutex poisoned");
        if let Some(staged) = queue.pop_front() {
            return staged;
        }
        // Silent fallback — empty queue produces a debug stub so tests
        // staging zero responses don't panic. Mirrors the
        // `MockSentimentClassifier` policy.
        Ok(Generation {
            text: "[MockLlmClient empty-queue stub]".to_string(),
            parsed: None,
            finish_reason: FinishReason::Stop,
            usage: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context::single_user_local()
    }

    fn req() -> GenerateRequest {
        GenerateRequest::default()
    }

    #[tokio::test]
    async fn empty_queue_returns_stub_and_increments_call_count() {
        let mock = MockLlmClient::default();
        let g = mock.generate(&ctx(), &req()).await.unwrap();
        assert!(g.text.contains("empty-queue stub"));
        assert_eq!(mock.call_count(), 1);
    }

    #[tokio::test]
    async fn with_response_drains_in_fifo_order() {
        let g1 = Generation {
            text: "first".into(),
            parsed: None,
            finish_reason: FinishReason::Stop,
            usage: None,
        };
        let g2 = Generation {
            text: "second".into(),
            parsed: None,
            finish_reason: FinishReason::Stop,
            usage: None,
        };
        let mock = MockLlmClient::default()
            .with_response(g1.clone())
            .with_response(g2.clone());
        let r1 = mock.generate(&ctx(), &req()).await.unwrap();
        let r2 = mock.generate(&ctx(), &req()).await.unwrap();
        assert_eq!(r1.text, "first");
        assert_eq!(r2.text, "second");
        assert_eq!(mock.call_count(), 2);
    }

    #[tokio::test]
    async fn with_error_surfaces_at_generate() {
        let mock = MockLlmClient::default().with_error(LlmError::RateLimited);
        let r = mock.generate(&ctx(), &req()).await;
        assert!(matches!(r, Err(LlmError::RateLimited)));
    }

    #[tokio::test]
    async fn mixed_responses_and_errors_drain_in_order() {
        let g = Generation {
            text: "ok".into(),
            parsed: None,
            finish_reason: FinishReason::Stop,
            usage: None,
        };
        let mock = MockLlmClient::default()
            .with_response(g.clone())
            .with_error(LlmError::Timeout)
            .with_response(g.clone());
        assert_eq!(mock.generate(&ctx(), &req()).await.unwrap().text, "ok");
        assert!(matches!(
            mock.generate(&ctx(), &req()).await,
            Err(LlmError::Timeout)
        ));
        assert_eq!(mock.generate(&ctx(), &req()).await.unwrap().text, "ok");
    }
}
