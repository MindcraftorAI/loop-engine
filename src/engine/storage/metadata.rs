//! Storage metadata — file/key attributes the gate + audit-trail need.
//!
//! Phase B C-B1: introduces `StorageMetadata` carried by the new
//! `Storage::metadata(&key)` method. The promotion gate uses `birthtime`
//! to defend against tampered `created_at` frontmatter — a backdated
//! frontmatter can't outsmart the filesystem's birth time.
//!
//! `birthtime: Option<DateTime<Utc>>` is `Option` because not every FS
//! tracks creation time:
//!   - macOS APFS: yes (st_birthtime / btime)
//!   - Linux ext4 (kernel ≥ 4.11) + statx: yes
//!   - Linux ext4 (older kernels) + classic stat: NO — only mtime
//!   - FAT32, network mounts (some): NO
//!
//! Backends that can't determine birthtime return `None`; the gate
//! falls back to `mtime` with a `BlockReason::TamperedAge` only firing
//! when birthtime is present AND disagrees with frontmatter.

use chrono::{DateTime, Utc};

/// Per-key metadata exposed by [`super::Storage::metadata`]. Limited to
/// the attributes the engine actually consumes; backends may track more
/// internally but only surface this trio.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StorageMetadata {
    /// Filesystem birth time (creation time) if the backend can
    /// determine it. `None` means "this backend cannot prove when the
    /// key was first created" — gate treats this as no tamper signal
    /// (NOT as "definitely tampered").
    pub birthtime: Option<DateTime<Utc>>,
    /// Most-recent modification time if the backend tracks it.
    pub mtime: Option<DateTime<Utc>>,
    /// Size in bytes.
    pub size_bytes: u64,
}
