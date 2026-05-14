//! Errors from [`super::VectorIndex`] calls.

use thiserror::Error;

use crate::engine::error::EngineError;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum VectorIndexError {
    /// Backend transport failure (network call for remote indexes,
    /// disk I/O for local mmap-backed indexes).
    #[error("vector index transport: {0}")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Provided vector failed structural validation (e.g. wrong
    /// length vs `dimensions()`, or contained NaN/Inf).
    #[error("invalid vector: {0}")]
    InvalidVector(String),

    /// Provided vector length doesn't match the index's
    /// `dimensions()`. Common adapter-misconfiguration failure.
    #[error("dimension mismatch: provided={provided} expected={expected}")]
    DimensionMismatch { provided: usize, expected: usize },

    /// Caller asked for an operation the impl doesn't support
    /// (e.g. native delete on a backend that only tombstones).
    #[error("unsupported operation: {0}")]
    Unsupported(String),

    /// Backend internal error — bucket for impl-specific failures
    /// that don't fit other variants.
    #[error("vector index internal: {0}")]
    Internal(String),
}

impl VectorIndexError {
    pub fn transport<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Transport(Box::new(err))
    }
}

impl From<VectorIndexError> for EngineError {
    fn from(err: VectorIndexError) -> Self {
        EngineError::VectorIndex(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimension_mismatch_display_includes_both_values() {
        let e = VectorIndexError::DimensionMismatch {
            provided: 384,
            expected: 768,
        };
        let s = format!("{e}");
        assert!(s.contains("384") && s.contains("768"));
    }

    #[test]
    fn transport_constructor_wraps_error() {
        let inner = std::io::Error::new(std::io::ErrorKind::TimedOut, "slow");
        let err = VectorIndexError::transport(inner);
        assert!(matches!(err, VectorIndexError::Transport(_)));
    }

    #[test]
    fn from_into_engine_error() {
        let err: EngineError =
            VectorIndexError::InvalidVector("contains NaN".into()).into();
        assert!(matches!(err, EngineError::VectorIndex(_)));
    }
}
