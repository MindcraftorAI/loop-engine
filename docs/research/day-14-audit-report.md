# Day 14 Audit Report

**Cycle:** Day 14 (single-crate module restructure + Context/Storage/EventSource)
**Audit window:** commits `da32c25..488df4c` (3 commits: pre-research, mechanical restructure, abstractions, Day 13 audit fixes)
**Phase:** 5 (audit — backward-looking)
**Date:** 2026-05-13

**Build status at audit time:** `cargo build` clean; `cargo test` 142 unit + 3 integration tests pass; `cargo clippy --all-targets` clean.

---

## CRITICAL findings

### C1. `EventSource` trait defined but never implemented

`src/engine/events.rs:99-110` defines the `EventSource` trait and the engine surface re-exports it via `lib.rs`, but no host adapter implements it. `src/host/claude_code/jsonl_watcher/mod.rs` continues to expose the Day-13-shape `spawn_watcher() -> WatcherHandle` returning the local `WatcherEvent` enum instead of `EngineEvent`. Grep confirms zero `impl EventSource` blocks in the tree.

Why it matters: learn-notes D5 and the build-scope item #4 explicitly state "Refactor JsonlWatcher to implement EventSource (returns BoxStream<...> instead of its current ad-hoc mpsc shape)." This was an in-scope deliverable. Without it the `EventSource` trait is dead code — `lib.rs` advertises a public surface that has zero impls and there is no compile-checked guarantee that `EngineEvent` covers the watcher's needs (e.g. `WatcherEvent::UserTurn` has `parent_uuid`, `cc_version`, `git_branch` — `EngineEvent::UserTurn` has only `text`, `event_uuid`, `cwd`, `timestamp`). The trait was designed without verifying the existing emitter could implement it.

Recommended fix: either (a) extend `EngineEvent::UserTurn` to carry the fields `WatcherEvent` carries and add `impl EventSource for JsonlWatcher` as the Phase 1 deliverable D5 demanded, OR (b) document in the Day-14 post-research that the EventSource impl slipped to Day 15 and explicitly mark it as such in the trait's module doc (currently the doc says "**Phase 3b status:** trait + types defined. The first impl (`JsonlWatcher::EventSource`) lands in Phase 3c alongside the Day 13 audit fixes A1-A5" — but the audit fixes shipped, so this comment is now stale).

### C2. `LocalFsStorage::list` returns directory entries as keys, breaking the "keys only" contract

`src/engine/storage/filesystem.rs:87-104`: `list()` iterates `read_dir`, calls `path_to_key_string` on each entry, and pushes regardless of whether the entry is a file or directory. The trait doc on `storage/mod.rs:54-56` says "List all keys under `prefix`. Returns only keys, not bytes." — "keys" everywhere else in the trait means file-addressable blobs, and `MemoryStorage::list` only returns the keys that were `put`. So the two impls disagree on whether sub-directories appear.

Why it matters: callers cannot rely on a uniform `list()` contract across backends. A consumer that round-trips `list()` → `get()` against `LocalFsStorage` will hit `Backend(io::Error: Is a directory)` on sub-dir entries.

Recommended fix: in `list()`, filter `entry.file_type()?.is_file()` before adding to `out`. Add a test case proving directories under the prefix are excluded.

### C3. `LocalFsStorage::list` is non-recursive — does not match `MemoryStorage::list` semantics

`src/engine/storage/filesystem.rs:87-104` calls `tokio::fs::read_dir` on the resolved prefix path. It is **one level deep**. `MemoryStorage::list` does a `starts_with(prefix_str)` filter, which is **arbitrarily deep**. So `list("lessons")` on disk returns the five status directory names; on memory it returns every lesson key across all five statuses.

Why it matters: the Phase 2 `lessons` migration is going to call `list(prefix)` to find lessons. The expected semantics across backends must match. This is the same root cause as C2 but on the recursion axis.

Recommended fix: pick one semantic (recommend recursive, matching `MemoryStorage` and `object_store::list`). Implement `LocalFsStorage::list` with `walkdir` or a manual queue; add round-trip tests against both backends with the same expected output.

---

## MAJOR findings

### M1. `anyhow::Result` returned from public `engine::*` functions

Pre-research smell #3 explicitly forbids `anyhow::Error` in engine public function signatures. Found in:

- `src/engine/paths.rs:21,32,37,43,48,53,60,69,75` — every public function returns `Result<_>` aliased to `anyhow::Result`.
- `src/engine/lessons/loader.rs:42,59,75` — `get_lesson_by_id`, `load_lesson_file`, `lesson_file_path`.
- `src/engine/lessons/signals.rs:22` — file is `use anyhow::{anyhow, Context, Result};`.
- `src/engine/lessons/lock.rs:23` — same.
- `src/engine/lifecycle.rs:16` — same.

Why it matters: this is the textbook TS-with-Rust smell. The engine is supposed to use typed errors (`EngineError`, `StorageError`, etc.); `anyhow` belongs to the binary layer. The Day 14 build was supposed to land Phase 1 *abstractions*, and per learn-notes D8 "delegating wrappers preserve the old API for callers not yet migrated" — that's fine for keeping callers working, but the new `StorageError` type exists and `engine::lessons` is not yet using it.

Note: this is "missed because out-of-scope-for-Phase-1" not "regression." Learn-notes D8 explicitly defers lessons/lifecycle migration to Phase 2. So this is a MAJOR finding only insofar as the audit checklist asks for it. The recommended fix is to land it in Day 15 alongside the function-signature refactor that adds `ctx: &Context` (since that's the same surgery).

### M2. `MemoryStorage` and `LocalFsStorage` both expose unsealed `pub fn new()` — sealed trait insufficient

The `Storage: Sealed` pattern (`storage/mod.rs:82-86`) only prevents external `impl Storage for MyType`. But because `LocalFsStorage` and `MemoryStorage` are themselves `pub` types (re-exported via `engine::storage`), an external caller can construct them and then provide their own `impl Sealed for SomeNewtype` if they wrap... actually no — `sealed::Sealed` is `pub(crate)` so it cannot be referenced from outside. The seal works.

But there is no `trybuild` test pinning that the seal actually prevents external impls. Audit checklist item: "Storage::sealed actually prevents external impls (a test that tries to impl Storage outside the crate, expected to fail compile, recorded via trybuild)" — this test was not added.

Recommended fix: add a `tests/trybuild/sealed_storage_blocks_external.rs` ui-test that attempts `impl Sealed for LocalUnit {}` from outside the crate. Low priority once the architecture is otherwise solid, but the audit checklist called it out explicitly.

### M3. Audit fix A5 has a race window between FSEvent registration and `initial_scan`

`src/host/claude_code/jsonl_watcher/runner.rs:78-86`: the order is `watcher.watch(&dir, ...)` (line 79) → `initial_scan(&dir, &path_tx)` (line 86) → `tokio::spawn(run_loop)` (line 88). FSEvents will start delivering callbacks the moment `watch()` returns. Those callbacks send `PathChange` records into `path_tx`. `initial_scan` then synthesizes its own `Modify` records for the same files.

Behavior in the runner (`handle_change`, lines 181-198): the first record (real-FSEvent or synthesized, whichever arrives first in the channel) triggers `SessionStarted` + cursor creation. The second is a no-op for that file. So `SessionStarted` fires exactly once per file — correct.

BUT: if the real FSEvent for an existing file is a `Modify` (typical, since the file already existed before the watch started), the cursor is `new_at_eof` (tail-from-now). If instead some flavor of FSEvent reports it as `Create` (this **can** happen on macOS FSEvents when a directory has just appeared — the kernel sometimes reports historical creates), the cursor is `new_at_start` (replay-from-zero). On Linux inotify, pre-existing files don't get an IN_CREATE — so initial_scan is the only source, and it sends `Modify` → tail-from-now ✓.

Why it matters: cross-platform the behavior is correct in the common case, but macOS FSEvents-quirk delivery could result in unexpected zero-offset replay of a pre-existing transcript. The fix would be to make `handle_change` ignore the `Create` flavor for files whose `metadata().created()` predates `WatcherHandle` startup time — but that's expensive.

Recommended fix: document the behavior in the runner doc-comment (acknowledge the edge case), keep current code (low-frequency, non-corrupting). OR change classify so that Create-on-known-cursor (initial_scan ran first) doesn't replay. Lower priority.

### M4. `process_cursor` MAX_ITER livelock on single line >1MB

`src/host/claude_code/jsonl_watcher/runner.rs:222-273`: if a file accumulates >1MB of bytes with no `\n` (a single pathological line), `result.advance() == 0` every iteration, `cursor.offset` never advances, and the loop runs 64 iterations each reading the same 1MB. Per-iteration cost: one file open + 1MB read. Total wasted work: 64MB read.

Why it matters: pathological JSONL lines >1MB are unlikely but not impossible (e.g. a user pasting a massive doc). The current code does terminate (MAX_ITER caps it), but it does so silently — the next FSEvent will trigger the same cycle. No tracing event fires when the cap is hit. Once the writer finally emits `\n`, the line is consumed normally, but until then it's effectively a DoS amplifier.

Recommended fix: after `MAX_ITER` iterations with no advance, emit `tracing::warn!(path = %cursor.path.display(), "watcher: stuck on partial line, capped at MAX_ITER")` and return. Optionally bump `cursor.offset` past the fragment with a "skip oversized line" policy (lossy but bounded).

### M5. `WatcherEvent` is not `#[non_exhaustive]`

`src/host/claude_code/jsonl_watcher/events.rs:11`: `pub enum WatcherEvent { ... }` — no `#[non_exhaustive]`. Adding a new variant is a breaking API change for any external consumer that matches exhaustively.

Per learn-notes D6 ("`host::*` is unstable — break freely") this is technically allowed since `host::*` is by-contract unstable. But the same module is going to be the FIRST `EventSource` impl and the variants directly motivate `EngineEvent` variants. Adding `#[non_exhaustive]` is free insurance.

Recommended fix: add `#[non_exhaustive]` to `WatcherEvent` and to `ParseOutcome`, `SkipReason` in `parser.rs`. One-line change each.

### M6. Pre-existing `with_temp_loop_home` + `ENV_LOCK` still in use across 4 test modules

Per D7 ("Drop `with_temp_loop_home` + `ENV_LOCK` once all callers migrated"), the eventual goal is `MemoryStorage` or `TempDir`-backed `LocalFsStorage` for new tests. The audit checklist asks "do any new tests still rely on `LOOP_HOME` env var or the global `ENV_LOCK`?"

The answer: existing tests do (`src/engine/lessons/loader.rs:85-102`, `src/engine/lessons/signals.rs:155-168`, `src/engine/lifecycle.rs:273,311`, `src/engine/paths.rs:84-122`). NEW tests in the audit window (`engine/context.rs`, `engine/storage/*`, `engine/storage/filesystem.rs`, `engine/storage/memory.rs`) do NOT — they all use `TempDir` or in-memory state directly. ✓

So the answer to the audit checklist question is: new tests are clean; legacy tests still use the pattern as expected (D8 defers their migration to Phase 2). This is a tracking note, not a violation.

### M7. `MemoryStorage::new()` is a trivial constructor when `Default` is derived

`src/engine/storage/memory.rs:30-32`: `pub fn new() -> Self { Self::default() }`. The type derives `Default`. Pre-research smell #4 explicitly flags "pub fn new(...) that just assigns fields where Default would do" — this is the textbook hit. Calling sites can use `MemoryStorage::default()` or just `MemoryStorage::new()` — both compile. The `new()` adds no value.

Recommended fix: delete `pub fn new()` (the test code in the same file uses it but can be `MemoryStorage::default()`).

---

## MINOR findings

### m1. `runner.rs::handle_change` has a dead `is_new` boolean

`src/host/claude_code/jsonl_watcher/runner.rs:181,196-198`: `let is_new = !cursors.contains_key(&change.path);` is computed before the match. After the match, `if !is_new { debug!(...); }` logs only for known cursors. The check could just live inside the `Some(c)` arm of the match without precomputing.

Recommended fix: inline the debug log into the `Some(c)` arm; delete `is_new`.

### m2. `process_line` emits `ParseError.offset` as the file offset at the START of the current batch, not the line's actual byte offset

`src/host/claude_code/jsonl_watcher/runner.rs:275-299`: `process_line` reads `cursor.offset` for the `ParseError.offset` field. But `process_line` is called inside the for-loop in `process_cursor` BEFORE `cursor.offset = from + result.advance()` is applied. So all parse errors in a batch report the same offset (the offset at the start of the read), not their actual byte offset within the JSONL file.

Why it matters: makes debugging harder. Not a correctness issue.

Recommended fix: thread a byte-offset accumulator through the per-line loop, or accept the current behavior with a doc-comment.

### m3. `engine::paths::ENV_LOCK` is `#[cfg(test)]` but lives in non-test public module

`src/engine/paths.rs:17-18`: `#[cfg(test)] pub static ENV_LOCK: ...`. The `#[cfg(test)]` ensures it's compiled only in test builds, but it's `pub` — meaning during test builds, any module can reach into `paths::ENV_LOCK`. Currently used in `lessons::loader::tests`, `lessons::signals::tests`, `lifecycle::tests`. That's fine, but the symbol's visibility could be `pub(crate)` to make the test-only invariant explicit at the surface.

Recommended fix: change to `pub(crate) static ENV_LOCK` since no integration test outside the crate accesses it.

### m4. `Version` newtype lacks `Display` / `From` ergonomics

`src/engine/storage/version.rs:7-18`: just `from_bytes` and `as_bytes`. The doc says "S3 would use ETag" — an ETag is a string. Constructing `Version` from `String`/`&str` requires `Version::from_bytes(s.into_bytes())`. Minor ergonomic gap; not load-bearing today since the only versioned impl (`MemoryStorage`) uses raw u64 bytes.

Recommended fix: add `impl From<String>` and `impl Display` in Phase 3c when CAS lands for real.

### m5. `Context::generate_session_id` uses a non-cryptographic, non-monotonic source

`src/engine/context.rs:151-158`: `format!("session-{now:x}")` where `now` is wall-clock nanoseconds. Two processes started in the same nanosecond (or with skewed clocks across hosts) collide. The TS-side generator uses UUIDs.

Why it matters: in single-user mode, a single process per machine — extremely unlikely to collide. But this gets surfaced once multi-tenant is real.

Recommended fix: use `uuid::Uuid::new_v4()` when the crate adds a UUID dep (TS side does), or document the constraint.

### m6. `EngineEvent::UserTurn` does not carry `cc_version` / `parent_uuid` / `git_branch`

`src/engine/events.rs:32-48`: only `session_id`, `event_uuid`, `text`, `timestamp`, `cwd`. The host's `WatcherEvent::UserTurn` carries five additional fields that downstream code uses (correction-window mining wants `parent_uuid`, version-drift tripwire wants `cc_version`). Either (a) `EngineEvent` is missing fields by oversight, or (b) the engine intentionally projects away the host-specific fields. There's no doc comment explaining which.

Recommended fix: decide and document. Likely add `parent_uuid: Option<String>` to `EngineEvent::UserTurn` (broadly applicable) and leave `cc_version`/`git_branch` as host-internal payload.

### m7. `StorageError::Backend` carries `Box<dyn Error>` — technically a "Box<dyn Error>" smell

Pre-research smell #2 forbids `Box<dyn Error>` at API boundaries. `storage/error.rs:25` and `events.rs:70,75` both have it.

Counter-argument: the variant is named (`Backend`, `Transient`, `Fatal`) and the *enum* is typed. `Box<dyn Error>` here is the standard "underlying error preserved as a source chain" pattern (matches `object_store::Error::Generic`, `sqlx::Error::Io`). Not a true smell in this context — the smell #2 wording was about returning a bare `Box<dyn Error>` as the error type, not having it as a wrapped source. Calling out so the audit reviewer can confirm this interpretation.

Verdict: ACCEPTED (false-positive). Documented for clarity.

### m8. `StorageKey::from_raw` uses `debug_assert!` only — release builds accept invalid keys

`src/engine/storage/key.rs:48-54`: invariant check is `debug_assert!`. In release, malformed keys created via `from_raw` will silently propagate. The function is `pub(crate)` so the blast radius is contained, but the safety doc says "Always canonical."

Recommended fix: hard `assert!` (the cost is trivial — three `contains`/`starts_with` checks on a usually-short string).

### m9. `engine::events.rs` module doc references "Phase 3c" which is no longer accurate

`src/engine/events.rs:9-12`: doc says "The first impl (`JsonlWatcher::EventSource`) lands in Phase 3c alongside the Day 13 audit fixes A1-A5." The Day 13 audit fixes shipped in this audit window (488df4c) but no EventSource impl shipped. The phase terminology is stale.

Recommended fix: update to "Phase 2 (Day 15+)" or align with current cycle naming.

### m10. `loop_daemon::lessons` etc. backward-compat re-exports in `lib.rs:29` — fine, but expand the doc

`src/lib.rs:29`: `pub use engine::{buffer, lessons, lifecycle, paths, pid, yaml};` — these are the legacy paths kept alive while callers migrate. The comment above (lines 23-28) explains the intent. No issue, just verify these are removed in Day 15 (per D8) and not allowed to drift permanent.

Recommended fix: track removal in Day 15 build scope.

---

## Verified clean

### Locked-decision compliance

- **D1 (module organization):** `src/engine/` + `src/host/claude_code/` exist; modules are plain `pub mod`; no Cargo features; no workspace; edition stays `2021`. ✓
- **D2 (Context shape):** `tenant_id: TenantId`, `user_id: UserId`, `session_id: SessionId`, `team_id: Option<TeamId>`; `#[non_exhaustive]`; all IDs are `Arc<str>` newtypes with the requested methods; `Context::single_user_local()` exists; no `Default` impl. ✓
- **D3 (Storage trait):** `dyn Storage`-style object-safe trait; `async_trait` macro applied; fixed `StorageError` enum; custom `StorageKey` newtype; `LocalFsStorage` + `MemoryStorage` both ship. ✓
- **D4 (sealed trait):** `pub(crate) mod sealed { pub trait Sealed {} }` correctly scoped; both impls have `impl Sealed for ...`. Trybuild test absent (M2) but the seal pattern is correct in source. ✓ (pattern correct, ✗ test missing)
- **D5 (EventSource trait):** trait defined with `BoxStream<Result<EngineEvent, EventSourceError>>` return; `CancellationToken` parameter present. No impl yet (C1). Trait shape ✓; deliverable ✗.
- **D6 (public surface):** `lib.rs` curates a small prelude (`Context`, `Storage`, `EngineEvent`, etc.); full module paths still work via re-exports. ✓
- **D7 (test strategy):** new tests use `TempDir` (filesystem.rs) or pure in-memory (memory.rs, context.rs, key.rs). Legacy `with_temp_loop_home` retained per D8. ✓
- **D8 (migration phasing):** delegating wrappers via `lib.rs` `pub use engine::*`; old callers unbroken. ✓
- **D9 (Cargo edition):** `edition = "2021"` ✓ (Cargo.toml:4).
- **D10 (dependencies):** `async-trait`, `bytes`, `futures` added; all MIT/Apache-2.0; `notify` declared in `THIRD_PARTY_LICENSES.md` as CC0-1.0 (closes A4). ✓
- **OQ1-OQ7:** all matched recommendations (team_id present, agent_id collapsed into session_id, MemoryStorage ships, async_trait macro used, put_if_version is the CAS primitive). ✓

### Audit fixes A1-A5 verification

- **A1 (tail-from-now for pre-existing):** `runner.rs:181-195`. `PathChangeKind::Create` → `new_at_start`; everything else → `new_at_eof`. The kind comparison is correct: `Create` is what notify reports for files born during this watcher session (FSEvents Create flag, inotify IN_CREATE). For Linux pre-existing files initial_scan synthesizes Modify → tail. ✓
- **A2 (offset advances by actual_read):** `cursor.rs:48-61` defines `ReadAppendedResult { actual_read, fragment_len }` with `advance() = actual_read - fragment_len`. `runner.rs:265` uses `from + result.advance()` not `from + count`. ✓
- **A3 (MAX_APPEND_READ stall fix):** `runner.rs:242-272` loops up to MAX_ITER=64 iterations; each pass classifies + reads up to 1MB; exit when `count < MAX_APPEND_READ` indicating caught up. Bounded but not livelock-free in the single-line-with-no-newline case (M4). ✓ for the intended case.
- **A4 (notify CC0-1.0 declaration):** `THIRD_PARTY_LICENSES.md:14-16` declares `notify` with `CC0-1.0` SPDX + rationale + reference to Day 13 finding A4. ✓
- **A5 (SessionStarted for pre-existing files):** `runner.rs:101-119` `initial_scan` reads the directory at watcher startup, filters `.jsonl`, synthesizes `Modify` PathChange per file. `handle_change`'s first-sight path (line 184) calls `emit_session_started` regardless of kind. ✓
- **Bonus (macOS path canonicalization):** `runner.rs:51-57` canonicalizes the watched dir before registering the watcher, eliminating `/var` vs `/private/var` cursor-key duplication. ✓

### TS-with-Rust-syntax smell sweep

Of the 17 smells in pre-research:

1. `Arc<RwLock<HashMap<String, Context>>>` registry — **NOT FOUND** ✓
2. `Box<dyn Error>` at boundaries — used only as `#[source]` wrapper inside named variants (m7); not a true hit ✓
3. `anyhow::Error` in engine signatures — present in legacy modules per D8 deferral; new modules use typed errors ✓ (and M1)
4. `pub fn new()` trivial constructor — `MemoryStorage::new()` hit (M7)
5. `Option<Option<T>>` — **NOT FOUND** ✓
6. `Vec<Box<dyn Trait>>` for closed-set — **NOT FOUND** ✓
7. `FooOptions` kwargs struct — **NOT FOUND** ✓
8. `async fn` without `await` — `MemoryStorage::{get,put,delete,list,put_if_version,get_with_version}` are `async fn` but the body is sync (held `std::sync::Mutex`). This is FORCED by `Storage: async_trait` and is documented in storage/mod.rs as acceptable (the macro requires async signatures). Real backends will await I/O. ✓ ACCEPTED — trait requires.
9. `String` where `&str` works — **NOT FOUND** in new modules ✓
10. `crate::host::*` from `crate::engine::*` — `grep -rn 'crate::host' src/engine/` returns one hit, in the boundary documentation comment at `engine/mod.rs:9`. No actual code reference. ✓ Boundary rule respected.
11. Stringly-typed scope/status — `StorageKey::lesson(ctx, status: &str, ...)` takes status as `&str` rather than a typed enum. This matches the legacy `paths::LESSON_STATUS_DIRS: &[&str]` pattern. Acceptable for Phase 1; would benefit from a `LessonStatus` enum at the StorageKey API in Phase 2.
12. `Mutex<()>` lock-without-value — `paths::ENV_LOCK: std::sync::Mutex<()>` exists at `paths.rs:18` but it's `#[cfg(test)]` test infrastructure (D8: dropped in Phase 2). ✓ Not a production smell.
13. Unnecessary `Arc<Mutex<T>>` — **NOT FOUND** ✓
14. `if let Some else return Err` — **NOT FOUND** in new modules ✓
15. Manual byte iteration for UTF-8 — `cursor.rs:171` does `buf.iter().rposition(|&b| b == b'\n')`. This is intentional and correct: we're working with raw bytes from a file before UTF-8 validation, looking for the last newline byte. Not a smell. ✓
16. Visitor-pattern transliterations — **NOT FOUND** ✓
17. `tokio::runtime::Handle` field — **NOT FOUND** ✓

### Code quality

- **File size limit (≤500 LOC):** all files in scope are ≤458 LOC (`yaml/writer.rs`). New abstraction files are well under: `storage/filesystem.rs` 240, `storage/memory.rs` 210, `context.rs` 205, `events.rs` 110, `storage/mod.rs` 86, `storage/key.rs` 127. ✓
- **Module boundary:** zero `crate::host` references inside `src/engine/`. ✓
- **`#[non_exhaustive]` discipline:** present on `Context`, `EngineEvent`, `EventSourceError`, `StorageError`. Missing on host-side `WatcherEvent`, `ParseOutcome`, `SkipReason` (M5).
- **Sealed pattern correctness:** `pub(crate) mod sealed` + `pub trait Sealed {}` + `impl Sealed for LocalFsStorage / MemoryStorage` — the seal does block external impls. ✓
- **Test isolation:** new abstraction tests use `TempDir` or in-memory only; no env-var dependency. ✓

### License audit

- `async-trait = "0.1"` — MIT OR Apache-2.0 ✓
- `bytes = "1"` — MIT ✓
- `futures = "0.3"` — MIT OR Apache-2.0 ✓
- `notify = "8"` — CC0-1.0, declared in `THIRD_PARTY_LICENSES.md` ✓
- No AGPL/GPL/SSPL deps introduced. ✓

### Macros / derives / ergonomics

- `Storage` + `EventSource` use `#[async_trait]` correctly imported (`use async_trait::async_trait;`). ✓
- `impl_id_newtype!` macro covers Debug (via derive on the struct), Clone (derive), PartialEq+Eq+Hash (derive), AsRef<str>, Display, plus `new()` and `as_str()`. ✓
- `Context` is `#[non_exhaustive]` ✓
- `StorageError` is `#[non_exhaustive]` and uses `thiserror::Error` ✓
- `Version` has `Debug+Clone+PartialEq+Eq+Hash` (derive) — adequate for CAS use.

### Phase 3b stubs

- `LocalFsStorage::put_if_version` and `get_with_version` return `StorageError::Backend(io_err("... not yet implemented for LocalFsStorage (Phase 3c)"))`. Return type matches trait. Error message is clear about Phase 3c implementation. ✓
- Test `put_if_version_returns_backend_error_in_phase_3b` (filesystem.rs:228-239) pins the contract — flips to failure when stub turns real. ✓

---

## Summary

| Severity | Count | Examples |
|---|---|---|
| CRITICAL | 3 | C1 EventSource never implemented; C2/C3 `LocalFsStorage::list` semantic mismatch with `MemoryStorage::list` |
| MAJOR | 7 | M1 anyhow in engine (deferred per D8); M3 FSEvent/initial_scan race; M4 MAX_ITER livelock on >1MB lines; M5 WatcherEvent not non_exhaustive; M6 legacy tests; M7 trivial `new()`; M2 missing trybuild |
| MINOR | 10 | m1 dead is_new; m2 ParseError offset; m4 Version ergonomics; m5 session_id collision; m6 EngineEvent field gap; m8 debug_assert; m9 stale doc |
| Verified clean | many | all D1-D10 + OQ1-OQ7 except D5 impl; all A1-A5 fix locations correct; 16 of 17 TS smells absent or accepted-justified; boundary rule respected |

---

## TL;DR

The Day 14 build is structurally sound and the abstractions (Context / Storage / EventSource) are idiomatic Rust matching the pre-research design intent. The Day 13 audit fixes (A1-A5 + macOS canonicalization) are all in place and correct. 142 tests pass, clippy is clean.

**The single biggest concern is C1** — the `EventSource` trait was defined and re-exported as part of the public engine surface but **no impl ships**, even though the build-scope explicitly listed "refactor JsonlWatcher to implement EventSource" as Phase 1 work item #4. The trait is currently dead code, the watcher continues to use its Day-13-shape `WatcherEvent`/`mpsc` ad-hoc surface, and crucially the shape of `EngineEvent::UserTurn` was never validated against the fields `WatcherEvent::UserTurn` actually emits (parent_uuid, cc_version, git_branch are missing from EngineEvent). Either the EventSource impl needs to land before Day 14 closes, or the post-research note must explicitly defer it and acknowledge the unvalidated trait shape. C2/C3 are secondary but real: `LocalFsStorage::list` returns directory entries and is non-recursive while `MemoryStorage::list` is recursive — backends MUST agree before Phase 2 lessons migration begins to call it.
