//! Errors from [`super::Embedder::embed`] calls.

use thiserror::Error;

use crate::engine::error::EngineError;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EmbeddingError {
    /// Network / transport-level failure.
    #[error("transport error: {0}")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Provider rate-limited the request.
    #[error("rate limited")]
    RateLimited,

    /// Provider returned a response but it didn't match the expected
    /// shape (vector length != `dimensions()`, missing field, etc).
    #[error("invalid output: {0}")]
    InvalidOutput(String),

    /// Adapter cannot embed the requested input (e.g. text exceeds
    /// provider token limit).
    #[error("unsupported input: {0}")]
    UnsupportedInput(String),

    /// Per-call timeout.
    #[error("timeout")]
    Timeout,
}

impl EmbeddingError {
    pub fn transport<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Transport(Box::new(err))
    }
}

impl From<EmbeddingError> for EngineError {
    fn from(err: EmbeddingError) -> Self {
        EngineError::Embedding(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_constructor_wraps_error() {
        let inner = std::io::Error::new(std::io::ErrorKind::TimedOut, "slow");
        let err = EmbeddingError::transport(inner);
        assert!(matches!(err, EmbeddingError::Transport(_)));
    }

    #[test]
    fn invalid_output_display_carries_payload() {
        let err = EmbeddingError::InvalidOutput("vector len 384 != dim 768".into());
        assert!(format!("{err}").contains("384"));
    }

    #[test]
    fn from_into_engine_error() {
        let err: EngineError = EmbeddingError::RateLimited.into();
        assert!(matches!(
            err,
            EngineError::Embedding(EmbeddingError::RateLimited)
        ));
    }
}
