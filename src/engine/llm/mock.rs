//! `MockLlmClient` — test fixture / dev shim behind `test-fixtures`.
//!
//! Phase D D-D8: builder-chain matching `MockSentimentClassifier`.
//! `default()` is constructable; `with_response(Generation)` and
//! `with_error(LlmError)` are FIFO-queued. Empty queue falls back to a
//! debug-stub `Generation::Text` so tests don't panic on overflow —
//! observable via `call_count()`.

#![cfg(any(test, feature = "test-fixtures"))]

use std::sync::Mutex;

use async_trait::async_trait;

use crate::engine::context::Context;
use crate::engine::llm::error::LlmError;
use crate::engine::llm::request::GenerateRequest;
use crate::engine::llm::response::{FinishReason, Generation};
use crate::engine::llm::{sealed::Sealed, LlmClient};

#[derive(Debug, Default)]
pub struct MockLlmClient {
    /// FIFO of pre-staged responses. Drained one-per-call.
    responses: Mutex<Vec<Result<Generation, LlmError>>>,
    /// Observable call counter — survives across response-queue drain.
    call_count: Mutex<usize>,
}

impl MockLlmClient {
    /// Queue a success response. Calls drain in FIFO order.
    pub fn with_response(self, generation: Generation) -> Self {
        self.responses
            .lock()
            .expect("MockLlmClient mutex poisoned")
            .push(Ok(generation));
        self
    }

    /// Queue an error response.
    pub fn with_error(self, err: LlmError) -> Self {
        self.responses
            .lock()
            .expect("MockLlmClient mutex poisoned")
            .push(Err(err));
        self
    }

    /// How many times has `generate` been called? Observable AFTER the
    /// queue is drained — assertions can verify exact call count even
    /// when the test stages 0 responses (silent-fallback mode).
    pub fn call_count(&self) -> usize {
        *self
            .call_count
            .lock()
            .expect("MockLlmClient mutex poisoned")
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
        {
            let mut n = self
                .call_count
                .lock()
                .expect("MockLlmClient mutex poisoned");
            *n += 1;
        }
        let mut queue = self
            .responses
            .lock()
            .expect("MockLlmClient mutex poisoned");
        if queue.is_empty() {
            // Silent fallback — empty queue produces a debug stub so
            // tests staging zero responses don't panic. Mirrors the
            // `MockSentimentClassifier` policy.
            return Ok(Generation {
                text: "[MockLlmClient empty-queue stub]".to_string(),
                parsed: None,
                finish_reason: FinishReason::Stop,
                usage: None,
            });
        }
        // FIFO — remove(0) keeps insertion order matching what tests
        // staged. Cost is O(n) but queue length is small in tests.
        queue.remove(0)
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
        assert_eq!(
            mock.generate(&ctx(), &req()).await.unwrap().text,
            "ok"
        );
        assert!(matches!(
            mock.generate(&ctx(), &req()).await,
            Err(LlmError::Timeout)
        ));
        assert_eq!(
            mock.generate(&ctx(), &req()).await.unwrap().text,
            "ok"
        );
    }
}
