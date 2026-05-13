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
        _key: &StorageKey,
        _bytes: Bytes,
        _expected_version: Option<&Version>,
    ) -> Result<bool, StorageError> {
        // Phase 3c: implement via sidecar fd-lock + atomic rename, lifting
        // the pattern from src/engine/lessons/lock.rs. Tracked as part of
        // Day 14 Task #45 audit-fix follow-up; lessons-migration in
        // Day 15+ consumes this.
        Err(StorageError::backend(io_err(
            "put_if_version not yet implemented for LocalFsStorage (Phase 3c)"
                .to_string(),
        )))
    }

    async fn get_with_version(
        &self,
        _key: &StorageKey,
    ) -> Result<Option<(Bytes, Version)>, StorageError> {
        Err(StorageError::backend(io_err(
            "get_with_version not yet implemented for LocalFsStorage (Phase 3c)"
                .to_string(),
        )))
    }
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

    #[tokio::test]
    async fn put_if_version_returns_backend_error_in_phase_3b() {
        // Phase 3c will implement; this test pins the current contract
        // so we notice when the stub turns into a real implementation.
        let dir = TempDir::new().unwrap();
        let storage = LocalFsStorage::new(dir.path());
        let ctx = Context::single_user_local();
        let key = StorageKey::lesson(&ctx, "active", "x");
        let result = storage
            .put_if_version(&key, Bytes::from_static(b"x"), None)
            .await;
        assert!(matches!(result, Err(StorageError::Backend(_))));
    }
}
