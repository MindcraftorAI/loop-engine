# Day 14 Post-Research Notes

**Date:** 2026-05-13
**Phase:** Post-research (workflow cycle phase 4 — forward-looking)
**Cycle:** Day 14 (single-crate module restructure + Context/Storage/EventSource)
**Build commits:** `be2750a` (3a mechanical), `95ba208` (3b abstractions), `488df4c` (3c audit fixes)

---

## What shipped in Day 14 build

### Phase 3a — Mechanical restructure (`be2750a`)
- `src/yaml/`, `buffer.rs`, `pid.rs`, `lessons/`, `lifecycle.rs`, `paths.rs` → `src/engine/`
- `src/watcher/` (Day 13 WIP, untracked at the time) → `src/host/claude_code/jsonl_watcher/`
- `cli.rs`, `config.rs`, `observability.rs` stayed at `src/` top level (binary-specific glue)
- Internal `crate::*` imports rewritten via sed
- `lib.rs` re-exports the moved engine modules at the crate root for backward compat
- 127 → 127 tests pass (no regressions)

### Phase 3b — New abstractions defined (`95ba208`)
- `engine::context` — Context + ID newtypes (TenantId, UserId, SessionId, TeamId) + ContextBuilder + `single_user_local()`
- `engine::storage` — sealed `dyn Storage` trait, async via `async_trait`, fixed `StorageError`, typed `StorageKey` newtype, opaque `Version`
- `engine::storage::filesystem` — `LocalFsStorage` (production fs backend; `put_if_version`/`get_with_version` stubbed for Phase 3c)
- `engine::storage::memory` — `MemoryStorage` (full impl including CAS)
- `engine::events` — `EventSource` trait, `EngineEvent` enum (UserTurn/UserInterrupt/SessionStarted/SessionEnded), `EventSourceError` (Transient/Fatal)
- `Cargo.toml` deps: `async-trait`, `bytes`, `futures` (and the Day 13 `notify=8` carryover)
- `THIRD_PARTY_LICENSES.md`: declared `notify` CC0-1.0 (closes Day 13 audit A4)
- 127 → 142 tests pass (+15 for the new abstractions)

### Phase 3c — Day 13 audit fixes A1/A2/A3/A5 (`488df4c`)
- A1: `handle_change` picks cursor mode by event kind (`Create` → `new_at_start`, else → `new_at_eof`)
- A2: `read_appended` returns `ReadAppendedResult { lines, actual_read, fragment_len }` with `.advance()`; runner uses `result.advance()` not the requested `count`
- A3: `process_cursor` loops on classify+read until file is caught up (MAX_ITER=64 guard)
- A5: new `initial_scan` synthesizes `PathChange{kind=Modify}` per pre-existing `.jsonl` file at watcher startup
- Bonus: macOS path canonicalization fix in `spawn_watcher` (was producing duplicate cursors for `/var/...` vs `/private/var/...`)
- 142 → 142 unit tests pass + 3 integration

---

## What did NOT ship (scope deferrals)

### JsonlWatcher → impl EventSource — DEFERRED to Day 15

**Why:** The current `EngineEvent::UserTurn` has 4 fields (session_id, event_uuid, text, timestamp, cwd). The current `WatcherEvent::UserTurn` has 8 fields including `parent_uuid`, `git_branch`, `cc_version`. The orchestrator (Day 15+) will need `parent_uuid` for correction-window mining, `cc_version` for the daemon-version tripwire, and possibly `git_branch` for project-scoped sentiment routing.

Trying to nail down `EngineEvent` shape in Day 14 without orchestrator input would be guesswork — exactly what the user's "no guesswork" rule (`feedback_rust_idiomatic_refactor`) forbids.

**Plan for Day 15:**
- Day 15 pre-research nails down `EngineEvent::UserTurn` field set with orchestrator consumption requirements as input
- Day 15 build: refactor `JsonlWatcher` to impl `EventSource` (returns the curated EngineEvent) AND build orchestrator on top
- The trait + the test scaffold (`MemoryStorage` etc.) already shipped in Phase 3b, so Day 15 build has the infrastructure ready

This is documented decision-update mid-build, not workflow drift. The locked learn-notes D8 explicitly described migration as "two-phase" — JsonlWatcher's `EventSource` impl is squarely Phase 2 work.

### `LocalFsStorage::put_if_version` / `get_with_version` — DEFERRED

Stubbed in Phase 3b with `Err(StorageError::Backend(io_err("not yet implemented (Phase 3c)")))`.

**Why deferred:** No engine caller exists yet that needs CAS write. The lessons module still uses the Day 12 sidecar-flock pattern via `paths::loop_home()` directly. Migrating lessons to use `Storage::put_if_version` IS the trigger for this implementation.

**Plan:**
- Day 15+: when sentiment orchestrator writes signals, it needs CAS write → migrate `lessons::signals::record_sentiment_signal` to `storage.put_if_version` → ship the LocalFsStorage impl at that point.
- Implementation pattern: lift the existing sidecar-flock from `engine::lessons::lock` and adapt for opaque keys.

### `cargo-public-api` snapshot — OPT-IN until Day 17 (per OQ4)
Tool integration not added in Day 14. Will add during Day 15-16 once engine surface settles, promote to gating in Day 17.

---

## Forward-feeding learnings (for Day 15 pre-research)

### L1. `EngineEvent` shape: needs orchestrator-driven field decisions

The orchestrator's actual consumption pattern determines which fields belong in `EngineEvent::UserTurn` vs in a `HostExtras` opaque struct vs dropped entirely. Day 15 pre-research must:

- Inventory orchestrator inputs (correction-window mining: parent_uuid; daemon version tripwire: cc_version; project routing: cwd + git_branch)
- Decide: flat fields on `EngineEvent::UserTurn`, OR a `HostExtras` sub-struct, OR a typed enum of host-specific events
- The right answer is probably an `EngineEvent::UserTurn { common_fields..., host_extras: HostExtras }` where `HostExtras` is `#[non_exhaustive]` + opaque to the engine (engine ignores it, host adapters and host-specific consumers downcast).

### L2. Lessons migration is the trigger for `put_if_version`

When the sentiment orchestrator (Day 15) writes signals via `lessons::record_sentiment_signal`, the existing sidecar-flock + atomic rename moves into `LocalFsStorage::put_if_version`. The lessons module then takes `&Context, &dyn Storage` instead of using `paths::loop_home()`.

This is Phase 2 of the two-phase migration plan (learn-notes D8). It's natural to do alongside Day 15 sentiment work.

### L3. Test environment migration is also natural at Day 15

Current 142 tests still use `LOOP_HOME` env + `ENV_LOCK`. When lessons migrates to Context+Storage, those tests migrate to `TestHarness` (per pre-research Q8). The `ENV_LOCK` mutex retires when the last caller is gone.

Risk: a Big Bang test rewrite is risky. Mitigation: migrate test-by-test as the underlying module migrates.

### L4. macOS path canonicalization is a recurring trap

The `spawn_watcher` canonicalization fix was caught during integration test triage. The same mismatch (`/var/...` vs `/private/var/...`) will bite ANY code that mixes `std::fs::read_dir` output with `notify` callback paths.

**Apply forward:** when adding new watch loops in Day 15+, normalize all paths through `std::fs::canonicalize` at entry. Consider a `WatchRoot` newtype that enforces canonicalization at construction.

### L5. Sealed trait pattern: works as designed

The `Storage: sealed::Sealed` pattern + `LocalFsStorage`, `MemoryStorage` both impl `Sealed` inside the crate. External code can't impl `Storage`. Tested by inference; could add a `trybuild` test that proves it fails to compile from outside, but that's overkill for now.

### L6. `Cargo.lock` policy still needs deciding

Currently gitignored (inherited from earlier library-style `.gitignore`). For a binary crate, `Cargo.lock` should be committed for reproducible builds. Not a Day 14 deliverable but should land before sentiment work starts (Day 15) so dependency versions are pinned.

**Decision-needed:** Day 15 kickoff. Recommend: commit `Cargo.lock`, remove from `.gitignore`. One-line change.

### L7. ADR refresh deferred

ADRs 0004 (TypeScript) and 0010 (single-user file layout) are now superseded by the Rust pivot + Context/Storage abstractions but no new ADRs were written in Day 14. Logged in `docs/research/2026-05-13-collapse-post-research.md` items #2 + #3.

**Apply forward:** Day 17 (end of sentiment work, before close) is a natural ADR-refresh moment — by then we have evidence the abstractions earned their keep.

---

## Open questions for Day 14 close (audit phase will report on these)

These are things the audit agent should specifically flag:

- Did I miss any of the 17 TS-with-Rust-syntax smells anywhere in the new code?
- Is the sealed trait pattern actually enforced or could it be bypassed?
- Are there any internal `crate::*` paths that should be `super::*` for readability?
- Does any new file exceed the 500 LOC limit?
- Did `Cargo.toml` license attestations get updated for every new dep?
- Is `THIRD_PARTY_LICENSES.md` accurate for the non-MIT/Apache deps after the additions?

The audit agent (running in parallel) writes findings to `docs/research/day-14-audit-report.md`.

---

## Workflow cycle status

| Phase | Status | Artifact |
|---|---|---|
| 1. Pre-research | ✅ done | `docs/research/day-14-pre-research.md` (1567 lines, agent-produced) |
| 2. Learn | ✅ done | `docs/research/day-14-learn-notes.md` (locked decisions) |
| 3. Build | ✅ done | commits `be2750a`, `95ba208`, `488df4c` |
| 4. Post-research | ✅ done | this file |
| 5. Audit | 🟡 IN FLIGHT | agent running; output at `docs/research/day-14-audit-report.md` |
| 6. Commit | ⏳ pending | will close cycle once audit findings are applied |

Test count: 142 unit + 3 integration = 145 total. All green.

Related: [[feedback-workflow-cycle]], [[feedback-rust-idiomatic-refactor]], [[feedback-execute-the-plan]], `docs/research/day-14-pre-research.md`, `docs/research/day-14-learn-notes.md`
