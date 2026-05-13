//! Crate-level engine error type.
//!
//! Per Day 16b D5: typed `EngineError` replaces `anyhow::Error` in
//! engine public function returns. Legacy modules (Day 11/12 lessons
//! sync API, lifecycle, pid, buffer) keep `anyhow` for now — the
//! migration is per-module and incremental. New async APIs introduced
//! Day 16b+ use `EngineError`.
//!
//! `#[non_exhaustive]` — variants will grow. No `Clone` impl per
//! OQ-D16b-E (`io::Error` and `StorageError` are not `Clone`).

use std::io;

use thiserror::Error;

use crate::engine::storage::StorageError;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EngineError {
    /// A lesson lookup failed because no lesson with that id exists in
    /// any status directory.
    #[error("lesson not found: {id}")]
    LessonNotFound { id: String },

    /// Storage backend returned an error.
    #[error("storage error: {0}")]
    Storage(#[source] StorageError),

    /// YAML parse or serialization error. Boxed because the YAML stack
    /// has multiple error types in play (`serde_yml`, our purpose-built
    /// `engine::yaml::reader`). OQ-D16b-A: no typed YAML variants.
    #[error("yaml error: {0}")]
    Yaml(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Generic parse / shape error — body content didn't match the
    /// expected structure.
    #[error("parse error: {0}")]
    Parse(String),

    /// Compare-and-swap retry budget exhausted. `retries` is the count
    /// of failed attempts before giving up.
    #[error("CAS contended on {key} after {retries} retries")]
    CasContended { key: String, retries: u32 },

    /// Underlying I/O error.
    #[error("io error: {0}")]
    Io(#[source] io::Error),

    /// Genuinely uncategorized engine-level error. Use sparingly — adding
    /// a named variant is preferred when the error class repeats.
    #[error("engine error: {0}")]
    Other(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl EngineError {
    pub fn yaml<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Yaml(Box::new(err))
    }

    pub fn other<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Other(Box::new(err))
    }
}

impl From<StorageError> for EngineError {
    fn from(err: StorageError) -> Self {
        Self::Storage(err)
    }
}

impl From<io::Error> for EngineError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_includes_kind_and_payload() {
        let err = EngineError::LessonNotFound {
            id: "les-abc".into(),
        };
        let s = format!("{err}");
        assert!(s.contains("lesson not found"));
        assert!(s.contains("les-abc"));
    }

    #[test]
    fn storage_error_converts_via_from() {
        let storage_err = StorageError::NotFound {
            key: "lessons/active/x.md".into(),
        };
        let engine_err: EngineError = storage_err.into();
        assert!(matches!(engine_err, EngineError::Storage(_)));
    }

    #[test]
    fn io_error_converts_via_from() {
        let io_err = io::Error::new(io::ErrorKind::PermissionDenied, "boom");
        let engine_err: EngineError = io_err.into();
        assert!(matches!(engine_err, EngineError::Io(_)));
    }

    #[test]
    fn cas_contended_carries_key_and_retries() {
        let err = EngineError::CasContended {
            key: "lessons/active/x.md".into(),
            retries: 5,
        };
        let s = format!("{err}");
        assert!(s.contains("CAS contended"));
        assert!(s.contains("5"));
    }

    #[test]
    fn yaml_constructor_boxes_arbitrary_error() {
        let inner = io::Error::other("yaml-ish");
        let err = EngineError::yaml(inner);
        assert!(matches!(err, EngineError::Yaml(_)));
    }
}
