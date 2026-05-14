//! `MockEmbedder` — test fixture behind `test-fixtures`.
//!
//! Phase D D-D8: explicit `new(dimensions)` constructor — NO `Default`
//! impl, because dimension is load-bearing (callers wiring vector
//! search MUST commit to a dimension). Builder-chain matches
//! [`super::super::llm::MockLlmClient`] FIFO semantics. Empty queue
//! falls back to an all-zeros vector of the configured dimension.
//!
//! `with_deterministic` (text-hash → cyclic-expand → L2-normalize)
//! deferred to Phase E per OQ-D6 — the memory store is the consumer
//! that needs reproducible vectors for similarity tests.

#![cfg(any(test, feature = "test-fixtures"))]

use std::sync::Mutex;

use async_trait::async_trait;

use crate::engine::context::Context;
use crate::engine::embedding::error::EmbeddingError;
use crate::engine::embedding::{sealed::Sealed, Embedder};

#[derive(Debug)]
pub struct MockEmbedder {
    dimensions: usize,
    responses: Mutex<Vec<Result<Vec<Vec<f32>>, EmbeddingError>>>,
    call_count: Mutex<usize>,
}

impl MockEmbedder {
    /// Construct with an explicit dimension. No `Default` — dimension
    /// must be a deliberate choice.
    pub fn new(dimensions: usize) -> Self {
        Self {
            dimensions,
            responses: Mutex::new(Vec::new()),
            call_count: Mutex::new(0),
        }
    }

    /// Queue a success response (FIFO). Caller is responsible for
    /// vector length matching `dimensions`; mismatch would surface as
    /// the adapter's invariant violation, not the mock's.
    pub fn with_response(self, vectors: Vec<Vec<f32>>) -> Self {
        self.responses
            .lock()
            .expect("MockEmbedder mutex poisoned")
            .push(Ok(vectors));
        self
    }

    /// Queue an error response (FIFO).
    pub fn with_error(self, err: EmbeddingError) -> Self {
        self.responses
            .lock()
            .expect("MockEmbedder mutex poisoned")
            .push(Err(err));
        self
    }

    /// How many times has `embed` been called?
    pub fn call_count(&self) -> usize {
        *self
            .call_count
            .lock()
            .expect("MockEmbedder mutex poisoned")
    }
}

impl Sealed for MockEmbedder {}

#[async_trait]
impl Embedder for MockEmbedder {
    async fn embed(
        &self,
        _ctx: &Context,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        {
            let mut n = self
                .call_count
                .lock()
                .expect("MockEmbedder mutex poisoned");
            *n += 1;
        }
        let mut queue = self
            .responses
            .lock()
            .expect("MockEmbedder mutex poisoned");
        if queue.is_empty() {
            // Empty-queue fallback: one all-zeros vector per input
            // text, at the configured dimension. Lets tests verify
            // "embed got called" without staging responses.
            return Ok((0..texts.len())
                .map(|_| vec![0.0_f32; self.dimensions])
                .collect());
        }
        queue.remove(0)
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context::single_user_local()
    }

    #[tokio::test]
    async fn empty_queue_returns_zero_vector_per_input() {
        let m = MockEmbedder::new(4);
        let texts = vec!["hello".to_string(), "world".to_string()];
        let v = m.embed(&ctx(), &texts).await.unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], vec![0.0_f32; 4]);
        assert_eq!(v[1], vec![0.0_f32; 4]);
        assert_eq!(m.call_count(), 1);
    }

    #[tokio::test]
    async fn with_response_drains_in_fifo_order() {
        let m = MockEmbedder::new(3)
            .with_response(vec![vec![1.0, 0.0, 0.0]])
            .with_response(vec![vec![0.0, 1.0, 0.0]]);
        let r1 = m.embed(&ctx(), &["a".into()]).await.unwrap();
        let r2 = m.embed(&ctx(), &["b".into()]).await.unwrap();
        assert_eq!(r1[0][0], 1.0);
        assert_eq!(r2[0][1], 1.0);
    }

    #[tokio::test]
    async fn with_error_surfaces_at_embed() {
        let m = MockEmbedder::new(4).with_error(EmbeddingError::RateLimited);
        let r = m.embed(&ctx(), &["x".into()]).await;
        assert!(matches!(r, Err(EmbeddingError::RateLimited)));
    }

    #[test]
    fn dimensions_returns_configured_value() {
        let m = MockEmbedder::new(384);
        assert_eq!(m.dimensions(), 384);
    }
}
