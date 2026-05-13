# Day 16b Audit Report
**Cycle:** Day 16b (focused scope: EngineError + storage CAS impl + StorageBackedSignalWriter)
**Audit window:** commits `03dd82c..22806ae`
**Phase:** 5 (audit — backward-looking)
**Auditor:** independent agent
**Test baseline at audit:** 238 unit + 3 integration (= 241), all green; `cargo clippy --all-targets` and `--features test-fixtures` both clean with `-D warnings`.

---

## CRITICAL findings

**None.**

The delivered scope (EngineError chassis, storage CAS impl with sidecar-flock, StorageBackedSignalWriter) ships behind a clean public surface, with regression tests that exercise the locked path (not just the happy path) for all five CAS state transitions, and zero `crate::host` references inside `engine/`. D1, D2, D5, D7, D8 (in their tightened form), D9, D10, D11 all compliant. No anyhow leak in new async APIs.

---

## MAJOR findings

### M1. Missing rename-survival test in new `storage/lock.rs`


`/Users/slee/projects/loop/src/engine/storage/lock.rs` claims to be a verbatim lift of the 127-test-validated sidecar-flock pattern (commit message + module doc lines 1-21). The original `/Users/slee/projects/loop/src/engine/lessons/lock.rs` carries FIVE tests, the most important being `lock_survives_target_rename` (lessons/lock.rs lines 158-202) — the entire reason the sidecar pattern exists per audit Day 12 #1. The new `storage/lock.rs` only carries THREE tests (`sidecar_path_is_hidden_and_in_same_dir`, `lock_serializes_concurrent_callers`, `lock_works_when_target_does_not_exist`).

`storage/lock.rs` is the helper that production CAS (`put_if_version_sync`) calls through. The CAS path performs `atomic_write_sync` which does `std::fs::rename(tmp, target)` INSIDE the lock-held critical section. That's exactly the failure pattern the original test was added to catch. Dropping that test on the lift is a meaningful coverage regression for the helper that gates ALL Day 16b correctness claims.

**Fix:** port `lock_survives_target_rename` from `lessons/lock.rs:158-202` into `storage/lock.rs`. ~45 lines, identical body modulo `with_lock` → `with_sidecar_lock`.

### M2. `compute_version_sync` lacks `#[cfg(unix)]` guard

`/Users/slee/projects/loop/src/engine/storage/filesystem.rs:206` does `use std::os::unix::fs::MetadataExt;` unconditionally inside `compute_version_sync`. This breaks Windows builds. Pre-existing patterns in the codebase properly cfg-guard (`/Users/slee/projects/loop/src/engine/pid.rs:15` uses `#[cfg(unix)]` / `#[cfg(not(unix))]`). `engine/lessons/signals.rs` has the same un-guarded pattern (pre-existing, not a 16b regression — but the new CAS path inherits the constraint).

**Assessment:** the project shows no Windows target infrastructure and `fd_lock` itself has Windows semantics that differ. Practically the engine is Unix-only today. But this becomes a meaningful constraint once Day 17+ either (a) adds a CI matrix or (b) the engine extracts as a standalone crate. **Recommend** explicit `#[cfg(unix)]` on the unix-only function with a `compile_error!("loop-engine requires a Unix-like target (mtime_ns + fd_lock)")` for `not(unix)` rather than silent breakage.

---

## MINOR findings

### m1. Stale module-doc on `storage/filesystem.rs`

Lines 7-11 still describe Phase 3b status as "put_if_version and get_with_version are stubbed". The 16b commit retired those stubs. Doc says one thing, code does the opposite.

**Fix:** update the module doc to say "Phase 3c: CAS impls live; see `compute_version_sync` / `with_sidecar_lock`."

### m2. `EngineError` and `StorageBackedSignalWriter` ship as zero-caller public API

Both are `pub` and `pub use`'d at crate root (`engine::EngineError`, `engine::sentiment::StorageBackedSignalWriter`). Neither has a non-test caller in this commit (lessons migration deferred to Day 17, orchestrator wiring deferred). The risk is that ergonomic flaws (variant shape, constructor signature, missing `From` impl) won't surface until Day 17 callers arrive.

**Mitigation already in place:** tests exercise constructor + match patterns. Day 17 audit must re-evaluate ergonomics under actual use.

### m3. `EngineError::CasContended` variant fully unused

Declared in `error.rs:43-44` but emitted from nowhere in 16b. The bounded-retry CAS loop that emits it lives in the deferred `lessons::record_sentiment_signal` migration (post-research L1). Documented; not a build defect; flagged for tracking only.

### m4. `version_changes_on_each_put` test relies on timing, not synchronization

`/Users/slee/projects/loop/src/engine/storage/filesystem.rs:474-490` writes two payloads of DIFFERENT lengths back-to-back and asserts versions differ. The test passes because the lengths differ (the `len` half of the 24-byte version varies). But the test doc-comment claims "Different content → different len → different version" — it does NOT test "same-length different-content → version differs on its own merit." That second property is not guaranteed by the (mtime_ns, len) encoding on coarse-mtime filesystems like APFS in the same ms tick.

D2 in learn-notes explicitly accepts this for single-writer-per-file. So this is **not a bug**, but the test name `version_changes_on_each_put` overpromises. Either rename to `version_changes_when_length_changes`, or add a `std::thread::sleep(Duration::from_millis(2))` between same-length puts and assert version difference for explicit coverage of the mtime half.

### m5. `render_signal_yaml` uses `{:?}` debug-formatting for enum values

`signals.rs:230-259` formats `Polarity`, `Hazard`, `AttributionMethod` via `{:?}`. Persisted YAML format becomes coupled to Rust's `Debug` trait output. Post-research L3 already calls this out as a Day 17 followup ("add `Display` impl or `serialize_to_yaml` method").

Flagged here for visibility — the persisted format is now part of the on-disk contract. If a future Rust version pretty-prints debug output differently (unlikely but theoretically possible) or someone adds a derive that adds fields, the YAML output drifts silently.

### m6. `lessons/lock.rs` remains in tree as a parallel impl

Commit message and learn-notes D1 say "Lift `engine::lessons::lock::with_lock` into a new `engine::storage::lock` module. Re-export from `engine::lessons::lock` for one cycle." 16b lifted the helper but did NOT add the re-export or deprecation marker. Both `lessons/lock.rs::with_lock` and `storage/lock.rs::with_sidecar_lock` now exist as parallel copies of the same pattern, each with their own test suite.

Post-research L2 already tracks this as Day 17 work. The duplication is intentional during the scope-tightening window; flagged here to ensure it doesn't get forgotten if Day 17 also tightens scope.

### m7. `atomic_write_sync` orphans `.tmp` file on rename failure

`filesystem.rs:221-229`: if `std::fs::write(&tmp, bytes)` succeeds but `std::fs::rename(&tmp, path)` fails (rare — disk full, permission), the `.tmp` file is left behind. Pre-existing behavior in the async `put` path; the new CAS path inherits it. Sidecar lock prevents concurrent CAS attempts from clobbering each other's `.tmp`, so this is disk-leak only, not corruption.

**Recommendation:** add `let _ = std::fs::remove_file(&tmp);` on the rename error branch. Two lines, removes a class of operational mysteries during fault-recovery later.

### m8. `MemoryStorage::put_if_version` increments version counter even on CAS failure

`/Users/slee/projects/loop/src/engine/storage/memory.rs:75` mints a new version BEFORE checking the precondition. If CAS fails, the counter still advanced. Pre-existing from Day 14 — NOT a 16b regression — but the new CAS regression tests now exercise the failure paths more, so the wasted counter values are reachable. Trivial monotonic-leak; not a correctness issue.

---

## Verified clean

### Locked-decision compliance

- **D1** put_if_version + get_with_version use `tokio::task::spawn_blocking` wrapping the sync sidecar-flock helper (`filesystem.rs:136`, `:147`). S31 prevented.
- **D2** `Version` encoded as 24 bytes: `mtime_ns (i128 LE, 16 bytes) || len (u64 LE, 8 bytes)` per `compute_version_sync` (`filesystem.rs:205-217`). i128 overflow impossible (i64::MAX * 1e9 + 1e9 ≪ i128::MAX, verified arithmetically). S32 prevented (no String/ISO timestamp encoding).
- **D3** N/A (pre-research, not a build deliverable).
- **D4** N/A (deprecated sync wrappers deferred to Day 17).
- **D5** `EngineError` is crate-level (`engine/error.rs`), `#[non_exhaustive]`, derives `thiserror::Error + Debug`, 7 variants matching spec, `From<StorageError>` + `From<io::Error>` present, NO `Clone` impl (OQ-D16b-E). No `From<anyhow::Error>` defined (S34 prevented).
- **D6** N/A (TestHarness deferred).
- **D7** `StorageBackedSignalWriter` lives in `engine::sentiment::signals`, derives `Debug`, holds `Arc<dyn Storage>` (single indirection, not `Arc<Box<>>` — S38 prevented).
- **D8** 3 of 8 commits delivered (scope-tightened); the 3 are coherent and each leaves `cargo test --all` green.
- **D9** Cargo.toml and Cargo.lock unchanged in audit window (`git diff 03dd82c..22806ae -- Cargo.toml Cargo.lock` empty).
- **D10** `filesystem.rs` prod LOC = 254 (well under 500 ceiling). `storage/lock.rs` prod LOC = 76 (well under 200 ceiling). `error.rs` prod LOC = 83. `signals.rs` prod LOC = 336.
- **D11** 13 audit smells addressed (see "Smell-by-smell" section below).
- **D12** Production daemon stays non-functional (main.rs orchestrator wiring deferred per commit message). Documented.

### Smell-by-smell (S31-S43)

| Smell | Status | Note |
|---|---|---|
| S31 fd_lock across `.await` | ✅ prevented | spawn_blocking wraps sync work |
| S32 Version as String/ISO timestamp | ✅ prevented | 24 opaque bytes |
| S33 Split read of (bytes, version) | ✅ prevented | sidecar lock held through read+stat (`get_with_version_sync` lines 181-193) |
| S34 EngineError::Other(anyhow::Error) | ✅ prevented | Other takes `Box<dyn Error + Send + Sync>`, no `From<anyhow>` impl |
| S35 spawn_blocking for already-async IO | ✅ prevented | only wraps sync `fd_lock` + sync `std::fs::*`; pure tokio::fs paths (get/put/list/delete) untouched |
| S36 anyhow::Result in new async APIs | ✅ prevented | new APIs return `Result<_, StorageError>` or `Result<_, SignalWriteError>` |
| S37 EngineError → anyhow::Error wrapping | ✅ N/A | no migration code yet |
| S38 Arc<Box<dyn Storage>> | ✅ prevented | `StorageBackedSignalWriter::storage: Arc<dyn Storage>` |
| S39 TestHarness Drop-side-effects | ✅ N/A | TestHarness deferred |
| S40 Eager String allocation in error path | ✅ no offenders found in new code |
| S41 CAS-loop without retry bound | ✅ N/A | no CAS loop in 16b; single-shot create-only |
| S42 tokio::sync::Mutex<TempDir> | ✅ N/A | no async TempDir sharing introduced |
| S43 record_sentiment_signal rebuilds StorageKey | ✅ N/A | `StorageBackedSignalWriter::record` builds the StorageKey once from `(ctx, session_id, source_event_uuid)`; lessons migration deferred |

### Code quality

- Zero `crate::host` references in `src/engine/` (boundary contract holds).
- Zero `pub` items in `storage/lock.rs` (helpers correctly `pub(crate)`).
- Zero production-code `unwrap()` calls in new files (all `unwrap()` confined to tests).
- Zero `TODO` / `FIXME` / `XXX` comments in new code.
- Module declarations correct: `engine/mod.rs:15` declares `error`, `engine/mod.rs:25` re-exports `EngineError`; `storage/mod.rs:20` declares `lock` as `pub(crate)`.

### Test coverage

- 5 CAS regression tests in `filesystem.rs:415-526` cover: create-only-on-absent, RMW round-trip with CAS-on-stale-version, get-on-missing, version-changes-on-put (with the m4 caveat), CAS-against-expected-None-when-file-exists, CAS-against-expected-Some-when-file-missing.
- 2 `StorageBackedSignalWriter` tests in `signals.rs:404-448` cover: round-trip persistence + first-write-wins dedup on same `source_event_uuid`. The dedup test correctly asserts `item_id: les-a` (the FIRST write) — proving first-write-wins, not silent data loss.
- 5 `EngineError` tests in `error.rs:84-131` cover: Display formatting, From<StorageError>, From<io::Error>, CasContended payload, yaml() constructor boxing.
- 3 `storage/lock.rs` tests at lines 78-135 (see M1 — missing rename-survival).
- Total new tests: ~15. Lib test count 238 vs Day 16a's 223 = +15. Matches commit message claim.
- Day 14 stub-pin test correctly retired (audit-checklist item from learn-notes line 114).

---

## Specific risk-area verification

Per the audit task's section 3 — walking each named risk against the code:

**`put_if_version_sync` failure modes.**
- Sidecar lock acquisition failure (`lock.rs:64-73`): returns `StorageError::backend(io::Error)` early; no temp file written, no partial state. Verified.
- `atomic_write_sync` mid-rename failure: tmp file orphaned (m7). Target file unchanged. Lock releases cleanly via guard drop. Next CAS attempt reads the unchanged target, recomputes version, and may succeed. **No corruption window** — the version that gets read after a failed rename matches the version that existed BEFORE the failed attempt, so any caller's CAS token remains valid. Verified.
- Version-mismatch path (lines 174): returns `Ok(false)` with no side effects. Verified.

**`get_with_version_sync` early-return paths (`filesystem.rs:181-193`).**
- File-not-found inside the lock-held closure (`Err(e) if e.kind() == io::ErrorKind::NotFound`): returns `Ok(None)` and the closure exits, releasing the lock via guard drop. Verified.
- `std::fs::read` succeeds + `compute_version_sync` fails (e.g. metadata gone between syscalls — impossible under our single-writer-per-file invariant, but theoretically): returns `Err(StorageError)`, lock releases. Verified.
- The lock is held through BOTH `std::fs::read` AND `compute_version_sync`'s subsequent `std::fs::metadata` call → S33 prevented. Critically, the read+stat ordering means the version reflects the file state AT-OR-AFTER the read, never before — so a caller using the returned version for subsequent CAS sees a version that "covers" the bytes they read. This is the correct invariant.

**`compute_version_sync` overflow analysis.**
- `mtime_secs: i64` cast to `i128` → no truncation possible (i64 ⊂ i128).
- `mtime_secs * 1_000_000_000` in i128: max value ≈ 9.2e27, well under i128::MAX (≈1.7e38). Verified arithmetically.
- `mtime_nsec` is `i64`; documented to be `0..1_000_000_000` on Unix; cast to i128 with `as` is safe in all cases.
- `meta.len()` is `u64`; `to_le_bytes()` produces 8 bytes; copied into bytes[16..24]. Verified.

**`StorageBackedSignalWriter::record` idempotency.**
- Call path: `put_if_version(key, body, None)` → on absent file: file created, returns `Ok(true)` → writer returns `Ok(())`. On present file: precondition fails (expected=None but file exists), returns `Ok(false)` → writer ignores `_ok` and returns `Ok(())`.
- **First-write-wins claim verified by the dedup test** (`signals.rs:425-448`): writes signal A (item="les-a"), then signal B (item="les-b") with same `source_event_uuid`, then asserts persisted body contains `item_id: les-a`. This is genuine idempotency, not silent data loss — the second write was rejected at the storage layer, not silently dropped at the writer.
- **Subtle**: the writer cannot distinguish "wrote new" from "was already there" — `Ok(())` covers both. For signal emission tied to unique event UUIDs this is the intended semantic; if Day 17+ adds a caller that NEEDS to know which case happened, it must call `put_if_version` directly.

**`render_signal_yaml` injection safety.**
- The only externally-influenced fields are `item_id`, `source_event_uuid`, and `timestamp`. `item_id` is a `LoadedItemId` typed newtype, `source_event_uuid` is an event UUID generated upstream, `timestamp` is `DateTime<Utc>` formatted via `to_rfc3339`. None can contain `\n`, `:`, or `"` in normal operation.
- `polarity`, `attribution_method`, `detected_hazards` are enum `Debug` outputs — bounded set of identifiers, no colons.
- **Verified low risk** but coupled to Debug format stability (m5).

**`StorageKey::sentiment_signal` safety.**
- Format: `signals/<session_id>/<event_uuid>.yaml` (single-user) or prefixed with `tenants/<t>/users/<u>/`. Session ID and event UUID flow from the orchestrator (engine-side, not user-controlled). The string assembled never starts with `/`, never contains `..`, never contains `\` — so `from_raw`'s assertions hold for all legitimate inputs.
- If a session_id or event_uuid ever DID contain `..` or `/`, `from_raw`'s `assert!` would panic at construction time, fail-fast. Acceptable.
- Verified.

---

## Day 17 deferred-scope tracking

The 16b commit message + post-research L1-L6 + OQ-D17-X1..X3 cleanly enumerate what was deferred:

| Deferred | Tracked in commit msg | Tracked in post-research | Tracked as TODO in code |
|---|---|---|---|
| Step 4: lessons/loader.rs async migration | ✅ | ✅ | ❌ (no inline TODO — acceptable, documented in research artifacts) |
| Step 5: lessons/signals.rs async migration + 5-retry CAS | ✅ | ✅ (L1) | ❌ |
| Step 6: TestHarness + ENV_LOCK retirement | ✅ | ✅ (D6 deferred) | ❌ |
| Step 8: main.rs orchestrator wiring | ✅ | ✅ ("What's STILL missing") | ❌ |
| lessons/lock.rs re-export → retire | ✅ | ✅ (L2 + OQ-D17-X1) | ❌ |
| `render_signal_yaml` proper serializer | (implicit) | ✅ (L3 + L4 + OQ-D17-X2) | ❌ |

**Day 17 carryover hygiene: good.** All deferrals are tracked in at least two research artifacts. Zero `TODO` / `FIXME` comments in source code is intentional — the team's convention routes followups through `docs/research/` not source comments, which matches user feedback on workflow discipline. The risk is that someone reads `error.rs` and sees `CasContended` with no callers and wonders why; a one-line `// emitted by lessons::record_sentiment_signal (Day 17+)` doc-comment on the variant would close that gap cheaply. Worth adding, but not a defect.

---

## TL;DR

Day 16b delivers a clean focused build of the persistence chassis: `EngineError` typed-error landed standalone, `LocalFsStorage` CAS impls properly use `spawn_blocking + sidecar-flock`, and `StorageBackedSignalWriter` correctly implements first-write-wins dedup via create-only CAS. Locked decisions D1, D2, D5, D7, D9, D10, D12 all compliant; all 13 audit smells (S31-S43) either prevented or N/A in delivered scope; 238 lib tests + 3 integration tests pass; clippy clean. The deferrals to Day 17 (lessons migration, TestHarness, orchestrator wiring) are documented in three places (commit message, post-research, learn-notes).

**Biggest concern: M1 — the new `storage/lock.rs` shipped without porting the `lock_survives_target_rename` test from `lessons/lock.rs`.** That test exists specifically because audit Day 12 caught an inode-after-rename race in a naive flock implementation; the new CAS path performs `atomic_write_sync` (which renames) INSIDE the lock-held section. Lifting the helper without lifting its most load-bearing regression test creates a real "second-system" risk where the new copy could silently drift from the validated semantics. ~45 lines to fix. Recommend addressing before Day 17 picks up the lessons migration that will multiply the number of callers depending on this lock.
