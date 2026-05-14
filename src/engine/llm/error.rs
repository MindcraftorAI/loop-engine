//! Errors from `LlmClient::generate` calls.
//!
//! Phase D D-D4: typed `LlmError` for trait surface; `From<LlmError> for
//! EngineError` lets engine-level functions (e.g. `narrative::generate`)
//! propagate via the `EngineError` family.

use thiserror::Error;

use crate::engine::error::EngineError;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LlmError {
    /// Network / transport-level failure. The boxed inner error preserves
    /// the original cause for tracing without forcing the trait to know
    /// the adapter's HTTP client type.
    #[error("transport error: {0}")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Provider rate-limited the request. Adapters that handle Retry-After
    /// internally MAY swallow + retry instead of surfacing this; engine
    /// callers (e.g. `narrative::generate`) treat it as terminal.
    #[error("rate limited")]
    RateLimited,

    /// Provider returned a response but it didn't match the requested
    /// shape (parse failure on `Generation::parsed`, missing `content`,
    /// etc). Adapters that return `Generation::parsed = None` for a
    /// `JsonSchema` request also surface here at the engine boundary.
    #[error("invalid output: {0}")]
    InvalidOutput(String),

    /// Output parsed but failed engine-side semantic validation
    /// (e.g. `narrative::generate` rejecting `confidence: observed`
    /// with empty `evidence_refs` per Phase D D-D10 defense-in-depth).
    #[error("validation failed: {0}")]
    ValidationFailed(String),

    /// Adapter cannot honor a requested feature (e.g. `JsonSchema`
    /// response format on a provider with no structured-output API).
    /// Adapters MAY fall back rather than surfacing this — at their
    /// discretion.
    #[error("unsupported feature: {0}")]
    UnsupportedFeature(String),

    /// Per-call timeout. Adapters that retry-with-backoff handle
    /// internally; if surfaced, treat as terminal.
    #[error("timeout")]
    Timeout,
}

impl LlmError {
    /// Convenience constructor that boxes any `Send + Sync` error into
    /// the `Transport` variant.
    pub fn transport<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Transport(Box::new(err))
    }
}

impl From<LlmError> for EngineError {
    fn from(err: LlmError) -> Self {
        EngineError::Llm(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_constructor_wraps_arbitrary_send_sync_error() {
        let inner = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let err = LlmError::transport(inner);
        assert!(matches!(err, LlmError::Transport(_)));
        assert!(format!("{err}").contains("transport"));
    }

    #[test]
    fn rate_limited_display() {
        assert_eq!(format!("{}", LlmError::RateLimited), "rate limited");
    }

    #[test]
    fn invalid_output_carries_payload() {
        let err = LlmError::InvalidOutput("missing field 'trigger'".into());
        assert!(format!("{err}").contains("missing field"));
    }

    #[test]
    fn from_into_engine_error() {
        let err: EngineError = LlmError::Timeout.into();
        assert!(matches!(err, EngineError::Llm(LlmError::Timeout)));
    }
}
