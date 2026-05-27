//! LLM client abstraction.
//!
//! Phase D D-D1..D-D6: single-method `LlmClient` trait that engine
//! modules call to invoke an LLM. Engine-private; provider impls
//! (Anthropic API, Claude Agent SDK, OpenAI, local Ollama, etc) live
//! in the future monolith repo per the engine/monolith split. The
//! engine ships only the trait + types + `MockLlmClient` (behind the
//! `test-fixtures` feature).
//!
//! Pattern is lifted from [`crate::engine::storage::Storage`] +
//! [`crate::engine::sentiment::SentimentClassifier`]:
//!   - `#[async_trait]` for object-safe async methods.
//!   - `Send + Sync + Debug + Sealed` bounds for `Arc<dyn _>` use.
//!   - Sealed via private `sealed::Sealed` so external crates cannot
//!     impl directly; cross-crate monolith impls land via the
//!     workspace pattern (OQ-D3 resolution — see learn-notes).
//!
//! **No streaming in Phase D.** Streaming is an additive future method
//! (`generate_stream` returning `Stream<Item = ...>`) when a consumer
//! needs it; the engine's narrative/skill-eval use cases are one-shot.
//!
//! **No retries / backoff / cost tracking in the engine.** Adapter
//! impls own those concerns per D-D11 + D-D12.

use std::fmt::Debug;

use async_trait::async_trait;

pub mod error;
pub mod mock;
pub mod openai_compatible;
pub mod request;
pub mod response;

pub use error::LlmError;
pub use openai_compatible::OpenAiCompatibleLlm;
pub use request::{GenerateRequest, ResponseFormat};
pub use response::{FinishReason, Generation, TokenUsage};

#[cfg(any(test, feature = "test-fixtures"))]
pub use mock::MockLlmClient;

use crate::engine::context::Context;

/// LLM client abstraction.
///
/// One method: `generate`. All variation is in [`GenerateRequest`] +
/// [`ResponseFormat`]. The trait is sealed; engine-shipped impls
/// today are only `MockLlmClient` (test fixture, behind the
/// `test-fixtures` Cargo feature). Monolith adapters
/// (Anthropic, Claude Agent SDK, ...) ship in the future monolith
/// crate and satisfy the sealed marker via the workspace pattern.
#[async_trait]
pub trait LlmClient: Send + Sync + Debug + sealed::Sealed {
    /// Invoke the LLM and return a single `Generation`. Pure async,
    /// object-safe — held as `Arc<dyn LlmClient>` in engine modules.
    ///
    /// Errors are typed via [`LlmError`]. Engine consumers convert to
    /// `EngineError::Llm(_)` via the provided `From` impl.
    ///
    /// Engine treats every call as opaque — no retries, no backoff,
    /// no timeout enforcement at this layer. Adapters that handle
    /// `RateLimited`/`Timeout` internally MAY swallow + retry before
    /// returning; if they surface the error, engine callers treat
    /// as terminal (per D-D12).
    async fn generate(
        &self,
        ctx: &Context,
        request: &GenerateRequest,
    ) -> Result<Generation, LlmError>;
}

pub(crate) mod sealed {
    /// Private marker — external crates cannot satisfy this, so they
    /// cannot implement [`super::LlmClient`] directly. Monolith
    /// adapters land via the workspace pattern (see Phase D
    /// learn-notes OQ-D3 resolution).
    ///
    /// Stabilized per Phase H D-H2 (`phase-h-learn-notes.md`): sealed
    /// for v1.0; revisit in v1.1 if external implementors emerge.
    /// Workspace pattern locked — external LlmClient impls land via
    /// adapter modules in the engine crate, not downstream crates.
    /// See `INTEGRATING.md`.
    pub trait Sealed {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Compile-time test: the trait is object-safe. If this stops
    /// compiling, Phase D's `dyn` guarantee is broken (probably a
    /// generic method or `Self` in return position was added).
    #[allow(dead_code)]
    fn object_safety_check(_: Arc<dyn LlmClient>) {}

    /// Compile-time test: `MockLlmClient` satisfies the trait + can
    /// be held as `Arc<dyn LlmClient>`.
    #[allow(dead_code)]
    fn mock_satisfies_trait() {
        let mock = MockLlmClient::default();
        let _arc: Arc<dyn LlmClient> = Arc::new(mock);
    }
}
