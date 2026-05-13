# Day 16b Pre-Research: Persistence Migration + EngineError + TestHarness

**Date:** 2026-05-13
**Cycle phase:** Pre-research (workflow cycle phase 1)
**Cycle:** Day 16b (`put_if_version` / `get_with_version` impls + lessons → `(ctx, storage)` migration + `StorageBackedSignalWriter` + `EngineError` + `TestHarness`)
**Predecessors:** `day-16-pre-research.md` Q5/Q6/Q8, `day-16a-post-research.md`, `day-16a-audit-report.md`, `day-16a-learn-notes.md`, `day-14-learn-notes.md` D7/D8
**Hard rule:** `feedback_rust_idiomatic_refactor.md` — pre-research idiomatic Rust BEFORE porting; TS is *what*, not *how*. No guesswork.

---

## Scope summary (verbatim from prompt)

1. `LocalFsStorage::put_if_version` + `get_with_version` impls — lift `engine::lessons::lock::with_sidecar_lock` pattern.
2. Lessons module migration to `(&Context, &dyn Storage)` — retire `paths::loop_home()` direct calls; replace internal sidecar-flock with `Storage::put_if_version`.
3. `StorageBackedSignalWriter` — replaces 16a's `LoggingSignalWriter` as the orchestrator's signal output sink in production.
4. Test migration — `ENV_LOCK` + `with_temp_loop_home` retire; tests adopt `TestHarness { ctx, storage: MemoryStorage }` (`engine::test_support`).
5. `EngineError` enum — replaces `anyhow::Error` in engine public signatures (crate-level per OQ-D16b-3).

Estimated total build: ~1100–1400 LOC across 8 files; ~25 new tests; ~7 existing tests rewritten.

---

## Background grounding (verified before writing this doc)

### What's actually on disk today (2026-05-13)

- `src/engine/lessons/lock.rs:46` — `with_lock<F, T>(target: &Path, f: F) -> Result<T>` using `fd_lock::RwLock` on a sidecar `.<name>.lock` file. 127-test-validated; lift target for `put_if_version`.
- `src/engine/lessons/loader.rs:42` — `get_lesson_by_id(id: &str) -> Result<Option<LoadedLesson>>` scans `paths::LESSON_STATUS_DIRS` via `paths::lessons_status_dir(status)?.join(...)`. Inherently `&Context`-less and synchronous.
- `src/engine/lessons/signals.rs:52` — `record_sentiment_signal(id, polarity) -> Result<LoadedLesson>` calls `get_lesson_by_id` + `with_lock(&path, ||...)` + atomic-rename write.
- `src/engine/storage/filesystem.rs:126-150` — Day 14 stub: both `put_if_version` and `get_with_version` return `StorageError::Backend("... not yet implemented for LocalFsStorage (Phase 3c)")`.
- `src/engine/storage/version.rs:8` — `Version(Box<[u8]>)` opaque newtype, with `from_bytes` + `as_bytes`.
- `src/engine/storage/memory.rs:69-96` — `MemoryStorage` already implements both CAS methods correctly (atomic per the inner Mutex).
- `src/engine/paths.rs:22` — `pub(crate) static ENV_LOCK` (Day 14 m3 audit fix). 7 tests across `loader.rs` + `signals.rs` + `paths.rs` join it.
- `src/engine/sentiment/signals.rs:131-166` — `SignalWriter` trait + `LoggingSignalWriter` + `MockSignalWriter`. Orchestrator's output sink seam (Day 16a D13).
- `src/engine/sentiment/orchestrator/mod.rs:572-line file` — three submodules (`config`, `derive`, `state`); writer is injected as `Arc<dyn SignalWriter>` so 16b swaps without touching the orchestrator.
- `src/main.rs` — **no Orchestrator wired in yet.** Day 16a built the orchestrator type but didn't construct one in the daemon binary. Day 16b's wiring is therefore the FIRST production construction.
- `Cargo.toml:57` — `fd-lock = "4"` already a direct dep. No new deps needed for 16b.
- `Cargo.toml:73, 89, 94` — `async-trait`, `dashmap`, `tokio-stream` already direct deps.

### TS reference reality check (`loop-archive-2026-05-13/core-ts/`)

- `core-ts/src/lib/file-mutex.ts:1-50` — TS uses **in-process `async-mutex` only** (`new Mutex(); mutexes.set(path, m)`), NOT a cross-process flock. The comment on line 8-11 explicitly rejects `proper-lockfile` as "the wrong primitive."
- `core-ts/src/lessons/signals.ts:22, 50, 78, 122, 156` — every signal write uses `withFileLock(initialLookup.path, async () => { ... atomicWriteUtf8(...) })`.
- `core-ts/src/lessons/loader.ts:316-322` — `atomicWriteUtf8(file, contents)` = `writeFile(tmp); rename(tmp, file)`. No fsync.

**Consequence for Q5:** The Rust sidecar-flock is **strictly stronger** than the TS in-process mutex. Cross-process safety today is "atomic-rename only" — if TS writes a lesson while Rust is mid-RMW, the TS write may or may not survive depending on Rust's flock timing. The Rust side adding flock does NOT degrade TS behavior; the TS side could optionally adopt the same sidecar pattern later (it's an MIT crate of equivalent capability). 16b ships **flock-on-Rust-side, in-process-mutex-on-TS-side** — same as today, no cross-process degradation, and Rust→Rust contention is correctly serialized.

### Day 16a learnings carrying into 16b

- L1: orchestrator.rs is now SPLIT into `orchestrator/{mod, config, derive, state}.rs` (audit M1 fix) — total ~1045 LOC across 4 files. M1 closed.
- L2: `last_assistant_turn_at` is set but no caller pushes assistant turns. Day 16a audit C2 added `Orchestrator::push_assistant_turn` to fix this. 16b's smoke test verifies it.
- L3: `MemoryStorage` not yet exercised by orchestrator-shaped tests; 16b's `StorageBackedSignalWriter` is the FIRST real consumer.
- L4: `JsonlWatcherSource` integration test deferred to Day 17.
- M4 (audit): `loaded_items` is hard-coded `Vec::new()` in `process_event`; manifest assembly lives elsewhere. 16b adds the seam via `Orchestrator::update_manifest`.
- OQ-D16b-6: orchestrator.rs split happened in 16a audit phase — **closed.**
- OQ-D16b-7: `StorageBackedSignalWriter` integration — answered in this doc (Q4).

### What 16b does NOT touch (deferred)

- Manifest assembly (lessons → orchestrator). The `LoadedItem` seam exists via `Orchestrator::update_manifest`; populating it from `lessons::list` is Day 17.
- `JsonlWatcherSource` end-to-end integration smoke (L4 → Day 17).
- Pretrigger wiring (m4 → Day 17 or whenever pretrigger is exercised).
- `EngineError` adoption in `lifecycle.rs`, `pid.rs`, `buffer.rs` — these stay `anyhow` for one cycle. Only `lessons/*` migrates in 16b.
- 2024 edition bump (deferred, separate audit).

---

## Q1: `LocalFsStorage::put_if_version` implementation strategy

### Decision (confirmed from Day 16 pre-research Q5)

**Lift the existing `engine::lessons::lock::with_lock` pattern** as a private helper inside `engine::storage::filesystem`. Reasons:

1. **127-test-validated correctness.** Day 12 audit's `lock_serializes_concurrent_callers` + `lock_survives_target_rename` tests prove the sidecar-on-stable-inode pattern. Re-deriving CAS over content-hash or version-file would re-invite the inode-reuse-after-rename hazard Day 12 originally caught.
2. **TS cross-process compat.** TS uses in-process mutex only (verified above). Rust's sidecar flock is strictly stronger; cross-process behavior is at least as safe as today.
3. **Single source of code.** The `with_lock` helper currently lives in `engine::lessons::lock`. 16b moves it to `engine::storage::lock` (private to the storage module) and re-exports from `engine::lessons::lock` for one cycle. Storage CAS is the new primary consumer; lessons (post-migration) calls Storage, not the lock directly.

### `Version` encoding (confirmed from Day 16 pre-research Q5)

**`mtime_ns (i128, 16 bytes) + len (u64, 8 bytes)` = 24 bytes total.**

Rationale:
- `mtime_ns` alone is too coarse on APFS (millisecond resolution). Adding `len` catches the rare same-mtime-different-content case.
- Adding `inode` (32 bytes total) is unnecessary for our atomic-rename pattern: rename swaps inodes, so a stale inode is auto-detected by the mtime delta. We accept the marginal collision risk.
- Adding SHA-256 content hash (O(file size)) is overkill for ≤64KB lesson files; mtime+len has zero false-positives in production after Day 12's atomic-rename pattern.
- All-zero version (`[0u8; 24]`) is reserved for "file absent" — semantically distinct from any real version. Used internally; `Option<Version>` at the API boundary preserves the existing contract.

Implementation note: `std::fs::Metadata::modified()` returns `SystemTime`. Convert with `.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as i128`. On APFS the low ~6 digits are always zero (millisecond clamp); not a correctness issue, but worth a docstring comment.

### `with_lock` lift — where it lives

**Recommend: move `engine::lessons::lock` → `engine::storage::lock` (crate-private).** Commit cadence:

- Commit A: move file + re-export `pub use crate::engine::storage::lock::{with_lock, sidecar_lock_path}` from `engine::lessons::lock` to preserve the old import path. All 127 tests still pass.
- Commit B: `put_if_version` + `get_with_version` implementations land using the new path. New 16b tests pass.
- Commit C (later in the cycle): `record_sentiment_signal` migrates to call `Storage::put_if_version` instead of `with_lock` directly. The re-export from `engine::lessons::lock` becomes empty.
- Commit D (final 16b commit): retire the empty `engine::lessons::lock` re-export.

### Concrete code sketch — `put_if_version`

```rust
// src/engine/storage/filesystem.rs (16b)

use super::lock::{sidecar_lock_path, with_lock_sync};
use super::version::Version;

const VERSION_ENCODING_LEN: usize = 24; // 16 bytes mtime_ns + 8 bytes len

async fn put_if_version(
    &self,
    key: &StorageKey,
    bytes: Bytes,
    expected_version: Option<&Version>,
) -> Result<bool, StorageError> {
    let path = self.resolve(key);
    let expected_owned = expected_version.cloned();
    let bytes_owned = bytes;

    // fd_lock::RwLock is sync; the OS does NOT release the flock when a tokio
    // future suspends. `spawn_blocking` keeps the lock-acquire + RMW on a
    // blocking pool thread where suspension can't strand the lock.
    tokio::task::spawn_blocking(move || -> Result<bool, StorageError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(StorageError::backend)?;
        }
        with_lock_sync(&path, || -> Result<bool, StorageError> {
            let current = read_version_sync(&path)?;
            if current.as_ref() != expected_owned.as_ref() {
                return Ok(false); // CAS lost
            }
            atomic_write_sync(&path, &bytes_owned)?;
            Ok(true)
        })
    })
    .await
    .map_err(|join_err| StorageError::backend(join_err))?
}

// In src/engine/storage/lock.rs (16b — moved from lessons/lock.rs)
pub(crate) fn with_lock_sync<F, T>(target: &Path, f: F) -> Result<T, StorageError>
where F: FnOnce() -> Result<T, StorageError>,
{
    let lock_path = sidecar_lock_path(target)?;
    let file = OpenOptions::new()
        .read(true).write(true).create(true).truncate(false)
        .open(&lock_path)
        .map_err(StorageError::backend)?;
    let mut lock = fd_lock::RwLock::new(file);
    let _guard = lock.write().map_err(StorageError::backend)?;
    f()
}

// Filesystem helpers (private to storage/filesystem.rs)
fn read_version_sync(path: &Path) -> Result<Option<Version>, StorageError> {
    match std::fs::metadata(path) {
        Ok(m) => {
            let mtime_ns = m.modified()
                .map_err(StorageError::backend)?
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as i128;
            let len = m.len();
            let mut buf = [0u8; VERSION_ENCODING_LEN];
            buf[..16].copy_from_slice(&mtime_ns.to_le_bytes());
            buf[16..].copy_from_slice(&len.to_le_bytes());
            Ok(Some(Version::from_bytes(buf.to_vec().into_boxed_slice())))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StorageError::backend(e)),
    }
}

fn atomic_write_sync(path: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    let tmp = staged_tmp_path(path)?;
    let mut f = OpenOptions::new()
        .write(true).create_new(true)
        .open(&tmp).map_err(StorageError::backend)?;
    f.write_all(bytes).map_err(StorageError::backend)?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(StorageError::backend)?;
    Ok(())
}

fn staged_tmp_path(target: &Path) -> Result<PathBuf, StorageError> {
    let stem = target.file_name().and_then(|n| n.to_str())
        .ok_or_else(|| StorageError::backend(io::Error::other("target path has no filename")))?;
    let parent = target.parent()
        .ok_or_else(|| StorageError::backend(io::Error::other("no parent")))?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let pid = std::process::id();
    Ok(parent.join(format!(".{stem}.tmp.{pid}.{ts}")))
}
```

### Failure modes

| Failure | Handling | Surfaces as |
|---|---|---|
| Lock acquisition fails (EBADF, EWOULDBLOCK after blocking — won't happen; we use blocking `write()`) | Bubble as `StorageError::Backend` | Caller retries or aborts |
| Stat fails for an existing-but-permission-denied file | `StorageError::Backend(io::Error)` | Caller maps to `EngineError::Storage(_)` |
| Lock acquired but file deleted by another process between stat and write | `Ok(false)` (version was Some, current None — mismatch) | Caller re-reads |
| Atomic-rename fails (cross-filesystem, EXDEV) | `StorageError::Backend` | Caller diagnoses (shouldn't happen — temp file is in same parent) |
| `spawn_blocking` panics inside the closure | `tokio::task::JoinError` → `StorageError::Backend` | Caller surfaces as outage |
| Process killed mid-write | Sidecar file orphaned (no cleanup needed — it's small + reused on next call). The temp file may exist; the next put cleans it via `create_new` failure → fallback rename | No-op (manual cleanup is acceptable) |

### Regression tests (16b ships ~7 storage tests, retiring the `put_if_version_returns_backend_error_in_phase_3b` stub-pin)

1. `put_if_version_create_only_succeeds_on_absent` — `expected = None`, key absent → `Ok(true)`, file present.
2. `put_if_version_create_only_fails_on_present` — `expected = None`, key present → `Ok(false)`, file unchanged.
3. `put_if_version_succeeds_with_matching_version` — write A, read version, write B with that version → `Ok(true)`; verify content == B.
4. `put_if_version_fails_with_stale_version` — write A, read v1, write B-via-CAS, attempt write-C-with-v1 → `Ok(false)`; verify content unchanged from B.
5. `concurrent_cas_serializes_across_threads` — N=4 threads RMW the same key 50 times each; tally `Ok(true)` + `Ok(false)` counts and verify final state matches an expected serialization (commutative final value).
6. `get_with_version_then_put_if_version_round_trip` — write, read with version, modify, put with that version → CAS succeeds, version advances.
7. `sidecar_lock_inode_stable_across_atomic_rename` — write A, capture sidecar inode; write B via CAS (atomic rename of target); capture sidecar inode; assert equal.

### Trade-offs summary

| Option | Pros | Cons | Verdict |
|---|---|---|---|
| Lift sidecar-flock (chosen) | 127-test-validated; preserves cross-process compat; minimal code change | flock is sync → `spawn_blocking` overhead per CAS | ✅ |
| Content-hash CAS | No flock dep | O(file size) per read; race window between hash and rename; no TS compat | ✗ |
| Per-key sidecar version file | Composable with cloud blobstores | More files; same flock dep; no clear win | ✗ |
| In-memory lock-manager + atomic rename | No flock | Loses cross-process | ✗ |

---

## Q2: `get_with_version` atomicity

### Problem statement (per S28)

A naive `read + stat` is racy:
1. Reader reads bytes A (version A).
2. Writer atomic-renames new content B (version B).
3. Reader stats → version B.
4. Reader returns `(bytes_A, version_B)`. CAS will succeed when it shouldn't.

### Recommendation: hold the sidecar lock during read + stat

Same flock that `put_if_version` takes. Cross-process serialization preserved. Cost: cross-process contention; mitigated by short critical section (one read + one stat).

### Code sketch — `get_with_version`

```rust
async fn get_with_version(
    &self,
    key: &StorageKey,
) -> Result<Option<(Bytes, Version)>, StorageError> {
    let path = self.resolve(key);
    tokio::task::spawn_blocking(move || -> Result<Option<(Bytes, Version)>, StorageError> {
        if !path.exists() {
            return Ok(None);
        }
        with_lock_sync(&path, || -> Result<Option<(Bytes, Version)>, StorageError> {
            // Order matters: stat AFTER read, then atomic-rename invariant
            // protects us — if a writer landed between read and stat, the
            // mtime/len reflect the new file (since rename swaps in one step).
            //
            // Actually — stat FIRST then read is also safe because the lock
            // serializes against the writer. Both orders work under the lock.
            // Choosing read-then-stat to keep `Option<None>` handling cheaper
            // (skip the read on absent).
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(StorageError::backend(e)),
            };
            let version = match read_version_sync(&path)? {
                Some(v) => v,
                None => return Ok(None), // raced with delete; treat as absent
            };
            Ok(Some((Bytes::from(bytes), version)))
        })
    })
    .await
    .map_err(StorageError::backend)?
}
```

### Race-resistance test (S28 regression pin)

```rust
#[tokio::test]
async fn get_with_version_consistent_under_concurrent_write() {
    let tmp = TempDir::new().unwrap();
    let storage = Arc::new(LocalFsStorage::new(tmp.path()));
    let ctx = Context::single_user_local();
    let key = StorageKey::lesson(&ctx, "active", "race");

    // Seed.
    storage.put(&key, Bytes::from_static(b"v0")).await.unwrap();

    // 4 readers + 4 writers in tight loops for 200 iterations each.
    let mut tasks = vec![];
    for i in 0..4 {
        let storage = storage.clone();
        let key = key.clone();
        tasks.push(tokio::spawn(async move {
            for j in 0..200 {
                let payload = format!("w{i}-{j}");
                let (_b, v) = storage.get_with_version(&key).await.unwrap().unwrap();
                let _ = storage.put_if_version(&key, Bytes::from(payload), Some(&v)).await.unwrap();
            }
        }));
    }
    for i in 0..4 {
        let storage = storage.clone();
        let key = key.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..200 {
                let (bytes, version) = storage.get_with_version(&key).await.unwrap().unwrap();
                // Pin: the returned bytes and version are mutually consistent.
                // We verify by re-stat'ing under no lock — if version drift
                // happened, the re-read might disagree, which would prove a
                // race. The lock-correctness invariant: bytes correspond to
                // the on-disk content as-of the version returned.
                let _ = (bytes, version, i); // probe-only
            }
        }));
    }
    for t in tasks { t.await.unwrap(); }
}
```

A stronger pin: store `(bytes_len, version_bytes_last_8_le_as_u64)` as the "should be consistent" pair (since our `Version` encodes `mtime + len`, the trailing 8 bytes ARE the length). Assert `version_trailing_8 == bytes.len() as u64` for every successful read.

### Trade-offs

| Option | Pros | Cons | Verdict |
|---|---|---|---|
| Hold sidecar flock during read + stat (chosen) | Symmetric with `put_if_version`; cross-process safe | Cross-process contention on reads | ✅ |
| Read-then-retry-on-mtime-change | No flock acquisition on reads (faster) | Still has split-read race during writer rename | ✗ |
| Memory-map + inode-pin | Pinned bytes immune to rename | Overkill; complex; doesn't compose with TS atomic-rename | ✗ |

---

## Q3: Lessons migration plan — incremental order, commit cadence

### Pre-research Q6 already locked: leaf-first, delegating wrappers, EngineError introduction

Sub-questions answered below.

### Order of file migrations

Dependency graph (16b state):
- `loader.rs` → uses `paths::lessons_status_dir` (synchronous)
- `signals.rs` → uses `loader::get_lesson_by_id` + `lock::with_lock` + `yaml::*` (synchronous)
- `lock.rs` → uses `fd_lock` directly (synchronous; standalone)
- `mod.rs` → re-exports

Migration order (leaf-first):

**Step 1: `engine/error.rs` lands.** New crate-level `EngineError` enum (Q5 below). `lessons::*` and `storage::*` reference it. No callers migrate yet.

**Step 2: `engine/storage/lock.rs` born.** Move `engine/lessons/lock.rs` → `engine/storage/lock.rs` (pub(crate)). Re-export from `engine::lessons::lock` to keep imports working. `lessons/signals.rs` still calls it via the re-export.

**Step 3: `LocalFsStorage::put_if_version` + `get_with_version` implementations land.** Uses the new `storage::lock::with_lock_sync`. Day 14 stubs retire. The `put_if_version_returns_backend_error_in_phase_3b` test gets DELETED (it pinned the stub; the stub is gone).

**Step 4: `engine/lessons/loader.rs` migrates.** Adds new async API alongside old sync API:

```rust
// NEW async API (16b primary)
pub async fn get_by_id(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
) -> Result<Option<LoadedLesson>, EngineError>;

// OLD sync API (delegating wrapper, scheduled retirement)
#[deprecated(since = "0.0.1", note = "use lessons::get_by_id(&ctx, storage, id)")]
pub fn get_lesson_by_id(id: &str) -> anyhow::Result<Option<LoadedLesson>> {
    // Synthesize ctx + storage at call time; preserves old behavior for one cycle.
    let ctx = Context::single_user_local();
    let storage = LocalFsStorage::new(paths::loop_home()?);
    let runtime = tokio::runtime::Handle::try_current().ok();
    let result = match runtime {
        Some(h) => h.block_on(get_by_id(&ctx, &storage, id)),
        None => {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            rt.block_on(get_by_id(&ctx, &storage, id))
        }
    };
    result.map_err(anyhow::Error::from)
}
```

NOTE on `Handle::try_current()`: this is the **idiomatic Rust** pattern for "use the current runtime if there is one, else build a one-off." `futures::executor::block_on` (Day 16 pre-research Q6 sketch) is a different executor that doesn't drive tokio's reactor — would deadlock on the first `tokio::fs::read`. `tokio::runtime::Handle::block_on` is correct.

Caution: `Handle::block_on` from within a current tokio runtime can deadlock if all worker threads are blocked. For the single deprecated wrapper, this is acceptable — the wrapper retires in Step 8.

**Step 5: `engine/lessons/signals.rs` migrates.** New `record_sentiment_signal(&ctx, storage, id, polarity)` uses the CAS loop:

```rust
pub async fn record_sentiment_signal(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
    polarity: SignalPolarity,
) -> Result<LoadedLesson, EngineError> {
    let lesson_key = lesson_key_for_id(ctx, storage, id).await?
        .ok_or_else(|| EngineError::LessonNotFound { id: id.to_string() })?;

    const MAX_RETRIES: u32 = 5; // OQ-D16b-4
    for retry in 0..=MAX_RETRIES {
        let (bytes, version) = storage.get_with_version(&lesson_key).await?
            .ok_or_else(|| EngineError::LessonNotFound { id: id.to_string() })?;

        let mut lesson = parse_lesson_bytes(&lesson_key, &bytes)?;
        apply_sentiment_signal(&mut lesson, polarity)?;
        let new_bytes = serialize_lesson(&lesson)?;

        let ok = storage.put_if_version(&lesson_key, new_bytes, Some(&version)).await?;
        if ok {
            return Ok(lesson);
        }
        // CAS lost — re-read and retry.
        if retry == MAX_RETRIES {
            return Err(EngineError::CasContended {
                key: lesson_key.as_str().to_string(),
                retries: MAX_RETRIES,
            });
        }
        // No sleep — flock serializes; retry is hot but bounded.
    }
    unreachable!()
}

// Helper: locate the lesson's StorageKey (scanning the 5 status dirs).
async fn lesson_key_for_id(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
) -> Result<Option<StorageKey>, EngineError> {
    for status in paths::LESSON_STATUS_DIRS {
        let key = StorageKey::lesson(ctx, status, id);
        if storage.get(&key).await?.is_some() {
            return Ok(Some(key));
        }
    }
    Ok(None)
}
```

The OLD `record_sentiment_signal(id, polarity)` becomes a delegating wrapper for one cycle (same pattern as `get_lesson_by_id`).

**Step 6: `StorageBackedSignalWriter` lands.** New type in `engine::sentiment::signals` (next to `LoggingSignalWriter`). Implements `SignalWriter` by translating `SentimentSignal` → `lessons::record_sentiment_signal`. See Q4 below.

**Step 7: `engine::test_support::TestHarness` lands.** See Q6. Test modules incrementally adopt; 16b ships the harness + migrates `lessons/loader/tests` + `lessons/signals/tests`. `paths/tests` keeps `ENV_LOCK` until Day 17 (paths-itself migration).

**Step 8: Retire `with_temp_loop_home` + old delegating wrappers.** Only after all in-crate callers use the new async API. Verify with `grep -rn 'with_temp_loop_home\|get_lesson_by_id\b' src/` returns nothing in production code. The deprecated wrappers stay until Day 17.

### Wrapper retirement timing

Decision: **deprecated wrappers stay until Day 17.** Reasons:

1. `cli.rs` / `main.rs` may have indirect references via daemon startup paths. Day 16b focuses on engine-side work; binary-side migration is Day 17.
2. The `#[deprecated]` attribute generates compiler warnings — they're visible signals without breaking builds.
3. Day 17 audit can confirm no remaining production callers and delete safely.

This is **two cycles of wrapper overlap**, exceeding the Day 14 D8 ideal of "one cycle." Justification: Day 16b already touches 5 modules; pushing binary migration to Day 17 keeps the cycle audit-reviewable.

### Big-bang vs incremental — incremental wins

Rationale (from Day 16 pre-research Q6, confirmed):
- 7+ tests use `ENV_LOCK`. Big-bang means one bug surface = full revert.
- The orchestrator (16a) is the natural FIRST `StorageBackedSignalWriter` caller. Big-bang would conflate orchestrator wiring with persistence migration.
- Day 14 D8 + Day 15 audit (M3/M4 small drift confirms two-phase works) precedent.

### Commit cadence

| # | Commit | LOC est. | Verifies |
|---|---|---|---|
| 1 | `engine/error.rs` + EngineError + From impls | ~80 | New file compiles; existing tests pass (no callers yet) |
| 2 | Move `lessons/lock.rs` → `storage/lock.rs` with re-export | ~50 (move + 5 import-path changes) | All 224 tests pass |
| 3 | `put_if_version` + `get_with_version` impls + tests | ~250 (impls + helpers + 7 tests) | New tests pass; stub-pin test deleted |
| 4 | `lessons/loader.rs` async `get_by_id` + delegating wrapper | ~80 | All loader tests pass |
| 5 | `lessons/signals.rs` async `record_sentiment_signal` + CAS loop + wrapper | ~150 | All signals tests pass |
| 6 | `engine/test_support.rs` + 3-4 test rewrites | ~250 | Rewritten tests pass + old tests pass |
| 7 | `StorageBackedSignalWriter` + tests | ~200 | Orchestrator integration smoke green |
| 8 | Production wiring in `main.rs`: orchestrator constructed with StorageBackedSignalWriter | ~80 | Binary builds; smoke run; daemon doesn't crash on startup |

**Total: ~1140 LOC across 8 commits.** Each commit `cargo test --all-features` green at HEAD.

---

## Q4: `StorageBackedSignalWriter` shape

### Where it lives

**Recommend: `engine::sentiment::signals` (same module as `SignalWriter` trait and `LoggingSignalWriter`).** Reasons:

1. **Co-location with the trait.** The trait + all impls in one file is the established pattern (`classifier.rs` has `SentimentClassifier` + `MockSentimentClassifier`). Discoverability beats topical purity.
2. **No lessons-layer dependency from sentiment.** The writer is a thin adapter: it constructs a `StorageKey`, calls `lessons::record_sentiment_signal(&ctx, storage, id, polarity)`. The lessons module is the only place lesson-frontmatter knowledge lives. The writer is sentiment-side because it's the sentiment trait's impl.
3. **The orchestrator already injects `Arc<dyn SignalWriter>`.** Swap-in at construction; zero orchestrator changes.

Alternative considered: dedicated `engine::sentiment::storage_writer` module. Rejected — 50 LOC per file is below the discoverability threshold.

### Type sketch

```rust
// src/engine/sentiment/signals.rs (additions to existing file ~50 LOC)

use std::sync::Arc;
use crate::engine::error::EngineError;
use crate::engine::lessons::{self, SignalPolarity};
use crate::engine::storage::Storage;
use super::types::Polarity;

/// Production `SignalWriter`: persists signals into the lesson layer via
/// `Storage::put_if_version` CAS.
///
/// Replaces 16a's `LoggingSignalWriter` as the orchestrator's signal sink
/// in production. `LoggingSignalWriter` stays for local dev / verbose modes.
///
/// Polarity mapping (`Polarity::Neutral` is unreachable — orchestrator
/// abstains on neutral per `AbstainReason::Neutral`):
/// - `Polarity::Positive` → `SignalPolarity::Positive` (`sentiment_positive`)
/// - `Polarity::Negative` → `SignalPolarity::Negative` (`sentiment_negative`)
/// - `Polarity::Neutral` → no-op + `tracing::warn!` (defense in depth)
#[derive(Debug, Clone)]
pub struct StorageBackedSignalWriter {
    storage: Arc<dyn Storage>,
}

impl StorageBackedSignalWriter {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl SignalWriter for StorageBackedSignalWriter {
    async fn record(
        &self,
        ctx: &Context,
        signal: &SentimentSignal,
    ) -> Result<(), SignalWriteError> {
        let polarity = match signal.polarity {
            Polarity::Positive => SignalPolarity::Positive,
            Polarity::Negative => SignalPolarity::Negative,
            Polarity::Neutral => {
                tracing::warn!(
                    item = %signal.item_id,
                    "StorageBackedSignalWriter: orchestrator emitted Neutral; this should not happen (orchestrator should abstain on Neutral)"
                );
                return Ok(()); // defensive no-op
            }
        };

        lessons::record_sentiment_signal(
            ctx,
            self.storage.as_ref(),
            signal.item_id.as_str(),
            polarity,
        )
        .await
        .map_err(|engine_err| SignalWriteError::backend(engine_err))?;
        Ok(())
    }
}
```

### Bounded-retry policy (already inside `lessons::record_sentiment_signal`)

Per OQ-D16b-4: 5 retries inside `record_sentiment_signal`. No sleep — flock serializes across processes, so retries are hot (the next acquire blocks until the contending writer finishes). The 5-retry cap protects against pathological cross-process contention where a retry loop livelocks; single-user mode (one Rust daemon + one TS MCP) rarely contends.

**No jittered backoff.** Reasons:
- Two contending writers; not N. Backoff helps with N≥3.
- Flock acquire is already a kernel-level wait; sleeping during the retry adds latency without reducing contention.
- If we ever scale to N writers (SaaS mode), revisit then.

### Polarity / method translation (in detail)

`SentimentSignal` carries:
- `polarity: Polarity` — one of `Positive` / `Negative` / `Neutral`.
- `attribution_method: AttributionMethod` — `DirectMention` / `PronounResolved` / `Recency` / `Salience`.

Lessons layer signal sources are TWO strings: `sentiment_positive` / `sentiment_negative` (per `SignalPolarity::signal_source`). The lessons layer does NOT track attribution method; that's a sentiment-side concern (recorded in the orchestrator output's evidence trail, not the lesson itself).

**Decision: lose attribution_method at the lessons boundary.** The `StorageBackedSignalWriter` translates polarity only. Attribution method survives in:
- orchestrator output (`OrchestratorOutput.signals[i].attribution_method`)
- `LoggingSignalWriter` (writes `method=?` as a structured tracing field)

Day 17 lesson-rich-signals enhancement could add a `signal_evidence` array to the lesson frontmatter with `{ method, hazards, confidence }` per signal — out of 16b scope.

### Backward compat with 16a `MockSignalWriter`

`MockSignalWriter` stays unchanged. 16a's three integration tests still use it (orchestrator-output assertions, not persistence assertions). 16b adds NEW tests that use `StorageBackedSignalWriter` + `MemoryStorage` for end-to-end persistence assertions.

### Tests for `StorageBackedSignalWriter` (~6 tests)

1. `writer_persists_positive_signal_to_lesson` — seed a lesson via `MemoryStorage`; call `writer.record(&ctx, signal)`; verify `lessons::get_by_id` returns a lesson with `sentiment_positive` in `external_signal_sources`.
2. `writer_persists_negative_signal_to_lesson` — same with `Negative`.
3. `writer_returns_engine_err_when_lesson_missing` — call without seeding; assert `SignalWriteError::Backend(EngineError::LessonNotFound)`.
4. `writer_handles_cas_contention_via_retry` — use `MemoryStorage` and concurrent-writer simulation; assert eventual success within retry cap.
5. `writer_neutral_polarity_is_defensive_noop` — synthesize a Neutral signal; assert `Ok(())` returned and no lesson mutation.
6. `writer_clones_cheaply_via_arc_storage` — verify Arc<dyn Storage> shares state across writer clones (orchestrator may hold the writer in multiple spawned tasks).

---

## Q5: `EngineError` introduction

### Decision (confirmed from OQ-D16b-3)

**Crate-level: `engine::error::EngineError`.** New file `src/engine/error.rs`. Shared across `lessons`, `storage`, and (eventually) `orchestrator` / `lifecycle` / `pid`.

### Shape

```rust
// src/engine/error.rs (new file ~80 LOC)

use thiserror::Error;
use crate::engine::storage::StorageError;

/// Engine-layer errors. Concrete variants for cases that callers can
/// distinguish; `Other` for the long tail.
///
/// `#[non_exhaustive]`: new variants don't break consumers' `match`es.
///
/// Idiomatic-Rust patterns enforced (per feedback_rust_idiomatic_refactor):
/// - `#[from] StorageError` for ergonomic `?` propagation.
/// - `#[source]` on wrapped errors so std error iteration works.
/// - Concrete data on the variant (not `String`) where the caller will
///   actually inspect.
/// - NO `Other(anyhow::Error)` variant — that would leak anyhow back in.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EngineError {
    /// Lesson with the given id not found in any status directory.
    #[error("lesson not found: {id}")]
    LessonNotFound { id: String },

    /// Memory / persona / skill / team item not found (Day 17+ uses).
    #[error("loaded item not found: {kind}/{id}")]
    LoadedItemNotFound { kind: &'static str, id: String },

    /// Storage backend error (filesystem, S3, etc.).
    #[error("storage error")]
    Storage(#[from] StorageError),

    /// Compare-and-set retries exhausted. Caller should re-read and
    /// retry at the next opportunity, or surface as outage.
    #[error("CAS contended on {key} after {retries} retries")]
    CasContended { key: String, retries: u32 },

    /// YAML parse / serialize error (lesson frontmatter, config files).
    /// Wraps a `Box<dyn Error>` because the YAML stack uses multiple
    /// error types (`serde_yml::Error`, our `engine::yaml::reader::Error`,
    /// etc.). Acceptable boxing — small leaf, parse errors are rare.
    #[error("yaml error in {context}")]
    Yaml {
        context: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Invalid input from a caller (malformed id, empty required field).
    #[error("invalid input: {message}")]
    InvalidInput { message: String },

    /// File-system path / permission error not specific to a Storage backend
    /// (e.g. `paths::loop_home()` failing to resolve). Mostly used during
    /// startup; production storage paths go through `StorageError`.
    #[error("path resolution: {context}")]
    Path {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl EngineError {
    pub fn lesson_not_found(id: impl Into<String>) -> Self {
        Self::LessonNotFound { id: id.into() }
    }
    pub fn invalid_input(msg: impl Into<String>) -> Self {
        Self::InvalidInput { message: msg.into() }
    }
    pub fn yaml(
        context: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Yaml { context: context.into(), source: Box::new(source) }
    }
}
```

### Module adoption in 16b

| Module | 16b state | Day 17+ |
|---|---|---|
| `engine/lessons/loader.rs` | NEW async API uses `EngineError`; old sync API uses `anyhow` (delegating) | Retire wrapper |
| `engine/lessons/signals.rs` | Same as loader | Retire wrapper |
| `engine/storage/*.rs` | Already uses `StorageError`; `EngineError::Storage(_)` wraps via `#[from]` | No change |
| `engine/sentiment/*.rs` | Sentiment errors stay `ClassifierError`/`AttributionError`/etc. (orchestrator-local); `StorageBackedSignalWriter` calls into lessons and converts `EngineError` to `SignalWriteError` | Possibly fold |
| `engine/lifecycle.rs` | NO migration in 16b (stays `anyhow`) | Migrate |
| `engine/pid.rs` | NO migration | Migrate |
| `engine/buffer.rs` | NO migration | Migrate |
| `engine/yaml/*.rs` | NO migration; yaml errors are wrapped at the lesson boundary | Possibly migrate |

### Conversion impls

- `impl From<StorageError> for EngineError` — derived via `#[from]` on `Storage(_)`.
- `impl From<EngineError> for anyhow::Error` — automatic via `anyhow`'s blanket `impl<E: std::error::Error> From<E> for anyhow::Error`. Lets delegating wrappers convert with `?`.
- NO `impl From<anyhow::Error> for EngineError` — would invite anyhow leakage. The new API never receives anyhow.
- NO `impl From<EngineError> for SignalWriteError` — `SignalWriteError::Backend(Box<dyn Error>)` already accepts EngineError via `SignalWriteError::backend(engine_err)`.

### Tests (~6 EngineError tests)

1. `engine_error_lesson_not_found_displays_correctly` — pin Display message format.
2. `engine_error_storage_from_conversion` — verify `StorageError::NotFound { key }` → `EngineError::Storage(_)` via `?`.
3. `engine_error_to_anyhow_preserves_source_chain` — convert to `anyhow::Error`; verify `e.source()` traverses to the inner StorageError.
4. `engine_error_cas_contended_carries_retries` — pin retry count in message.
5. `engine_error_yaml_wraps_source` — wrap a `serde_yml::Error`; verify Display + source chain.
6. `engine_error_invalid_input_is_constructible_from_str` — pin builder ergonomics.

---

## Q6: `TestHarness` design

### Decision (per OQ-D16b-5, refined)

**`engine::test_support::TestHarness` — crate-public, behind `test-fixtures` feature.** Integration tests under `tests/*.rs` see it via the self-reference dev-dep that Day 15 M3 added.

### Shape

```rust
// src/engine/test_support.rs (new file ~150 LOC, behind feature)

#![cfg(any(test, feature = "test-fixtures"))]

use std::sync::Arc;
use tempfile::TempDir;

use crate::engine::context::Context;
use crate::engine::storage::{LocalFsStorage, MemoryStorage, Storage};

/// Test scaffold: a `Context` + a `Storage` + (optionally) a `TempDir`.
///
/// Replaces the pre-Day-14 `with_temp_loop_home` + `ENV_LOCK` pattern.
/// Tests run in parallel — no global env mutation.
///
/// Two flavors:
/// - `TestHarness::in_memory()` — `MemoryStorage`; no filesystem.
/// - `TestHarness::on_disk()` — `LocalFsStorage` backed by a fresh `TempDir`.
///   The `TempDir` is held inside the harness; dropping the harness cleans
///   up the temp directory.
///
/// `Storage` is wrapped in `Arc<dyn Storage>` to match production wiring.
/// Tests can clone the Arc to share across spawned tasks.
pub struct TestHarness {
    pub ctx: Context,
    pub storage: Arc<dyn Storage>,
    /// `Some` for `on_disk()`, `None` for `in_memory()`.
    /// Held to keep the TempDir alive for the harness's lifetime.
    _tempdir: Option<TempDir>,
}

impl TestHarness {
    /// In-memory harness — fastest, no filesystem. Use for pure-logic
    /// tests where the storage backend's filesystem semantics don't matter.
    pub fn in_memory() -> Self {
        Self {
            ctx: Context::single_user_local(),
            storage: Arc::new(MemoryStorage::default()),
            _tempdir: None,
        }
    }

    /// Filesystem-backed harness on a fresh `TempDir`. Use for tests that
    /// exercise `LocalFsStorage`-specific behavior (atomic rename, flock,
    /// sidecar files).
    pub fn on_disk() -> Self {
        let tempdir = TempDir::new().expect("tempdir for TestHarness");
        let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(tempdir.path()));
        Self {
            ctx: Context::single_user_local(),
            storage,
            _tempdir: Some(tempdir),
        }
    }

    /// Variant: multi-tenant context (for hreflang-style routing tests).
    pub fn in_memory_for_tenant(tenant: &str, user: &str) -> Self {
        Self {
            ctx: Context::builder()
                .tenant_id(tenant)
                .user_id(user)
                .session_id("test-session")
                .build(),
            storage: Arc::new(MemoryStorage::default()),
            _tempdir: None,
        }
    }

    /// Storage-aware lesson seeding helper. Writes a minimum-frontmatter
    /// lesson under the given status using the harness's storage backend.
    pub async fn seed_lesson(&self, status: &str, id: &str) -> crate::engine::storage::StorageKey {
        use crate::engine::storage::StorageKey;
        use crate::engine::yaml::{combine_frontmatter, writer::serialize_lesson_frontmatter, LessonFrontmatter, LessonStatus};
        let fm = minimum_frontmatter(id);
        let yaml = serialize_lesson_frontmatter(&fm);
        let contents = combine_frontmatter(&yaml, "test body\n");
        let key = StorageKey::lesson(&self.ctx, status, id);
        self.storage.put(&key, contents.into_bytes().into()).await
            .expect("seed_lesson put failed");
        key
    }
}

fn minimum_frontmatter(id: &str) -> LessonFrontmatter {
    LessonFrontmatter {
        id: id.into(),
        description: "test lesson".into(),
        status: LessonStatus::Active,
        created_at: "2026-05-13T00:00:00.000Z".into(),
        // ... (rest of fields = sensible defaults, copied from current helpers)
    }
}
```

### Design decisions

1. **Two constructors, one struct.** Avoids type explosion (no `MemoryHarness` vs `DiskHarness`). The optional TempDir field carries the lifetime.
2. **`Arc<dyn Storage>` (not generic).** Storage is already designed object-safe; matching production wiring keeps test code production-shaped. Cost: dynamic dispatch — negligible at test scale.
3. **`_tempdir` field with leading underscore.** Convention: held-for-RAII, not read. Drop order matters — TempDir drops AFTER `Arc<dyn Storage>` since fields drop in declaration order; but Storage doesn't reference TempDir contents at drop time. Safe.
4. **`seed_lesson` helper.** Replaces the per-test `write_minimum_lesson` duplicates in `loader.rs` and `signals.rs`. Now a single seam.
5. **No `ctx_mut` accessor.** Context is immutable post-construction; multi-tenant constructors handle the divergent case.
6. **`#![cfg(any(test, feature = "test-fixtures"))]`** — same gating as `MockSentimentClassifier`, `MockSignalWriter`. Consistent.

### Tests that get rewritten

7 tests today use `with_temp_loop_home`:
- `loader::tests::returns_none_when_lesson_missing`
- `loader::tests::returns_none_for_invalid_id`
- `loader::tests::finds_lesson_in_active_status`
- `loader::tests::finds_lesson_in_each_status_dir`
- `loader::tests::lesson_file_path_uses_status_dir`
- `loader::tests::lesson_file_path_rejects_invalid_id`
- 8 tests in `signals::tests` (the writer suite)

Total: ~15 tests. They migrate as their parent module migrates (Step 4 + 5 from Q3).

### Example test rewrite (signals.rs)

BEFORE (current):
```rust
#[test]
fn adds_sentiment_positive_to_empty_sources() {
    with_temp_loop_home(|tmp| {
        write_lesson(tmp, "active", "les-emptysig", vec![]);
        let updated = record_sentiment_signal("les-emptysig", SignalPolarity::Positive)?;
        assert_eq!(updated.frontmatter.external_signal_sources, vec!["sentiment_positive"]);
        Ok(())
    });
}
```

AFTER (16b):
```rust
#[tokio::test]
async fn adds_sentiment_positive_to_empty_sources() {
    let h = TestHarness::on_disk();
    h.seed_lesson("active", "les-emptysig").await;

    let updated = lessons::record_sentiment_signal(
        &h.ctx,
        h.storage.as_ref(),
        "les-emptysig",
        SignalPolarity::Positive,
    ).await.unwrap();

    assert_eq!(
        updated.frontmatter.external_signal_sources,
        vec!["sentiment_positive"]
    );
}
```

Three improvements:
- `#[tokio::test]` async-aware.
- No global ENV mutation → parallel-safe.
- No setup boilerplate (`with_temp_loop_home` + `write_lesson`).

### Example test rewrite (loader.rs in-memory)

```rust
#[tokio::test]
async fn returns_none_when_lesson_missing() {
    let h = TestHarness::in_memory();
    let result = lessons::get_by_id(&h.ctx, h.storage.as_ref(), "les-missing").await.unwrap();
    assert!(result.is_none());
}
```

### ENV_LOCK retirement

After Step 7 (Q3):
- `lessons/loader.rs::tests` has no `ENV_LOCK` reference.
- `lessons/signals.rs::tests` has no `ENV_LOCK` reference.
- `paths.rs::tests` STILL uses `ENV_LOCK` for the 2 `loop_home`-direct tests. These stay; the env-var resolution is what `loop_home()` does.

**Net:** `ENV_LOCK` shrinks from 7 callers to 2. Survives until `paths::loop_home()` migration in Day 17 or later.

---

## Q7: Orchestrator's `LoggingSignalWriter` → swap for `StorageBackedSignalWriter`

### Where wiring happens

**`src/main.rs` (daemon entrypoint).** Day 16a built the Orchestrator type but didn't construct one. Day 16b's Step 8 is the FIRST production construction.

Sketch:
```rust
// src/main.rs (16b additions)

use std::sync::Arc;
use loop_daemon::engine::context::Context;
use loop_daemon::engine::storage::LocalFsStorage;
use loop_daemon::engine::sentiment::orchestrator::{Orchestrator, OrchestratorConfig};
use loop_daemon::engine::sentiment::signals::StorageBackedSignalWriter;
// + classifier construction (Day 15 wired)

fn build_orchestrator(ctx: &Context) -> anyhow::Result<Orchestrator> {
    let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(paths::loop_home()?));
    let writer = Arc::new(StorageBackedSignalWriter::new(storage.clone()));
    let classifier = build_classifier(ctx)?; // Day 15 anthropic-haiku-adapter (deferred to Day 17)
    Ok(Orchestrator::new(classifier, writer, OrchestratorConfig::default()))
}
```

Caveat: the **classifier wiring is deferred** (Day 15's Haiku adapter pre-research exists but no impl). For 16b, wire the orchestrator with `MockSentimentClassifier` if no real classifier exists yet — but only in `--features dev-mode` or behind a CLI flag (TBD whether 16b ships this). Strictly speaking, 16b can skip the binary-side wiring entirely and ship the writer + tests only; production daemon stays non-functional until Day 17 lands the classifier.

**Recommend: 16b ships the writer + comprehensive tests, but main.rs wiring stays a STUB until Day 17 (when the classifier lands).** This keeps cycle scope reviewable and avoids shipping a half-functional daemon binary.

### Config / environment switch

For verbose / debug mode, Day 17+ could add an `OrchestratorConfig::writer_mode` enum:
```rust
pub enum WriterMode { Storage, LoggingOnly, Both }
```
where `Both` chains the writers via a `ChainedSignalWriter` (writes to both — useful for testing). Out of 16b scope; mentioned for forward-feed.

### Backward compat with 16a tests

`MockSignalWriter` stays unchanged. The 3 existing orchestrator integration tests in `orchestrator/mod.rs` continue using `MockSignalWriter`. NEW 16b tests (in a new `tests/storage_backed_signal_writer.rs` integration file) use `StorageBackedSignalWriter` + `MemoryStorage` end-to-end.

### Integration smoke test (16b ships)

```rust
// tests/storage_backed_writer_smoke.rs (~80 LOC)

#[tokio::test]
async fn orchestrator_emits_signal_persists_to_lesson_via_storage_backed_writer() {
    use loop_daemon::engine::test_support::TestHarness;
    use loop_daemon::engine::sentiment::orchestrator::Orchestrator;
    use loop_daemon::engine::sentiment::signals::StorageBackedSignalWriter;
    use loop_daemon::engine::sentiment::classifier::MockSentimentClassifier;
    // ... etc.

    let h = TestHarness::on_disk();
    h.seed_lesson("active", "les-target01").await;

    let classifier = Arc::new(
        MockSentimentClassifier::default()
            .with_response(canned_positive_classification("les-target01")),
    );
    let writer = Arc::new(StorageBackedSignalWriter::new(h.storage.clone()));
    let orch = Orchestrator::new(classifier, writer, OrchestratorConfig::default());

    orch.update_manifest(&h.ctx, vec![manifest_item_for("les-target01")]).await;
    orch.push_assistant_turn(&h.ctx, turn_referencing("les-target01")).await;

    let event = EngineEvent::UserTurn { /* ...positive text... */ };
    let output = orch.process_event(&h.ctx, &event).await;
    assert_eq!(output.signals.len(), 1);

    // Verify persistence.
    let lesson = loop_daemon::engine::lessons::get_by_id(
        &h.ctx, h.storage.as_ref(), "les-target01"
    ).await.unwrap().unwrap();
    assert!(lesson.frontmatter.external_signal_sources.contains(&"sentiment_positive".to_string()));
}
```

This closes Day 16a M3 (no positive-path integration test).

---

## Q8: Day 16b TS-with-Rust-syntax smells (S31–S43)

Continuing the cycle numbering: Day 14 = S1-S17, Day 15 = S1-S17 (separate counting; same set restated), Day 16a = S18-S30, Day 16b = S31-S43.

### S31. `fd_lock::RwLock` held across `.await`

WRONG:
```rust
async fn put_if_version(&self, ...) {
    let _guard = lock.write()?;  // sync fd_lock
    self.write_async(...).await?;  // suspends with flock held — OS doesn't release!
}
```
RIGHT: `tokio::task::spawn_blocking(move || { let _g = lock.write()?; sync_write(...) }).await?`.
Rationale: `fd_lock` is sync; tokio suspension doesn't release the OS-level flock. Lock leaks to any other task that picks up the worker.

### S32. `Version` encoded as `String` or ISO timestamp

WRONG: `Version(String)` containing `"2026-05-13T12:34:56.789Z"` (mtime as ISO).
RIGHT: `Version(Box<[u8]>)` with opaque encoding the caller never inspects (carryover from S30; pin again because 16b is where the encoding is actually decided).

### S33. Split read of `(bytes, version)` in `get_with_version`

WRONG:
```rust
let bytes = tokio::fs::read(&path).await?;
let metadata = tokio::fs::metadata(&path).await?;  // raced!
```
RIGHT: hold the sidecar flock for both ops; do them sync inside `spawn_blocking` (carryover from S28).

### S34. `EngineError::Other(anyhow::Error)`

WRONG: `EngineError::Other(#[from] anyhow::Error)` — leaks anyhow back into typed errors.
RIGHT: a finite set of variants per Q5 above; anyhow stays only in the deprecated delegating wrappers.

### S35. `tokio::task::spawn_blocking` for I/O that's already async

WRONG: `spawn_blocking(|| tokio::fs::read(...))` — `tokio::fs::read` IS the async version; spawning a blocking task to await it is pure overhead.
RIGHT: use `tokio::fs::read(...).await` directly when async is available; reserve `spawn_blocking` for genuinely synchronous APIs (fd_lock, std::fs::rename in a CAS critical section, libc syscalls).

### S36. `anyhow::Result` in new async APIs

WRONG: `pub async fn get_by_id(ctx, storage, id) -> anyhow::Result<...>`
RIGHT: `pub async fn get_by_id(ctx, storage, id) -> Result<..., EngineError>`.
Rationale: the migration's entire point is to retire anyhow from engine surfaces. Spotted in code review by `grep -rn 'anyhow::Result\|anyhow::Error' src/engine/lessons/` after Step 5; should return only delegating-wrapper hits.

### S37. `Result<T, EngineError>::map_err(|e| anyhow!(e))` in non-wrapper code

WRONG: any in-engine call that converts EngineError back to anyhow.
RIGHT: propagate EngineError via `?`. Only the deprecated wrappers convert.

### S38. `Arc<Box<dyn Storage>>` (double indirection)

WRONG: `storage: Arc<Box<dyn Storage>>`
RIGHT: `storage: Arc<dyn Storage>`.
Rationale: `Arc<dyn Trait>` is the idiom; `Arc<Box<dyn Trait>>` adds a useless heap hop. Easy mistake when copying patterns from C++/Java textbooks.

### S39. Test harness with side-effects-in-`Drop`

WRONG:
```rust
impl Drop for TestHarness {
    fn drop(&mut self) {
        block_on(self.storage.delete_all())  // panics in async runtime / can't await in drop
    }
}
```
RIGHT: `TempDir` handles cleanup via its own Drop; harness has no custom Drop. MemoryStorage drops naturally.

### S40. Eager `String` allocation in error path

WRONG:
```rust
return Err(EngineError::lesson_not_found(format!("{id}")));  // format! always allocates
```
RIGHT: `EngineError::lesson_not_found(id.to_string())` — `format!("{id}")` is `id.to_string()` with extra parsing. Direct call is clearer and identical performance. For hotter paths, consider `Cow<'static, str>` — out of 16b scope.

### S41. CAS-loop without retry bound

WRONG:
```rust
loop {
    let (b, v) = storage.get_with_version(&key).await?;
    if storage.put_if_version(&key, modified, Some(&v)).await? { return Ok(...); }
}
```
RIGHT: bounded retry (5x per OQ-D16b-4) with `EngineError::CasContended` on exhaustion.
Rationale: unbounded retry is a livelock vector under pathological cross-process contention. Bounded retry surfaces the contention so an operator can investigate.

### S42. `tokio::sync::Mutex<TempDir>` to share between async tasks

WRONG: wrapping the test harness's TempDir in any kind of lock to share across tasks.
RIGHT: `TempDir` is `Send`. Share via `Arc<TempDir>` or — better — pass the path by clone and let the original handle keep ownership.

### S43. `lessons::record_sentiment_signal` rebuilds `StorageKey` from scratch instead of accepting one

WRONG (overly-coupled signature):
```rust
pub async fn record_sentiment_signal(
    ctx: &Context, storage: &dyn Storage, key: &StorageKey, polarity: SignalPolarity
) -> ...
```
RIGHT:
```rust
pub async fn record_sentiment_signal(
    ctx: &Context, storage: &dyn Storage, id: &str, polarity: SignalPolarity
) -> ...
```
Rationale: the caller (orchestrator → writer) has the `id`, not the `StorageKey`. Building the key is a lessons-internal concern (it scans status dirs to find which key resolves). Exposing `StorageKey` at the API would force every caller to know the status-dir-scan algorithm. Keep `StorageKey` as an internal implementation detail.

---

## Hard constraints check

| Constraint | Status |
|---|---|
| No AGPL/GPL/SSPL deps | ✅ No new deps. `fd-lock = 4` (MIT/Apache, already direct), `tempfile = 3` (MIT/Apache, already dev-dep), `thiserror = 2` (MIT/Apache, already direct) all fine. |
| File-size ≤500 LOC per file | All 16b files projected under 500 LOC. Largest: `lessons/signals.rs` projected ~250 LOC. `engine/test_support.rs` ~150 LOC. `engine/error.rs` ~80 LOC. `storage/filesystem.rs` grows from 343 to ~500 LOC — at the limit; if it tips over, split into `filesystem/{mod, cas, helpers}.rs`. |
| `#[non_exhaustive]` on growth-prone public types | `EngineError` ✅. `TestHarness` (struct) ⚠️ — fields are pub; recommend keeping fields pub and the struct non-`#[non_exhaustive]` since callers WANT field access (`h.ctx`, `h.storage`); fields stable. |
| Day 14 Context/Storage mandatory foundations | ✅ — all new APIs take `&Context, &dyn Storage`. |
| Day 13 sidecar-flock 127-test correctness preserved | ✅ — lift, not rewrite. Re-export from old path for one cycle. |

---

## Day 16a learnings forward-fed

| 16a learning | 16b application |
|---|---|
| L1 — orchestrator.rs split | Already done in 16a audit; nothing for 16b |
| L2 — `last_assistant_turn_at` dormant | `update_manifest` + `push_assistant_turn` already added in 16a audit (C2 fix). 16b smoke test exercises them. |
| L3 — `MemoryStorage` not exercised by orchestrator | 16b's `StorageBackedSignalWriter` is the first real consumer |
| L4 — `JsonlWatcherSource` end-to-end smoke missing | Still deferred to Day 17 |
| L5 — `EngineEvent::SessionStarted.path` host-leaky | No 16b change; flag persists |
| L6 — Day 17 solicitor needs `OrchestratorOutput` | Locked; Day 17 starts from this shape |
| L7 — Cargo.lock churn | No new deps in 16b; lock stays |
| L8 — `Orchestrator` not `Default` | No 16b change |

| 16a audit finding | 16b plan |
|---|---|
| C1 (SessionRecycled race) | Fixed in 16a; carryover discipline: critical sections never `.await` |
| C2 (handle_user_interrupt dead path) | Fixed in 16a via `push_assistant_turn`; 16b smoke test exercises emit path |
| M1 (orchestrator.rs over LOC) | Fixed; 16b watches new files for the same trap |
| M2 (no smoke test) | 16b ships `storage_backed_writer_smoke.rs` |
| M3 (no positive-path integration test) | 16b's smoke covers; closes M3 |
| M4 (loaded_items empty) | `update_manifest` already added; smoke test populates it |
| M5/M6/M7 (lint, pub visibility, output shape divergence) | Fixed in 16a |

---

## Open questions for 16b learn-phase (final decisions before build)

### OQ-D16b-A. `EngineError::Yaml` shape — `Box<dyn Error>` or named variants?

Pre-research recommends `Box<dyn Error>` for the YAML stack (multiple parse libs in play: `serde_yml::Error`, our hand-rolled `engine::yaml::reader::Error`). **Recommend: Box for now.** If a future cycle adds typed YAML errors with grep-able variants, revisit.

### OQ-D16b-B. CAS-retry sleep policy

Pre-research recommends NO sleep (flock already serializes). **Recommend confirm: no sleep.** Revisit only if SaaS-mode shows N≥3 contention.

### OQ-D16b-C. `lessons::lesson_file_path` — keep or retire?

Currently `pub fn lesson_file_path(status, id) -> Result<PathBuf>`. Only used by tests. Post-migration, tests don't need it (they use `StorageKey::lesson`). **Recommend: mark `#[doc(hidden)] pub(crate)` in 16b; delete in Day 17.**

### OQ-D16b-D. `StorageBackedSignalWriter` Debug impl shows Storage type?

Storage is `Arc<dyn Storage>`; its Debug yields `Box<dyn Debug>`. **Recommend: derive Debug; accept that production logs show "StorageBackedSignalWriter { storage: <dyn Storage> }".** Adequate for log grep.

### OQ-D16b-E. Should `EngineError` impl `Clone`?

Today: no (StorageError is not Clone; std::io::Error is not Clone). Would help test fixtures that want to inject the same error multiple times. **Recommend: no Clone for 16b.** Mock-error injection uses the one-shot pattern Day 15 m4 established.

### OQ-D16b-F. `TestHarness` returns from `seed_lesson`

Current sketch returns `StorageKey`. Alternative: return `LoadedLesson`. **Recommend: return `StorageKey`** — most tests don't need the loaded form, and the StorageKey is what other Storage ops need.

### OQ-D16b-G. Should `record_sentiment_signal` take `&Context` or move ownership?

`&Context` per the established pattern (`get`, `put`, `list` all take `&Context` indirectly via `StorageKey::lesson(&ctx, ...)`). **Recommend: `&Context`** — never move.

### OQ-D16b-H. Should we add a `Storage::compare_and_swap` shorthand method?

Combines `get_with_version` + `put_if_version` into one CAS-loop helper inside the trait. **Recommend: NO** — that helper lives at the lessons layer (it knows the bound-retry policy and the lesson-specific bytes-modify function). Adding to Storage forces every backend to implement a generic CAS-loop with a closure, which is a layering inversion.

---

## Scope concerns + new deferrals

### Scope concerns

1. **Two-phase migration touches 5 modules.** Step 1-8 (Q3) span lock, filesystem, loader, signals, sentiment/signals, test_support. Audit surface is large. Mitigate by committing each step separately so audit can review one at a time.
2. **`Handle::block_on` in deprecated wrappers may deadlock.** Acceptable transitional risk per Q3; deprecated wrappers retire in Day 17.
3. **`storage/filesystem.rs` LOC.** Projected ~500 LOC (current 343 + ~150 new). Right at the limit. If audit shows >500 prod LOC, split: `filesystem/mod.rs` keeps trait impl, `filesystem/cas.rs` houses `put_if_version` + `get_with_version` + helpers, `filesystem/io.rs` houses `atomic_write_sync` + `read_version_sync`.
4. **`StorageBackedSignalWriter` translation lossy.** AttributionMethod + hazards + confidence don't survive into the lesson YAML. 16b accepts this; Day 17+ may add a richer signal-evidence array.
5. **Integration test for orchestrator + writer requires `update_manifest` + `push_assistant_turn` to actually work end-to-end.** 16a's audit C2 fix added these; verify they're sound before 16b's smoke test depends on them.
6. **Production daemon stays non-functional.** Without a classifier (Haiku adapter deferred), the orchestrator-with-StorageBackedSignalWriter wiring in main.rs is incomplete. Either wire `MockSentimentClassifier` behind a feature flag for 16b OR accept that the daemon binary doesn't emit signals until Day 17. **Recommend: accept** — 16b focuses on persistence layer correctness.

### New deferrals

- **D-D16b-1.** Manifest assembly (lessons list → `Orchestrator::update_manifest`) — Day 17.
- **D-D16b-2.** Anthropic Haiku classifier adapter — Day 17+ (has its own pre-research doc).
- **D-D16b-3.** Lesson signal evidence array (rich per-signal metadata) — Day 18+.
- **D-D16b-4.** `paths::loop_home()` migration off `ENV_LOCK` (last 2 callers) — Day 17.
- **D-D16b-5.** `lifecycle.rs` + `pid.rs` + `buffer.rs` migration to EngineError — Day 17.
- **D-D16b-6.** `JsonlWatcherSource` end-to-end smoke — Day 17.
- **D-D16b-7.** `cargo-public-api` gating (currently opt-in, planned gating Day 17 per Day 14 OQ4).

---

## Sources / crate versions cited

- `fd-lock` 4.0.4 (Cargo.lock:267) — MIT/Apache, already direct dep.
- `tempfile` 3.x — MIT/Apache, already dev-dep.
- `thiserror` 2.x — MIT/Apache, already direct dep.
- `tokio` 1.x — MIT, already direct dep (`spawn_blocking`, `Handle::block_on`).
- `bytes` 1.x — MIT, already direct dep.
- `async-trait` 0.1.x — MIT/Apache, already direct dep.
- `dashmap` 6.x — MIT, already direct dep (16a).
- `anyhow` 1.x — MIT/Apache, already direct dep (used only in deprecated wrappers post-16b).

No new dependencies needed for 16b.

---

## Related

- `docs/research/day-16-pre-research.md` Q5 (put_if_version), Q6 (lessons migration), Q8 (S28-S30 smells) — 16b's predecessors.
- `docs/research/day-16a-post-research.md` L1-L8 + OQ-D16b-6 (orchestrator split, closed), OQ-D16b-7 (StorageBackedSignalWriter integration — answered in Q4).
- `docs/research/day-16a-audit-report.md` — C1/C2 fixes confirmed; M2/M3/M4 close via 16b smoke test.
- `docs/research/day-14-learn-notes.md` D7 (TestHarness), D8 (two-phase migration).
- `docs/research/day-14-audit-report.md` — m7 (Box<dyn Error> source pattern), m3 (ENV_LOCK pub(crate)) precedents.
- `loop-archive-2026-05-13/core-ts/src/lib/file-mutex.ts` — TS reference (in-process Mutex only; verifies Rust sidecar-flock is strictly stronger).
- `loop-archive-2026-05-13/core-ts/src/lessons/signals.ts` — TS reference for the `withFileLock` pattern (the *what*, not the *how*).
- `src/engine/lessons/{loader,signals,lock}.rs` — current Day 11/12 build outputs (16b migration targets).
- `src/engine/storage/{mod,filesystem,memory,version,key,error}.rs` — Day 14 build outputs (16b implements stubs, lifts lock).
- `src/engine/sentiment/{signals,orchestrator}.rs` — Day 16a outputs (16b extends signals with StorageBackedSignalWriter).
- `feedback_rust_idiomatic_refactor.md` — the hard rule that drove this doc's depth.

---

## TL;DR (3 paragraphs)

**`put_if_version` implementation.** Lift the 127-test-validated `engine::lessons::lock::with_lock` sidecar-flock pattern wholesale: move the file to `engine::storage::lock` (crate-private), re-export from `engine::lessons::lock` for one cycle for backward compat, and wire `LocalFsStorage::put_if_version` + `get_with_version` to use it. CAS path runs entirely inside `tokio::task::spawn_blocking` because `fd_lock` is sync and the OS doesn't release a flock when a tokio future suspends (S31). `Version` encodes as opaque 24 bytes = `mtime_ns (i128, 16) + len (u64, 8)` per Day 16 Q5 — APFS-ms-coarse but sufficient when paired with `len`. `get_with_version` holds the same sidecar lock during read+stat to keep the `(bytes, version)` pair coherent (S33). TS-cross-process compat is preserved-or-strengthened because TS uses in-process mutex only (verified in `core-ts/src/lib/file-mutex.ts`), not flock — Rust's flock is strictly stronger.

**Migration order.** Eight commits, leaf-first, each individually green: (1) `EngineError` lands as a standalone new file with no callers; (2) `lessons/lock.rs` moves to `storage/lock.rs` with a re-export keeping old imports working; (3) `put_if_version`/`get_with_version` impls + 7 regression tests land, retiring the Day 14 stub-pin test; (4) `lessons/loader.rs` grows a new async `get_by_id(&ctx, storage, id) -> Result<_, EngineError>` API alongside the deprecated sync wrapper that uses `tokio::runtime::Handle::block_on` (NOT `futures::executor::block_on`, which would deadlock on `tokio::fs`); (5) `lessons/signals.rs` grows the async `record_sentiment_signal` with a 5-retry bounded CAS loop (OQ-D16b-4); (6) `engine/test_support::TestHarness { ctx, storage, _tempdir }` lands with `in_memory()` / `on_disk()` constructors, and 15 ENV_LOCK-using tests get rewritten; (7) `StorageBackedSignalWriter` lands in `sentiment/signals.rs` next to `LoggingSignalWriter`, translating `Polarity → SignalPolarity` and calling `lessons::record_sentiment_signal`; (8) main.rs gets a STUB orchestrator-with-StorageBackedSignalWriter wiring that doesn't fully activate until Day 17's classifier lands. The deprecated `get_lesson_by_id` / `record_sentiment_signal` (sync) wrappers stay until Day 17 — two-cycle overlap is a knowing exception to the Day 14 D8 ideal of one cycle, justified by 16b's already-large audit surface.

**Scope concerns + new deferrals.** 16b touches 5 modules across 8 commits — large but committed step-by-step so audit reviews one slice at a time. `storage/filesystem.rs` grows to ~500 LOC (the hard limit); if audit shows it tipped over, split into `filesystem/{mod,cas,io}.rs`. The production daemon binary in main.rs stays NON-functional after 16b because the Anthropic Haiku classifier is deferred to Day 17 — 16b focuses on persistence-layer correctness only. New deferrals: D-D16b-1 manifest assembly, D-D16b-2 Haiku adapter, D-D16b-3 lesson signal evidence array, D-D16b-4 last 2 `ENV_LOCK` callers in `paths::tests`, D-D16b-5 lifecycle/pid/buffer EngineError migration, D-D16b-6 JsonlWatcherSource end-to-end smoke, D-D16b-7 `cargo-public-api` gating. All routed to Day 17 audit + Day 17 build.
