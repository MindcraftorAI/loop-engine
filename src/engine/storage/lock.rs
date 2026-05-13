//! Cross-process advisory file locking — sidecar-flock pattern.
//!
//! **Lift from `engine::lessons::lock`** per Day 16b D1. The original
//! `lessons::lock::with_lock` is preserved for backward compat (Day 11/12
//! sync API uses it). New async storage CAS callers go through these
//! helpers inside `tokio::task::spawn_blocking` (D1 — `fd_lock` is sync
//! and the OS doesn't release flock on a tokio future suspend).
//!
//! Semantics (verbatim from the original):
//! - Exclusive blocking lock (`fd_lock::RwLock::write()`).
//! - Lock is taken on a SIDECAR file (`.<name>.lock` in the same dir),
//!   NOT on the target file itself. Audit Day 12 caught a race in the
//!   naive implementation: when the target file is replaced via atomic
//!   rename, the original inode (which held the flock) becomes unlinked.
//!   The sidecar file's inode is stable, so callers serialize correctly.
//! - Advisory only: callers that don't take the lock can still race.
//!
//! Day 16b verification: TS-side uses in-process `async-mutex` only
//! (NOT flock — `loop-archive-2026-05-13/core-ts/src/lib/file-mutex.ts`
//! lines 8-11 explicitly reject `proper-lockfile`). Rust sidecar-flock
//! is strictly stronger than the TS pattern; no compat regression.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use fd_lock::RwLock;

use crate::engine::storage::error::StorageError;

/// Compute the sidecar lock path for `target`:
/// `parent_dir/.<filename>.lock`. Used for cross-process advisory
/// serialization where the target file may be replaced via atomic
/// rename.
pub(crate) fn sidecar_lock_path(target: &Path) -> Result<PathBuf, StorageError> {
    let parent = target.parent().ok_or_else(|| {
        StorageError::backend(std::io::Error::other(format!(
            "target has no parent: {}",
            target.display()
        )))
    })?;
    let name = target.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        StorageError::backend(std::io::Error::other(format!(
            "target has no filename: {}",
            target.display()
        )))
    })?;
    Ok(parent.join(format!(".{name}.lock")))
}

/// Run `f` with an exclusive advisory flock held on the SIDECAR file
/// for `target`. Creates the lock file if it doesn't exist.
///
/// **Sync — must be called inside `tokio::task::spawn_blocking` from
/// async contexts.** `fd_lock::RwLock::write()` blocks the current OS
/// thread.
pub(crate) fn with_sidecar_lock<F, T>(target: &Path, f: F) -> Result<T, StorageError>
where
    F: FnOnce() -> Result<T, StorageError>,
{
    let lock_path = sidecar_lock_path(target)?;
    // Ensure the parent directory exists so the lock file can be created.
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(StorageError::backend)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(StorageError::backend)?;
    let mut lock = RwLock::new(file);
    let _guard = lock.write().map_err(StorageError::backend)?;
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn sidecar_path_is_hidden_and_in_same_dir() {
        let target = Path::new("/tmp/foo/les-abc.md");
        let lock = sidecar_lock_path(target).unwrap();
        assert_eq!(lock, PathBuf::from("/tmp/foo/.les-abc.md.lock"));
    }

    #[test]
    fn lock_serializes_concurrent_callers() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target.md");

        let inside = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let target = target.clone();
                let inside = inside.clone();
                let max_concurrent = max_concurrent.clone();
                thread::spawn(move || {
                    with_sidecar_lock(&target, || {
                        let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                        max_concurrent.fetch_max(now, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(15));
                        inside.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .unwrap();
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            max_concurrent.load(Ordering::SeqCst),
            1,
            "more than one caller inside the critical section at once"
        );
    }

    #[test]
    fn lock_works_when_target_does_not_exist() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("not-yet.md");
        with_sidecar_lock(&target, || Ok(())).unwrap();
        assert!(sidecar_lock_path(&target).unwrap().exists());
    }
}
