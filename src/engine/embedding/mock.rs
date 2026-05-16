//! `MockEmbedder` — test fixture behind `test-fixtures`.
//!
//! Phase D D-D8: explicit `new(dimensions)` constructor — NO `Default`
//! impl, because dimension is load-bearing (callers wiring vector
//! search MUST commit to a dimension). Builder-chain matches
//! [`super::super::llm::MockLlmClient`] FIFO semantics. Empty queue
//! falls back to an all-zeros vector of the configured dimension.
//!
//! `with_deterministic` (text-hash → cyclic-expand → L2-normalize)
//! shipped Phase E D-E12: the memory store consumer needs reproducible
//! vectors for similarity-search tests, and a queue-of-canned-responses
//! pattern doesn't scale to "embed N texts, query with one, expect
//! N back in similarity order."

#![cfg(any(test, feature = "test-fixtures"))]

use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;

use crate::engine::context::Context;
use crate::engine::embedding::error::EmbeddingError;
use crate::engine::embedding::{Embedder, sealed::Sealed};

/// Phase D audit A-M2 fix: VecDeque + AtomicUsize matching the
/// `MockSentimentClassifier` precedent.
#[derive(Debug)]
pub struct MockEmbedder {
    dimensions: usize,
    responses: Mutex<VecDeque<Result<Vec<Vec<f32>>, EmbeddingError>>>,
    call_count: AtomicUsize,
    /// Phase E D-E12: when true, ignore the queued responses and
    /// produce a deterministic embedding via `text_to_vector` for
    /// every call. Same text always produces the same vector.
    deterministic: AtomicBool,
}

impl MockEmbedder {
    /// Construct with an explicit dimension. No `Default` — dimension
    /// must be a deliberate choice.
    pub fn new(dimensions: usize) -> Self {
        Self {
            dimensions,
            responses: Mutex::new(VecDeque::new()),
            call_count: AtomicUsize::new(0),
            deterministic: AtomicBool::new(false),
        }
    }

    /// Phase E D-E12: switch to deterministic mode. Subsequent
    /// `embed()` calls produce vectors derived from `DefaultHasher`
    /// over each input text → cyclic byte-fill → L2-normalize. Same
    /// text always produces the same vector across runs of the same
    /// binary. NOT collision-free across distinct texts, but
    /// acceptable for similarity-search tests.
    ///
    /// Note: `DefaultHasher` is not guaranteed stable across Rust
    /// stdlib versions (D-E12 stability caveat). If a test pins a
    /// specific vector by hash value, it may need to update on Rust
    /// upgrades. Tests SHOULD assert relative properties (cosine
    /// similarity ordering, dimension correctness) rather than
    /// absolute bit-equality.
    #[must_use]
    pub fn with_deterministic(self) -> Self {
        self.deterministic.store(true, Ordering::Relaxed);
        self
    }

    /// Convert a text to a deterministic, L2-normalized vector of
    /// `dimensions` floats. See [`Self::with_deterministic`].
    fn text_to_vector(&self, text: &str) -> Vec<f32> {
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        let seed = hasher.finish();
        // Cyclic byte-fill: split the u64 into 8 i8s, expand to
        // `dimensions` slots, normalize each byte to f32 in
        // approximately [-1.0, 1.0] via /128.0.
        let bytes = seed.to_le_bytes();
        let mut vec: Vec<f32> = (0..self.dimensions)
            .map(|i| {
                let b = bytes[i % 8] as i8;
                (b as f32) / 128.0
            })
            .collect();
        // L2-normalize. If the input happens to produce an all-zero
        // vector (vanishingly unlikely with a real hash), return
        // unit-axis-0 as a safe fallback so cosine similarity is
        // well-defined.
        let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 1e-9 {
            for v in vec.iter_mut() {
                *v /= norm;
            }
        } else if !vec.is_empty() {
            vec[0] = 1.0;
        }
        vec
    }

    /// Queue a success response (FIFO). Caller is responsible for
    /// vector length matching `dimensions`; mismatch would surface as
    /// the adapter's invariant violation, not the mock's.
    pub fn with_response(self, vectors: Vec<Vec<f32>>) -> Self {
        self.responses
            .lock()
            .expect("MockEmbedder mutex poisoned")
            .push_back(Ok(vectors));
        self
    }

    /// Queue an error response (FIFO).
    pub fn with_error(self, err: EmbeddingError) -> Self {
        self.responses
            .lock()
            .expect("MockEmbedder mutex poisoned")
            .push_back(Err(err));
        self
    }

    /// How many times has `embed` been called?
    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::Relaxed)
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
        self.call_count.fetch_add(1, Ordering::Relaxed);
        // Phase E D-E12: deterministic mode bypasses the queue
        // entirely — every text gets a hash-derived vector. Lets
        // similarity-search tests insert N memories + query with
        // related text + expect N vectors back in similarity order.
        if self.deterministic.load(Ordering::Relaxed) {
            return Ok(texts.iter().map(|t| self.text_to_vector(t)).collect());
        }
        let mut queue = self.responses.lock().expect("MockEmbedder mutex poisoned");
        if let Some(staged) = queue.pop_front() {
            return staged;
        }
        // Empty-queue fallback: one all-zeros vector per input text,
        // at the configured dimension. Lets tests verify "embed got
        // called" without staging responses.
        Ok((0..texts.len())
            .map(|_| vec![0.0_f32; self.dimensions])
            .collect())
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

    // Phase E D-E12 — `with_deterministic` tests

    #[tokio::test]
    async fn deterministic_same_text_produces_same_vector() {
        let m = MockEmbedder::new(8).with_deterministic();
        let r1 = m.embed(&ctx(), &["hello world".into()]).await.unwrap();
        let r2 = m.embed(&ctx(), &["hello world".into()]).await.unwrap();
        assert_eq!(r1, r2, "deterministic mode must be reproducible");
    }

    #[tokio::test]
    async fn deterministic_different_texts_produce_different_vectors() {
        let m = MockEmbedder::new(8).with_deterministic();
        let r1 = m.embed(&ctx(), &["alpha".into()]).await.unwrap();
        let r2 = m.embed(&ctx(), &["beta".into()]).await.unwrap();
        assert_ne!(
            r1, r2,
            "distinct texts should not collide (most of the time)"
        );
    }

    #[tokio::test]
    async fn deterministic_returns_l2_normalized_vectors() {
        let m = MockEmbedder::new(16).with_deterministic();
        let r = m.embed(&ctx(), &["test".into()]).await.unwrap();
        let v = &r[0];
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "vector should be L2-normalized, got norm = {norm}"
        );
    }

    #[tokio::test]
    async fn deterministic_bypasses_response_queue() {
        // Stage a response that would obviously fail an L2-norm
        // check. Verify deterministic mode IGNORES it.
        let m = MockEmbedder::new(4)
            .with_response(vec![vec![999.0, 999.0, 999.0, 999.0]])
            .with_deterministic();
        let r = m.embed(&ctx(), &["x".into()]).await.unwrap();
        let v = &r[0];
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "deterministic should ignore queue"
        );
    }

    #[tokio::test]
    async fn deterministic_returns_correct_dimension() {
        let m = MockEmbedder::new(384).with_deterministic();
        let r = m.embed(&ctx(), &["x".into()]).await.unwrap();
        assert_eq!(r[0].len(), 384);
    }
}
