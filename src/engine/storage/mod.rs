//! Engine storage abstraction.
//!
//! Object-safe `dyn Storage` trait + a fixed `StorageError` enum + a
//! typed `StorageKey` newtype. Pattern lifted from `object_store` /
//! `opendal`: backends are plain `Arc<dyn Storage>`, methods are
//! `async fn` via `async_trait` macro (preserves `Send` bounds), key
//! type encodes multi-tenant routing in one place per resource.
//!
//! **Sealed.** External crates cannot implement `Storage`; only impls
//! shipped inside this engine. Hosts wire one of the engine-provided
//! backends (`LocalFsStorage`, `MemoryStorage`) into the daemon.

use async_trait::async_trait;
use bytes::Bytes;
use std::fmt::Debug;

pub mod error;
pub mod filesystem;
pub mod key;
pub(crate) mod lock;
pub mod memory;
pub mod metadata;
pub mod version;

pub use error::StorageError;
pub use filesystem::LocalFsStorage;
pub use key::StorageKey;
pub use memory::MemoryStorage;
pub use metadata::StorageMetadata;
pub use version::Version;

/// Engine storage abstraction.
///
/// Backends implement key-addressed byte-blob I/O. Multi-tenant
/// path routing happens inside [`StorageKey`] constructors, NOT here
/// — `Storage` is identity-agnostic and sees only opaque keys.
///
/// `Send + Sync + Debug`: held in `Arc<dyn Storage>` and used across
/// tokio multi-thread runtime tasks.
///
/// Sealed via [`sealed::Sealed`] — only engine-shipped backends can
/// satisfy this trait. See module docs.
#[async_trait]
pub trait Storage: Send + Sync + Debug + sealed::Sealed {
    /// Read a key's contents. `Ok(None)` for absent (not an error).
    async fn get(&self, key: &StorageKey) -> Result<Option<Bytes>, StorageError>;

    /// Write a key, overwriting if it exists. Implementations MUST be
    /// crash-atomic — partial writes never observable (write to temp,
    /// then atomic rename on local filesystems; multipart upload then
    /// commit on S3).
    async fn put(&self, key: &StorageKey, bytes: Bytes) -> Result<(), StorageError>;

    /// Delete a key. Idempotent — deleting an absent key is `Ok(())`.
    async fn delete(&self, key: &StorageKey) -> Result<(), StorageError>;

    /// List all keys under `prefix`. Returns only keys, not bytes.
    /// Order is implementation-defined; callers requiring deterministic
    /// order must sort.
    async fn list(&self, prefix: &StorageKey) -> Result<Vec<StorageKey>, StorageError>;

    /// Compare-and-set write for cross-process safe RMW.
    ///
    /// - Returns `Ok(true)` on success, `Ok(false)` if the precondition
    ///   failed (current version differs from `expected_version`).
    /// - `expected_version = None` means "must not exist" (create-only).
    ///
    /// Local fs implements via sidecar `fd-lock` + atomic rename; S3
    /// would use `If-Match` etag; in-memory uses a `Mutex`.
    async fn put_if_version(
        &self,
        key: &StorageKey,
        bytes: Bytes,
        expected_version: Option<&Version>,
    ) -> Result<bool, StorageError>;

    /// Read a key and its version atomically (the version that callers
    /// will pass back to [`put_if_version`]). `Ok(None)` for absent.
    async fn get_with_version(
        &self,
        key: &StorageKey,
    ) -> Result<Option<(Bytes, Version)>, StorageError>;

    /// Phase B C-B1: read filesystem-level metadata (birthtime, mtime,
    /// size). Returns `Ok(None)` for absent keys. Used by the promotion
    /// gate to defend against tampered `created_at` frontmatter.
    ///
    /// `birthtime` is `Option<DateTime<Utc>>` because not every backend
    /// can determine creation time (FAT32, older Linux kernels, some
    /// network mounts). When `None`, the gate's tamper check abstains
    /// rather than treating the absence as either pass or fail.
    async fn metadata(&self, key: &StorageKey) -> Result<Option<StorageMetadata>, StorageError>;
}

pub(crate) mod sealed {
    /// Private trait — external crates cannot satisfy this, so they
    /// cannot implement [`super::Storage`].
    pub trait Sealed {}
}
