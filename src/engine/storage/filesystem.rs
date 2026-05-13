//! Local filesystem storage backend.
//!
//! The production backend in single-user mode. Resolves [`StorageKey`]
//! to absolute paths under a configurable root (default `$LOOP_HOME`
//! or `~/.loop/`). Atomic writes via temp-file + rename.
//!
//! **Phase 3b status:** `get` / `put` / `delete` / `list` are
//! implemented. `put_if_version` and `get_with_version` are stubbed
//! (return `StorageError::Backend(...)`) — they land in Phase 3c when
//! the orchestrator (Day 15+) actually needs CAS. See
//! `docs/research/day-14-learn-notes.md` D8 (migration phasing).

use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use bytes::Bytes;

use super::error::StorageError;
use super::key::StorageKey;
use super::sealed::Sealed;
use super::version::Version;
use super::Storage;

/// Local filesystem storage backend.
///
/// Constructed once at daemon startup; held as `Arc<dyn Storage>`
/// throughout. Root path comes from the host's wiring (typically
/// `paths::loop_home()` for single-user mode).
#[derive(Debug, Clone)]
pub struct LocalFsStorage {
    root: PathBuf,
}

impl LocalFsStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve a [`StorageKey`] to an absolute path under [`root`].
    fn resolve(&self, key: &StorageKey) -> PathBuf {
        self.root.join(key.as_str())
    }
}

impl Sealed for LocalFsStorage {}

#[async_trait]
impl Storage for LocalFsStorage {
    async fn get(&self, key: &StorageKey) -> Result<Option<Bytes>, StorageError> {
        let path = self.resolve(key);
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(Some(Bytes::from(bytes))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StorageError::backend(e)),
        }
    }

    async fn put(&self, key: &StorageKey, bytes: Bytes) -> Result<(), StorageError> {
        let path = self.resolve(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(StorageError::backend)?;
        }
        // Atomic write: temp file under same parent → rename. Same-FS
        // rename is guaranteed atomic on POSIX.
        let tmp = path.with_extension(temp_extension(&path));
        tokio::fs::write(&tmp, &bytes)
            .await
            .map_err(StorageError::backend)?;
        tokio::fs::rename(&tmp, &path)
            .await
            .map_err(StorageError::backend)?;
        Ok(())
    }

    async fn delete(&self, key: &StorageKey) -> Result<(), StorageError> {
        let path = self.resolve(key);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StorageError::backend(e)),
        }
    }

    async fn list(&self, prefix: &StorageKey) -> Result<Vec<StorageKey>, StorageError> {
        // Audit Day 14 C2+C3 fix: must match `MemoryStorage::list` semantics —
        // RECURSIVE walk, FILES ONLY (no directories). The trait contract is
        // "all keys under the prefix"; keys are file-addressable blobs, so
        // sub-directories are not keys.
        //
        // Implementation: manual queue-based walk; no external dep needed for
        // the few-thousand-file scale we operate at (a single user's lessons
        // dir has hundreds of files at most).
        let mut out = Vec::new();
        let mut stack: Vec<std::path::PathBuf> = vec![self.resolve(prefix)];

        while let Some(dir) = stack.pop() {
            let mut entries = match tokio::fs::read_dir(&dir).await {
                Ok(e) => e,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(StorageError::backend(e)),
            };
            while let Some(entry) = entries.next_entry().await.map_err(StorageError::backend)? {
                let file_type = entry.file_type().await.map_err(StorageError::backend)?;
                let entry_path = entry.path();
                if file_type.is_dir() {
                    stack.push(entry_path);
                    continue;
                }
                if !file_type.is_file() {
                    // symlinks, fifos, etc. — skip silently
                    continue;
                }
                let relative = entry_path
                    .strip_prefix(&self.root)
                    .map_err(|e| StorageError::backend(io_err(e.to_string())))?;
                let key_str = path_to_key_string(relative);
                out.push(StorageKey::from_raw(key_str));
            }
        }
        Ok(out)
    }

    async fn put_if_version(
        &self,
        key: &StorageKey,
        bytes: Bytes,
        expected_version: Option<&Version>,
    ) -> Result<bool, StorageError> {
        let path = self.resolve(key);
        let expected = expected_version.cloned();
        // Day 16b D1: sync work inside `spawn_blocking` because `fd_lock`
        // is sync and the OS doesn't release flock on tokio suspend (S31).
        tokio::task::spawn_blocking(move || put_if_version_sync(&path, &bytes, expected.as_ref()))
            .await
            .map_err(StorageError::backend)?
    }

    async fn get_with_version(
        &self,
        key: &StorageKey,
    ) -> Result<Option<(Bytes, Version)>, StorageError> {
        let path = self.resolve(key);
        // Day 16b D1: same spawn_blocking discipline.
        tokio::task::spawn_blocking(move || get_with_version_sync(&path))
            .await
            .map_err(StorageError::backend)?
    }
}

/// Sync impl: hold sidecar lock during read + stat + write so the
/// `(bytes, version)` pair stays coherent (Day 16b D1 + S33 prevention).
fn put_if_version_sync(
    path: &Path,
    bytes: &Bytes,
    expected_version: Option<&Version>,
) -> Result<bool, StorageError> {
    crate::engine::storage::lock::with_sidecar_lock(path, || {
        // Verify the expected_version under the lock.
        let current = read_version_sync(path)?;
        match (current.as_ref(), expected_version) {
            (None, None) => {
                // Create-only: no current version expected; write fresh.
                atomic_write_sync(path, bytes)?;
                Ok(true)
            }
            (Some(cur), Some(exp)) if cur == exp => {
                // CAS match: write.
                atomic_write_sync(path, bytes)?;
                Ok(true)
            }
            _ => Ok(false), // version mismatch (or expected None but file exists)
        }
    })
}

/// Sync impl: hold sidecar lock during read+stat so a concurrent
/// writer can't rotate the file between the two syscalls.
fn get_with_version_sync(path: &Path) -> Result<Option<(Bytes, Version)>, StorageError> {
    crate::engine::storage::lock::with_sidecar_lock(path, || match std::fs::read(path) {
        Ok(data) => {
            // Stat for the version AFTER read so any rotation between
            // syscalls is impossible — we hold the sidecar lock through
            // both. Mtime + len encode the version (Day 16b D2).
            let version = compute_version_sync(path)?;
            Ok(Some((Bytes::from(data), version)))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StorageError::backend(e)),
    })
}

/// Read the on-disk version (`mtime_ns + len`, 24 bytes LE). Returns
/// None if the file doesn't exist.
fn read_version_sync(path: &Path) -> Result<Option<Version>, StorageError> {
    match std::fs::metadata(path) {
        Ok(_) => compute_version_sync(path).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StorageError::backend(e)),
    }
}

/// Audit Day 16b M2: `cfg(unix)`-gate the Unix-specific mtime path.
/// Other platforms fall back to a `SystemTime`-derived version that's
/// also 24 bytes (nanoseconds-since-UNIX-EPOCH || len).
#[cfg(unix)]
fn compute_version_sync(path: &Path) -> Result<Version, StorageError> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).map_err(StorageError::backend)?;
    let mtime_secs = meta.mtime();
    let mtime_nsec = meta.mtime_nsec();
    let mtime_ns: i128 = (mtime_secs as i128) * 1_000_000_000 + (mtime_nsec as i128);
    let len = meta.len();
    let mut bytes = [0u8; 24];
    bytes[..16].copy_from_slice(&mtime_ns.to_le_bytes());
    bytes[16..24].copy_from_slice(&len.to_le_bytes());
    Ok(Version::from_bytes(bytes.to_vec()))
}

#[cfg(not(unix))]
fn compute_version_sync(path: &Path) -> Result<Version, StorageError> {
    use std::time::UNIX_EPOCH;
    let meta = std::fs::metadata(path).map_err(StorageError::backend)?;
    let mtime = meta.modified().map_err(StorageError::backend)?;
    let dur = mtime.duration_since(UNIX_EPOCH).unwrap_or_default();
    let mtime_ns: i128 = (dur.as_secs() as i128) * 1_000_000_000 + (dur.subsec_nanos() as i128);
    let len = meta.len();
    let mut bytes = [0u8; 24];
    bytes[..16].copy_from_slice(&mtime_ns.to_le_bytes());
    bytes[16..24].copy_from_slice(&len.to_le_bytes());
    Ok(Version::from_bytes(bytes.to_vec()))
}

/// Write `bytes` to `path` via temp-file + atomic rename. Caller must
/// hold the sidecar lock already.
///
/// Audit Day 16b minor: if `rename` fails, clean up the tmp file so we
/// don't leave orphans accumulating in the directory.
fn atomic_write_sync(path: &Path, bytes: &Bytes) -> Result<(), StorageError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(StorageError::backend)?;
    }
    let tmp = path.with_extension(temp_extension(path));
    std::fs::write(&tmp, bytes).map_err(StorageError::backend)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        // Best-effort cleanup; ignore secondary error.
        let _ = std::fs::remove_file(&tmp);
        return Err(StorageError::backend(e));
    }
    Ok(())
}

fn io_err(msg: String) -> io::Error {
    io::Error::other(msg)
}

fn temp_extension(path: &Path) -> String {
    let existing = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    if existing.is_empty() {
        "tmp".to_string()
    } else {
        format!("{existing}.tmp")
    }
}

fn path_to_key_string(p: &Path) -> String {
    // Always slash-delimited at the key level, even on Windows.
    p.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::Context;
    use tempfile::TempDir;

    #[tokio::test]
    async fn put_then_get_round_trip() {
        let dir = TempDir::new().unwrap();
        let storage = LocalFsStorage::new(dir.path());
        let ctx = Context::single_user_local();
        let key = StorageKey::lesson(&ctx, "active", "les-xyz");

        storage
            .put(&key, Bytes::from_static(b"hello"))
            .await
            .unwrap();
        let got = storage.get(&key).await.unwrap();
        assert_eq!(got.unwrap().as_ref(), b"hello");
    }

    #[tokio::test]
    async fn get_returns_none_for_missing() {
        let dir = TempDir::new().unwrap();
        let storage = LocalFsStorage::new(dir.path());
        let ctx = Context::single_user_local();
        let key = StorageKey::lesson(&ctx, "active", "missing");
        assert!(storage.get(&key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let storage = LocalFsStorage::new(dir.path());
        let ctx = Context::single_user_local();
        let key = StorageKey::lesson(&ctx, "active", "transient");
        // delete missing == Ok
        storage.delete(&key).await.unwrap();
        storage.put(&key, Bytes::from_static(b"x")).await.unwrap();
        storage.delete(&key).await.unwrap();
        assert!(storage.get(&key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn put_is_atomic_no_partial_observable() {
        // We can't directly observe atomicity from a test, but we can
        // verify the absence of leftover .tmp files after a successful put.
        let dir = TempDir::new().unwrap();
        let storage = LocalFsStorage::new(dir.path());
        let ctx = Context::single_user_local();
        let key = StorageKey::lesson(&ctx, "active", "les-atomic");
        storage
            .put(&key, Bytes::from_static(b"final"))
            .await
            .unwrap();
        let parent = dir.path().join("lessons/active");
        let tmps: Vec<_> = std::fs::read_dir(&parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(|s| s.contains("tmp"))
                    .unwrap_or(false)
            })
            .collect();
        assert!(tmps.is_empty(), "expected no leftover .tmp file");
    }

    #[tokio::test]
    async fn list_is_recursive_and_filters_to_files() {
        // Audit C2+C3 regression test: backend semantics must match
        // MemoryStorage — recursive walk + files only (no directories).
        let dir = TempDir::new().unwrap();
        let storage = LocalFsStorage::new(dir.path());
        let ctx = Context::single_user_local();

        // Three lessons under two status dirs (sub-directories).
        for (status, id) in [
            ("active", "a"),
            ("active", "b"),
            ("archived", "c"),
        ] {
            storage
                .put(&StorageKey::lesson(&ctx, status, id), Bytes::from_static(b"x"))
                .await
                .unwrap();
        }

        let prefix = StorageKey::from_raw("lessons".into());
        let mut keys: Vec<String> = storage
            .list(&prefix)
            .await
            .unwrap()
            .into_iter()
            .map(|k| k.as_str().to_string())
            .collect();
        keys.sort();
        // All three lesson FILES surface; no directory entries.
        assert_eq!(
            keys,
            vec![
                "lessons/active/a.md",
                "lessons/active/b.md",
                "lessons/archived/c.md",
            ],
            "expected recursive walk returning only files, got {:?}",
            keys
        );
    }

    #[tokio::test]
    async fn list_matches_memory_storage_semantics() {
        // Same expected output from both backends for the same put sequence.
        let dir = TempDir::new().unwrap();
        let fs_storage = LocalFsStorage::new(dir.path());
        let mem_storage = super::super::MemoryStorage::default();
        let ctx = Context::single_user_local();

        for (status, id) in [
            ("active", "a"),
            ("active", "b"),
            ("archived", "c"),
        ] {
            let key = StorageKey::lesson(&ctx, status, id);
            fs_storage.put(&key, Bytes::from_static(b"x")).await.unwrap();
            mem_storage
                .put(&key, Bytes::from_static(b"x"))
                .await
                .unwrap();
        }

        let prefix = StorageKey::from_raw("lessons".into());
        let mut fs_keys: Vec<String> = fs_storage
            .list(&prefix)
            .await
            .unwrap()
            .into_iter()
            .map(|k| k.as_str().to_string())
            .collect();
        let mut mem_keys: Vec<String> = mem_storage
            .list(&prefix)
            .await
            .unwrap()
            .into_iter()
            .map(|k| k.as_str().to_string())
            .collect();
        fs_keys.sort();
        mem_keys.sort();
        assert_eq!(fs_keys, mem_keys);
    }

    // Day 14 stub-pin test `put_if_version_returns_backend_error_in_phase_3b`
    // RETIRED in Day 16b — replaced by the regression tests below now that
    // the impl is live.

    // ---- put_if_version / get_with_version regression tests (Day 16b D1) ----

    #[tokio::test]
    async fn cas_create_only_succeeds_on_absent_then_fails() {
        let dir = TempDir::new().unwrap();
        let storage = LocalFsStorage::new(dir.path());
        let ctx = Context::single_user_local();
        let key = StorageKey::lesson(&ctx, "active", "les-cas1");

        // Create-only: file absent → success.
        let ok = storage
            .put_if_version(&key, Bytes::from_static(b"first"), None)
            .await
            .unwrap();
        assert!(ok);

        // Create-only again: file now present → fail.
        let ok = storage
            .put_if_version(&key, Bytes::from_static(b"second"), None)
            .await
            .unwrap();
        assert!(!ok);
        // Original value preserved.
        assert_eq!(storage.get(&key).await.unwrap().unwrap().as_ref(), b"first");
    }

    #[tokio::test]
    async fn cas_rmw_round_trip() {
        let dir = TempDir::new().unwrap();
        let storage = LocalFsStorage::new(dir.path());
        let ctx = Context::single_user_local();
        let key = StorageKey::lesson(&ctx, "active", "les-cas2");

        storage.put(&key, Bytes::from_static(b"v1")).await.unwrap();
        let (_, v1) = storage.get_with_version(&key).await.unwrap().unwrap();

        // CAS with the correct version succeeds.
        let ok = storage
            .put_if_version(&key, Bytes::from_static(b"v2"), Some(&v1))
            .await
            .unwrap();
        assert!(ok);
        assert_eq!(storage.get(&key).await.unwrap().unwrap().as_ref(), b"v2");

        // CAS with the stale v1 fails; v2 stays.
        let ok = storage
            .put_if_version(&key, Bytes::from_static(b"v3"), Some(&v1))
            .await
            .unwrap();
        assert!(!ok);
        assert_eq!(storage.get(&key).await.unwrap().unwrap().as_ref(), b"v2");
    }

    #[tokio::test]
    async fn get_with_version_returns_none_when_missing() {
        let dir = TempDir::new().unwrap();
        let storage = LocalFsStorage::new(dir.path());
        let ctx = Context::single_user_local();
        let key = StorageKey::lesson(&ctx, "active", "les-missing");
        assert!(storage.get_with_version(&key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn version_changes_on_each_put() {
        // Different content → different len → different version.
        let dir = TempDir::new().unwrap();
        let storage = LocalFsStorage::new(dir.path());
        let ctx = Context::single_user_local();
        let key = StorageKey::lesson(&ctx, "active", "les-v");

        storage.put(&key, Bytes::from_static(b"short")).await.unwrap();
        let (_, v1) = storage.get_with_version(&key).await.unwrap().unwrap();
        storage
            .put(&key, Bytes::from_static(b"a longer body now"))
            .await
            .unwrap();
        let (_, v2) = storage.get_with_version(&key).await.unwrap().unwrap();
        assert_ne!(v1, v2);
    }

    #[tokio::test]
    async fn cas_against_expected_none_fails_when_file_exists() {
        let dir = TempDir::new().unwrap();
        let storage = LocalFsStorage::new(dir.path());
        let ctx = Context::single_user_local();
        let key = StorageKey::lesson(&ctx, "active", "les-conflict");
        storage.put(&key, Bytes::from_static(b"existing")).await.unwrap();
        // Caller mistakenly thinks the file doesn't exist.
        let ok = storage
            .put_if_version(&key, Bytes::from_static(b"new"), None)
            .await
            .unwrap();
        assert!(!ok);
        assert_eq!(
            storage.get(&key).await.unwrap().unwrap().as_ref(),
            b"existing"
        );
    }

    #[tokio::test]
    async fn cas_against_expected_some_fails_when_file_missing() {
        let dir = TempDir::new().unwrap();
        let storage = LocalFsStorage::new(dir.path());
        let ctx = Context::single_user_local();
        let key = StorageKey::lesson(&ctx, "active", "les-phantom");
        // Caller has a stale version for a file that no longer exists.
        let fake_v = Version::from_bytes(vec![0u8; 24]);
        let ok = storage
            .put_if_version(&key, Bytes::from_static(b"data"), Some(&fake_v))
            .await
            .unwrap();
        assert!(!ok);
        // File still doesn't exist.
        assert!(storage.get(&key).await.unwrap().is_none());
    }
}
