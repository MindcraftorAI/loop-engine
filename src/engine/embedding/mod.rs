//! Embedding (vector) abstraction.
//!
//! Phase D D-D7: sibling of [`crate::engine::llm::LlmClient`] — NOT a
//! supertrait. Embedding providers (Voyage, Anthropic embedder,
//! OpenAI ada/text-embedding-3, local `fastembed-rs` or `candle-rs`)
//! are commonly the same vendor as LLM providers but the abstraction
//! is independent: one trait per concern.
//!
//! Engine ships only the trait + types + `MockEmbedder` (behind the
//! `test-fixtures` feature). Provider
//! impls live in the future monolith repo per the engine/monolith
//! split.
//!
//! Phase E (memory store) is the first consumer. Phase D's job is
//! purely to land the surface so Phase E can wire it up.

use std::fmt::Debug;

use async_trait::async_trait;

pub mod error;
pub mod mock;
pub mod openai_compatible;

pub use error::EmbeddingError;
pub use openai_compatible::OpenAiCompatibleEmbedder;

#[cfg(any(test, feature = "test-fixtures"))]
pub use mock::MockEmbedder;

use crate::engine::context::Context;

/// Text-to-vector embedding abstraction.
///
/// Batch API: takes `&[String]`, returns `Vec<Vec<f32>>` — one vector
/// per input text. Adapters that don't support batching internally
/// sequentialize; the engine API stays batch-shaped for cost
/// efficiency (memory store rebuilds can embed hundreds of texts in
/// one HTTP call when the provider supports it).
///
/// Sealed. Cross-crate monolith impls land via the workspace pattern.
#[async_trait]
pub trait Embedder: Send + Sync + Debug + sealed::Sealed {
    /// Embed a batch of texts. The returned `Vec<Vec<f32>>` has the
    /// same length as `texts`; each inner `Vec<f32>` has length
    /// `self.dimensions()`. Adapters that violate either invariant
    /// surface [`EmbeddingError::InvalidOutput`].
    async fn embed(&self, ctx: &Context, texts: &[String])
        -> Result<Vec<Vec<f32>>, EmbeddingError>;

    /// Vector dimensionality. Sync — adapters that need runtime
    /// discovery cache eagerly on first construction (provider HTTP
    /// call inside `new(...)` if needed). OQ-D5 defers async-
    /// dimensions to Phase E if a real adapter demands it.
    fn dimensions(&self) -> usize;
}

pub(crate) mod sealed {
    /// Private marker — external crates cannot satisfy this, so they
    /// cannot implement [`super::Embedder`] directly. Monolith
    /// adapters land via the workspace pattern.
    ///
    /// Stabilized per Phase H D-H2: sealed for v1.0. See
    /// `INTEGRATING.md` for the workspace-pattern integration
    /// model.
    pub trait Sealed {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Compile-time test: `Embedder` is object-safe.
    #[allow(dead_code)]
    fn object_safety_check(_: Arc<dyn Embedder>) {}

    /// Compile-time test: `MockEmbedder` satisfies the trait + holds
    /// as `Arc<dyn Embedder>`.
    #[allow(dead_code)]
    fn mock_satisfies_trait() {
        let mock = MockEmbedder::new(8);
        let _arc: Arc<dyn Embedder> = Arc::new(mock);
    }
}
