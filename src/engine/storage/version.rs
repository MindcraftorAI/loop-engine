//! Storage version token.
//!
//! Opaque to callers — backends use the inner bytes for their CAS
//! comparison. Local fs uses `mtime_ns + inode + size`; S3 would use
//! the `ETag` header; in-memory uses a monotonic counter.

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version(Box<[u8]>);

impl Version {
    pub fn from_bytes(bytes: impl Into<Box<[u8]>>) -> Self {
        Self(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}
