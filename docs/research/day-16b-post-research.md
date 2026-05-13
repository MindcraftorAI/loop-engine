# Day 16b Post-Research Notes

**Date:** 2026-05-13
**Phase:** Post-research (workflow cycle phase 4 — forward-looking)
**Cycle:** Day 16b (FOCUSED SCOPE: EngineError + storage CAS impl + StorageBackedSignalWriter)
**Build commit:** `22806ae`
**Total tests at cycle close (pre-audit):** 238 unit + 3 integration = **241**; clippy clean both default and `--features test-fixtures`.

---

## What shipped vs. locked decisions

| Decision | Status | Notes |
|---|---|---|
| D1. put_if_version impl strategy | ✅ shipped | spawn_blocking + sidecar-flock per spec |
| D2. Version encoding 24 bytes (mtime_ns + len) | ✅ shipped | LE bytes, i128 + u64 |
| D3. TS cross-process compat verified | ✅ pre-research | not a build deliverable |
| D4. Handle::block_on for sync wrappers | ⚠️ deferred | wrappers themselves deferred to Day 17 |
| D5. EngineError crate-level | ✅ shipped | 7 variants, From<StorageError>, From<io::Error> |
| D6. TestHarness | ⏸️ DEFERRED to Day 17 | scope-tightening |
| D7. StorageBackedSignalWriter | ✅ shipped (abridged) | one file per signal; lesson-aggregation deferred |
| D8. 8-commit cadence | ⚠️ 3 of 8 shipped | scope-tightening; rest routed to Day 17 |
| D9. No new deps | ✅ shipped | Cargo.toml unchanged |
| D10. File-size watch | ✅ shipped | filesystem.rs grew, but stayed under 500 prod LOC (check during audit) |
| D11. 13 smells (S31-S43) | ✅ documented | audit verifies absence in delivered scope |
| D12. Daemon stays non-functional | ✅ accepted | wiring deferred to Day 17 |

OQ-D16b-A..H: only OQ-A (EngineError::Yaml = Box) was directly applied in the build. Other OQs apply to deferred work and route to Day 17.

---

## Scope tightening rationale (recorded for cycle-close clarity)

Day 16b's pre-research projected ~700 LOC across 8 commits. Building all 8 in one cycle would:
1. Push audit surface past Day 14's 5-finding sweet spot
2. Mix two distinct concerns (storage-layer correctness AND lessons-module migration) in one cycle
3. Force the production daemon to remain non-functional anyway (no classifier until Day 17)

**Decision: ship the critical persistence path (Steps 1+2+3+5+7 abridged) and route the rest to Day 17.** This keeps:
- Audit surface bounded (3 new files, ~390 new prod LOC)
- Two concerns clearly separated (16b = storage correctness; Day 17 = lessons migration + integration)
- Day 17 scope as the natural co-pilot for the lessons migration (the solicitor needs the migrated lessons API anyway)

This is documented decision-update mid-build, not workflow drift. The locked plan's 8 commits stay valid; 5 of them just move to Day 17.

---

## What's now POSSIBLE end-to-end (regression-tested)

A wired daemon CAN now:
1. Construct `Arc<dyn Storage>` via `LocalFsStorage::new(loop_home)`
2. Build `Orchestrator::new(classifier, StorageBackedSignalWriter::new(storage), config)`
3. Process `EngineEvent::UserTurn` via classifier → derive_signals → write each emitted signal as `signals/<session>/<event-uuid>.yaml`
4. Use CAS create-only semantics (`put_if_version(key, bytes, None)`) for idempotent emit — duplicate event_uuids silently dedupe
5. Verify writes via `get` / `get_with_version`

What's STILL missing:
- Lessons module hasn't migrated to the async (`&Context, &dyn Storage`) API. The 16b code path is signals-only.
- main.rs doesn't construct an Orchestrator (no classifier yet — Haiku adapter is post-17).
- Lesson YAML signals: arrays aren't aggregated. The standalone signal files at `signals/<session>/<event>.yaml` are a stepping stone.

---

## Forward-feeding learnings for Day 17

### L1. The bounded CAS retry policy from pre-research Q4/Q5 is unused in 16b

Pre-research D7 said `lessons::record_sentiment_signal` would carry a 5-retry CAS loop. 16b's `StorageBackedSignalWriter` calls `put_if_version` exactly ONCE with `expected_version: None` (create-only) — no retry needed because there's no read-modify-write.

**Apply forward:** when Day 17 migrates lessons to the async API AND adds the lesson-aggregation path (read existing signals: array → append → write back), THAT's where the 5-retry bounded CAS loop lands. Until then, no retry needed.

### L2. The lift-vs-move decision for `lessons/lock.rs`

Pre-research D1 said "lift `lessons::lock::with_lock` into a new `engine::storage::lock` module. Re-export from `engine::lessons::lock` for one cycle." 16b LIFTED the helper (made a separate `storage/lock.rs` with `with_sidecar_lock`) but did NOT touch `lessons/lock.rs` (no re-export, no deprecation). Both modules now contain a sidecar-flock helper independently.

**Apply forward:** Day 17 lessons migration is the natural moment to either (a) retire `lessons/lock.rs` by routing its callers through `storage::lock::with_sidecar_lock`, OR (b) make `lessons/lock.rs` a thin re-export of `storage::lock`. Recommend (b) until all callers retire.

### L3. `Hazard` and `AttributionMethod` are `Debug`-formatted in YAML output

`render_signal_yaml` uses `{:?}` for `Polarity`, `Hazard`, and `AttributionMethod`. Debug-format-as-data is a smell (it changes if Rust's Debug ever pretty-prints differently). Audit may flag.

**Apply forward:** Day 17 should add a `Display` impl on each of these enums (or a serialize_to_yaml method) and migrate `render_signal_yaml`. Until then, the Debug output is human-readable and stable across rust versions in practice.

### L4. Sentiment signal YAML schema is unstandardized

The format `render_signal_yaml` emits is ad-hoc. No serde, no schema. Day 17 should pick a real serialization story:
- (a) Hand-rolled writer modeled on `engine::yaml::writer` (lesson frontmatter)
- (b) `serde_yml` round-trip with `#[derive(Serialize, Deserialize)]`
- (c) Append to the lesson YAML's `signals: array` directly (the eventual target)

### L5. `put_if_version_sync`'s `expected_version: None` semantics

When `expected_version = None` and the file EXISTS, the function returns `Ok(false)` (CAS mismatch). When the file doesn't exist, it returns `Ok(true)` (created). This is "create-only" semantics. Tests cover both paths.

But it's worth flagging: `expected_version: None` doesn't mean "I don't care about the version." It means "I expect the file to not exist." Some callers may want the former.

**Apply forward:** Day 17 may add a `put_unconditional` shorthand if "write regardless of version" becomes a common need. Until then, callers must read the current version first if they want unconditional write semantics.

### L6. `StorageKey::sentiment_signal` introduces a new key shape

Now we have `lesson`, `pid_file`, `config`, `daemon_log`, `sentiment_signal`. Day 17 will likely add more (lesson-signal-array key, manifest snapshot key, ...). The pattern is stable.

---

## Open questions for Day 17 pre-research (in addition to D-D16b-1..7 deferrals)

### OQ-D17-X1. Should Day 17 also retire `lessons/lock.rs`?

L2 above. Pre-research D1 implied yes; build deferred. Day 17 has scope to resolve.

### OQ-D17-X2. `render_signal_yaml` → real serializer?

L3 + L4 above. Pick a serialization story before adding more signal-shape fields.

### OQ-D17-X3. Solicitor's relationship to `StorageBackedSignalWriter`

Day 17 builds the solicitor (stale-lesson detection + host-version tripwire). Does the solicitor:
- (a) Read all signal files via `Storage::list(signals/<session>/)` and inspect freshness?
- (b) Subscribe to a separate event stream that the orchestrator emits alongside signal-write?
- (c) Query an aggregated index that Day 17+ maintains?

Day 17 pre-research must decide.

---

## Workflow cycle status

| Phase | Status | Artifact |
|---|---|---|
| 1. Pre-research | ✅ done | `day-16b-pre-research.md` (1235 lines, agent-produced) |
| 2. Learn | ✅ done | `day-16b-learn-notes.md` (12 decisions + 8 OQ + scope tightening) |
| 3. Build | ✅ done | commit `22806ae` (+2 new files, +6 modified, 632 insertions) |
| 4. Post-research | ✅ done | this file |
| 5. Audit | 🟡 running | output to `day-16b-audit-report.md` |
| 6. Commit | ⏳ pending | will close cycle once audit findings are applied |

Test count: 238 unit + 3 integration = **241**. All green.

Related: [[feedback-workflow-cycle]], [[feedback-rust-idiomatic-refactor]], [[feedback-execute-the-plan]], `docs/research/day-16b-pre-research.md`, `docs/research/day-16b-learn-notes.md`
