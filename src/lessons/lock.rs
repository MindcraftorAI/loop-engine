//! Cross-process advisory file locking.
//!
//! Wraps `fd-lock` so callers don't have to think about the underlying
//! syscall surface. Locks are released when the returned guard drops.
//! Use `with_lock` for scoped read-modify-write.
//!
//! Semantics:
//!   - Exclusive blocking lock (`write()` from fd-lock).
//!   - Lock is taken on a SIDECAR file (`.<name>.lock` in the same dir),
//!     NOT on the lesson file itself. Audit Day 12 caught a race in the
//!     naive implementation: when the lesson file is replaced via
//!     atomic rename, the original inode (which held the flock) becomes
//!     unlinked, leaving a window where a second process can open the
//!     new file, take its own flock, and lose the first writer's update.
//!     The sidecar lock file is never renamed — its inode is stable, so
//!     all callers serialize on the same flock target.
//!   - Advisory only: callers that don't take this lock can still race.
//!     TS side adopts the same sidecar pattern as a follow-up.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use fd_lock::RwLock;

/// Compute the sidecar lock file path for a given lesson file.
/// `~/.loop/lessons/active/les-abc.md` → `~/.loop/lessons/active/.les-abc.md.lock`
pub fn sidecar_lock_path(target: &Path) -> Result<PathBuf> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("target has no parent: {}", target.display()))?;
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("target has no filename: {}", target.display()))?;
    Ok(parent.join(format!(".{name}.lock")))
}

/// Run `f` with an exclusive advisory flock held on the SIDECAR lock
/// file for `target`. The sidecar is created if it doesn't exist. The
/// lock is released when this function returns (success or panic).
///
/// The lock guards the LOGICAL operation on `target` — even though the
/// flock is on `<target>.lock` (a separate, stable inode), every caller
/// using `with_lock` serializes through the same kernel-level mutex.
pub fn with_lock<F, T>(target: &Path, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    let lock_path = sidecar_lock_path(target)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening sidecar lock file: {}", lock_path.display()))?;
    let mut lock = RwLock::new(file);
    let _guard = lock
        .write()
        .with_context(|| format!("acquiring exclusive flock on {}", lock_path.display()))?;
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::thread;
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn lock_serializes_concurrent_callers() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target.md");
        std::fs::write(&target, "initial").unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let inside = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let target = target.clone();
                let counter = counter.clone();
                let inside = inside.clone();
                let max_concurrent = max_concurrent.clone();
                thread::spawn(move || {
                    with_lock(&target, || {
                        let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                        max_concurrent.fetch_max(now, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(20));
                        inside.fetch_sub(1, Ordering::SeqCst);
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .unwrap();
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(counter.load(Ordering::SeqCst), 4);
        assert_eq!(
            max_concurrent.load(Ordering::SeqCst),
            1,
            "more than one caller was inside the critical section at once"
        );
    }

    #[test]
    fn lock_releases_on_completion() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target.md");
        std::fs::write(&target, "initial").unwrap();
        with_lock(&target, || Ok(())).unwrap();
        with_lock(&target, || Ok(())).unwrap();
    }

    #[test]
    fn lock_returns_caller_value() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target.md");
        std::fs::write(&target, "data").unwrap();
        let value = with_lock(&target, || Ok(42usize)).unwrap();
        assert_eq!(value, 42);
    }

    #[test]
    fn lock_propagates_caller_error() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target.md");
        std::fs::write(&target, "data").unwrap();
        let result: Result<()> = with_lock(&target, || Err(anyhow!("oops")));
        assert!(result.is_err());
        with_lock(&target, || Ok(())).unwrap();
    }

    /// Audit Day 12 #1: sidecar approach works even if the target file
    /// doesn't exist yet — lock file gets auto-created.
    #[test]
    fn lock_works_when_target_does_not_exist() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("not-yet-existing.md");
        with_lock(&target, || Ok(())).unwrap();
        assert!(sidecar_lock_path(&target).unwrap().exists());
    }

    /// Audit Day 12 #1: sidecar lock survives a target rename so a
    /// second caller blocking on the same target serializes correctly,
    /// even when the locked side does rename-in-critical-section.
    #[test]
    fn lock_survives_target_rename() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target.md");
        std::fs::write(&target, "initial").unwrap();

        let serial = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let inside = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..4)
            .map(|i| {
                let target = target.clone();
                let serial = serial.clone();
                let max_concurrent = max_concurrent.clone();
                let inside = inside.clone();
                thread::spawn(move || {
                    with_lock(&target, || {
                        let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                        max_concurrent.fetch_max(now, Ordering::SeqCst);

                        let tmp = target.with_extension(format!("md.tmp.{i}"));
                        std::fs::write(&tmp, format!("from-thread-{i}")).unwrap();
                        std::fs::rename(&tmp, &target).unwrap();

                        thread::sleep(Duration::from_millis(10));
                        inside.fetch_sub(1, Ordering::SeqCst);
                        serial.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .unwrap();
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(serial.load(Ordering::SeqCst), 4);
        assert_eq!(
            max_concurrent.load(Ordering::SeqCst),
            1,
            "sidecar lock failed to serialize across rename-in-critical-section"
        );
    }

    #[test]
    fn sidecar_path_is_hidden_and_in_same_dir() {
        let target = Path::new("/tmp/foo/les-abc.md");
        let lock = sidecar_lock_path(target).unwrap();
        assert_eq!(lock, PathBuf::from("/tmp/foo/.les-abc.md.lock"));
    }
}
