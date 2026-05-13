# Day 16b Learn Notes — Locked Decisions for Build Phase

**Date:** 2026-05-13
**Cycle phase:** Learn (workflow cycle phase 2)
**Cycle:** Day 16b — LocalFsStorage CAS impls + lessons migration + StorageBackedSignalWriter + EngineError + TestHarness
**Source pre-research:** `docs/research/day-16b-pre-research.md` (1235 lines, 8 Q sections)

Locked decisions distilled from pre-research. All 8 open-question recommendations accepted.

---

## Locked decisions

### D1. `LocalFsStorage::put_if_version` / `get_with_version` strategy
- **Lift `engine::lessons::lock::with_lock`** (127-test-validated sidecar-flock) into a new `engine::storage::lock` module. Re-export from `engine::lessons::lock` for one cycle.
- CAS path runs inside `tokio::task::spawn_blocking` — `fd_lock` is sync; flock doesn't release on tokio suspend (S31 audit-smell prevention).
- Hold sidecar lock during BOTH read+stat in `get_with_version` (S33).

### D2. `Version` encoding
- 24 bytes opaque: `mtime_ns (i128 LE, 16 bytes) || len (u64 LE, 8 bytes)`.
- APFS mtime is ms-coarse — pairing with `len` makes same-ms collisions extremely unlikely for our use case (single-writer lesson files).

### D3. TS cross-process compatibility (verified, not assumed)
- TS uses **in-process `async-mutex` only**, NOT flock (`loop-archive-2026-05-13/core-ts/src/lib/file-mutex.ts` lines 8-11 explicitly reject `proper-lockfile`).
- Consequence: Rust sidecar-flock is **strictly stronger**; 16b ships it without degrading cross-process compat.

### D4. `tokio::runtime::Handle::block_on`, NOT `futures::executor::block_on`
- Deprecated sync wrappers use `Handle::block_on` to call new async APIs.
- `futures::executor::block_on` would deadlock on `tokio::fs::*` (different executor).

### D5. `EngineError` is crate-level (per OQ-D16b-3)
- New file `src/engine/error.rs`
- `pub enum EngineError` (`#[non_exhaustive]`, derives `thiserror::Error`, `Debug`)
- Variants: `LessonNotFound { id }`, `Storage(StorageError)`, `Yaml(Box<dyn Error + Send + Sync>)`, `Parse(String)`, `CasContended { key, retries }`, `Io(io::Error)`, `Other(Box<dyn Error + Send + Sync>)` — for genuinely uncategorized engine errors.
- `From<StorageError> for EngineError` + `From<io::Error> for EngineError` conversions.
- NO `Clone` impl (OQ-D16b-E).

### D6. `TestHarness` in `engine::test_support` (behind `test-fixtures` feature, per OQ-D16b-5)
- New file `src/engine/test_support.rs`
- `pub struct TestHarness { pub ctx: Context, pub storage: Arc<dyn Storage>, _tempdir: Option<TempDir> }`
- Constructors: `TestHarness::in_memory()` (`Arc<MemoryStorage>`), `TestHarness::on_disk() -> (Self, TempDir)` (RAII via the held TempDir).
- `seed_lesson(&self, status, id, body) -> StorageKey` helper (returns StorageKey per OQ-D16b-F).

### D7. `StorageBackedSignalWriter` lives in `engine::sentiment::signals` next to existing writers
- New struct alongside `LoggingSignalWriter` + `MockSignalWriter`.
- Holds `Arc<dyn Storage>` + `Arc<???>` no — wait: actually it needs a way to write sentiment signals to lessons. It uses the migrated `lessons::record_sentiment_signal(&ctx, storage, ...)` API.
- `Debug` derived (OQ-D16b-D).
- Bounded CAS retry: 5 attempts inside `lessons::record_sentiment_signal` (per pre-research Q4 — the retry policy lives in the lessons layer, not the writer or Storage).
- NO sleep between retries (OQ-D16b-B — flock already serializes).

### D8. Lessons migration: leaf-first, 8 commits, deprecated wrappers stay until Day 17 (knowing exception to Day 14 D8 one-cycle ideal)
Commit cadence:
1. **EngineError** lands as standalone — no callers yet
2. **lessons/lock.rs → storage/lock.rs** move with re-export shim
3. **put_if_version + get_with_version** impls + 7 regression tests; retire Day 14 stub-pin test
4. **lessons/loader.rs** grows async `get_by_id(&ctx, storage, id) -> Result<_, EngineError>` alongside the deprecated sync wrapper (`Handle::block_on`)
5. **lessons/signals.rs** grows async `record_sentiment_signal(&ctx, storage, ...)` with 5-retry bounded CAS loop
6. **TestHarness** lands + ~15 ENV_LOCK-using tests rewrite
7. **StorageBackedSignalWriter** lands
8. **main.rs** stub orchestrator wiring (deactivated — no classifier yet)

### D9. No new dependencies
All Day 16b deps already in tree: `fd-lock`, `tempfile`, `thiserror`, `tokio`, `bytes`, `async-trait`, `dashmap`, `anyhow`.

### D10. File-size watch points
- `storage/filesystem.rs`: 343 LOC → projected ~500 with CAS additions. If audit shows over, split into `filesystem/{mod, cas, io}.rs` (pre-planned).
- `storage/lock.rs` (new): keep below 200 LOC.

### D11. Audit smells to flag (S31-S43, 13 new persistence-specific)
S31 `fd_lock::RwLock` held across `.await` (S31 caused by NOT using spawn_blocking)
S32 `Version` encoded as String / ISO timestamp
S33 Split read of `(bytes, version)` in get_with_version
S34 `EngineError::Other(anyhow::Error)` — anyhow leak
S35 `tokio::task::spawn_blocking` for I/O that's already async (tokio::fs is async)
S36 `anyhow::Result` in new async APIs
S37 `Result<_, EngineError>::map_err(|e| anyhow!(e))` in non-wrapper code
S38 `Arc<Box<dyn Storage>>` (double indirection)
S39 TestHarness with side-effects-in-Drop
S40 Eager String allocation in error path
S41 CAS-loop without retry bound
S42 `tokio::sync::Mutex<TempDir>` to share between async tasks
S43 `lessons::record_sentiment_signal` rebuilds StorageKey from scratch instead of accepting one

### D12. Production daemon stays non-functional after 16b
No classifier (Haiku adapter is Day 17+). Orchestrator wiring in main.rs is stub. Accepting per scope concern #6 — 16b focuses on persistence-layer correctness only.

---

## OQ decisions (all accepting recommendations)

| OQ | Decision |
|---|---|
| OQ-D16b-A | EngineError::Yaml = `Box<dyn Error>` (no typed variants) |
| OQ-D16b-B | NO sleep between CAS retries (flock serializes) |
| OQ-D16b-C | `lesson_file_path` → `#[doc(hidden)] pub(crate)` in 16b; delete Day 17 |
| OQ-D16b-D | `StorageBackedSignalWriter` derives Debug |
| OQ-D16b-E | NO Clone on EngineError |
| OQ-D16b-F | `TestHarness::seed_lesson` returns `StorageKey` |
| OQ-D16b-G | `record_sentiment_signal` takes `&Context` (no move) |
| OQ-D16b-H | NO `Storage::compare_and_swap` shorthand (layering inversion) |

---

## Build scope (8 commits)

Per D8 above. Each commit must leave `cargo test --all` green.

---

## Audit checklist (for cycle-close audit agent)

- [ ] 223+ prior tests still pass
- [ ] 7 new put_if_version/get_with_version regression tests
- [ ] Day 14 stub-pin test `put_if_version_returns_backend_error_in_phase_3b` RETIRED
- [ ] `cargo test --features test-fixtures` exercises TestHarness
- [ ] No `crate::host` in `src/engine/`
- [ ] All 13 audit smells S31-S43 absent or accepted
- [ ] No `anyhow::Error` in NEW async engine public functions (deprecated wrappers may still convert)
- [ ] File-size: `storage/filesystem.rs` under 500 prod LOC OR pre-planned split applied
- [ ] License: no new deps; no AGPL/GPL/SSPL
- [ ] `StorageBackedSignalWriter` smoke test exercises full orchestrator → writer → storage path

---

Related: [[feedback-workflow-cycle]], [[feedback-rust-idiomatic-refactor]], [[feedback-execute-the-plan]], `docs/research/day-16b-pre-research.md`, `docs/research/day-16-pre-research.md` Q5/Q6/Q8.
