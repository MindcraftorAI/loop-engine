//! Storage error type.
//!
//! Fixed enum (not an associated `type Error;`) so callers don't have
//! to be generic over `S::Error`. Matches `object_store::Error`,
//! `opendal::Error`, `sqlx::Error` design.

use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StorageError {
    #[error("storage key not found: {key}")]
    NotFound { key: String },

    #[error("storage key already exists: {key}")]
    AlreadyExists { key: String },

    #[error("storage permission denied: {key}")]
    PermissionDenied { key: String },

    #[error("storage version mismatch on {key}")]
    VersionMismatch { key: String },

    #[error("storage backend: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl StorageError {
    pub fn backend<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Backend(Box::new(err))
    }
}
