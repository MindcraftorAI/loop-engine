# Day 15 Post-Research Notes

**Date:** 2026-05-13
**Phase:** Post-research (workflow cycle phase 4 — forward-looking)
**Cycle:** Day 15 (sentiment pretrigger + classifier trait + attribution)
**Build commits:** `dab70fc` (phase 3 sentiment); pre-build `5e55f93` (Cargo.lock policy fix, L6 from Day 14)
**Total tests at cycle close:** 197 unit + 3 integration = **200**; clippy clean both default and `--features test-fixtures`.

---

## What shipped (vs. learn-notes locked decisions)

| Decision | Status | Notes |
|---|---|---|
| D1. EngineEvent shape | ✅ shipped | parent_event_uuid + HostVersion + ProjectTag newtypes on UserTurn; parent_event_uuid on UserInterrupt |
| D2. Pretrigger | ✅ shipped | `regex=1` direct dep, `LazyLock<Regex>` + `Pretrigger` struct, `Default` impl, `with_pattern` test injection |
| D3. SentimentClassifier trait | ✅ shipped | sealed async_trait, object-safe, ClassifierError typed enum |
| D4. Attribution | ✅ shipped | pure function + `_with_fallback` closure-generic, no state machine |
| D5. Module layout | ✅ shipped | flat `engine/sentiment/{mod,types,pretrigger,classifier,attribution}.rs` |
| D6. Test strategy | ✅ shipped | MockSentimentClassifier behind `test-fixtures`; 30 adversarial pretrigger fixtures |
| D7. Lessons migration | ✅ deferred per plan | Day 16 work |
| D8. Naming (drop subagent) | ✅ shipped | `ClassificationRequest`, `SentimentClassifier` |
| D9. Three confidence newtypes | ✅ shipped | distinct types, clamp on construction |
| D10. Enum shapes | ✅ shipped | Polarity closed; Hazard/AttributionMethod/LoadedItemKind `#[non_exhaustive]` |
| D11. LoadedItemId | ✅ shipped | `Arc<str>` newtype matching SessionId pattern |
| D12. Dependencies | ✅ shipped | regex direct dep, no other new deps |
| D13. Feature flags | ✅ shipped | `[features] test-fixtures = []` |
| D14. File-size budget | ✅ met | largest is attribution.rs at ~330 LOC, all under 500 |
| D15. License audit | ✅ clean | regex MIT/Apache; no AGPL/GPL/SSPL introduced |

All 9 open-question decisions (OQ1-OQ9) shipped as recommended in pre-research.

---

## Learnings forward-feeding into Day 16

### L1. EngineEvent extension is non-breaking but adapter needs updating

The Day 15 EngineEvent::UserTurn additions (parent_event_uuid, host_version, project_tag) compile because the type is `#[non_exhaustive]` and no existing code constructs `EngineEvent::UserTurn` (the JsonlWatcher adapter still emits `WatcherEvent`, not `EngineEvent`).

**Apply forward:** Day 16 builds the JsonlWatcher → EventSource impl. Translation `WatcherEvent::UserTurn` → `EngineEvent::UserTurn` is now well-defined: `parent_uuid` → `parent_event_uuid`, `cc_version` → `host_version: Some(HostVersion::new(cc_version))`, `git_branch.or_else(cwd basename)` → `project_tag: Some(ProjectTag::new(derived))`.

### L2. Day 16 scope risk — confirmed by build experience

Day 15 build was 5 sub-phases (3a-3e) over ~1300 LOC. Day 16 has THREE substantive deliverables:
1. **JsonlWatcher → EventSource impl** (Phase 1 of integration; ~150 LOC adapter code + translation tests)
2. **`engine::sentiment::orchestrator`** — per-session state machine, rate limiting, hazard auto-abstain, classifier wiring (~400-600 LOC)
3. **Lessons migration to `Storage::put_if_version`** (D7 deferral; per pre-research scope-concern #4: 250-500 LOC across `lessons/loader.rs`, `lessons/signals.rs`, `storage/filesystem.rs`, 7+ test files)

Plus `LocalFsStorage::put_if_version` + `get_with_version` IMPLEMENTATIONS (Day 14 stubs).

**Total estimate:** ~1200-1500 LOC + cross-module test migration.

**Recommend split decision in Day 16 pre-research:** 16a = orchestrator + JsonlWatcher impl (engine sentiment loop end-to-end with no persistence); 16b = lessons migration + `put_if_version` impl + signal write. This isolates the engine sentiment loop validation from the persistence-layer refactor.

### L3. Pretrigger pattern was tighter than expected

The 30 adversarial fixtures stress-tested the pattern in writeable form. One real issue surfaced: the `that's/you're + right` compound was using a basic `['?]` character class instead of the smart-quote-tolerant `['‘’]?` used elsewhere in the pattern. Tests caught it before commit (test `fires_on_thats_right_smart_quote` failed; fixed in-cycle).

**Apply forward:** when adding to the pretrigger pattern (Day 17 audit may bring more fixtures from sentiment-design-rules), always use `['‘’]` not `[']` for apostrophe-tolerance. The literal smart quote chars in the source file are clearer than `\u{2019}` escapes.

### L4. Three confidence newtypes earn their keep at compile time

The `AttributionConfidence` / `ClassifierConfidence` / `CalibratedConfidence` distinction means:
- The attribution algorithm returns `AttributionConfidence` (no risk of conflating with classifier output)
- The classifier returns `ClassifierConfidence` (orchestrator must explicitly transform to `CalibratedConfidence`)
- Promotion thresholds in Day 16 will be on `CalibratedConfidence` — no accidental "raw classifier confidence" leaking past calibration

This is the kind of compile-time invariant that earns the "no guesswork" rule its keep.

### L5. Pure-function attribution is composable

`attribute_signal_with_fallback<F>` accepts a closure for Pass 4. The orchestrator (Day 16) can pass:
- A closure that calls the SentimentClassifier and translates the response
- A closure that returns `None` for testing the abstain path
- A closure that always returns the first candidate for unit-test priors

Generic `FnOnce` monomorphizes — no allocation, no `Box<dyn Fn>`. Cleanest possible shape.

**Apply forward:** Day 16 orchestrator wires this in `process_user_turn` or similar; will look like `attribute_signal_with_fallback(text, items, recents, |candidates| { classifier.classify(...).await ... })`. The `.await` inside the closure works because the closure is called inside an `async fn`.

### L6. Day 14 audit MAJOR M1 (anyhow in legacy engine modules) deferred again

Day 14 audit flagged anyhow::Result returns in legacy engine modules (paths/lessons/lifecycle) as MAJOR; learn-notes deferred to Day 16 per D8 two-phase migration. **Day 16 will swap anyhow for typed errors as it migrates lessons to Storage.** No additional action in Day 15.

### L7. MockSentimentClassifier is the test-fixture template

The `test-fixtures` feature gate + builder-chain mock pattern (lock-protected `VecDeque`, `AtomicUsize` call count) is a reusable shape. Day 16's orchestrator will likely need a `MockEventSource` if not already in scope — same shape.

### L8. JsonlWatcher → EventSource refactor is now SAFE to ship

Day 14 explicitly deferred this because the `EngineEvent::UserTurn` shape wasn't nailed down. Day 15 closed L1 (the shape question). Day 16's JsonlWatcher impl can now translate `WatcherEvent` → `EngineEvent` without guesswork.

---

## Open questions for Day 16 pre-research

These need agent investigation, not learn-phase decisions:

### OQ-D16-1. Orchestrator state machine encoding

Choices: plain `struct + match`, typestate, actor (ractor/actix-flavored), `enum SessionState`, channel-based actor with mpsc. Day 15 attribution intentionally avoided state machines; orchestrator needs SOME state (per-session turn buffer, rate-limit timestamps, in-flight attribution awaiting classifier response).

### OQ-D16-2. Per-session vs per-(session,user) state

Single-user mode has one user per session, but multi-tenant has one user across many sessions. State keying matters. Recommend pre-research surveys axum extractors / sqlx connection pooling patterns for this.

### OQ-D16-3. Rate limiting primitive

`tower::limit::RateLimitLayer`? `governor` crate? Hand-rolled token bucket? The orchestrator's rate limit is per-(session, lesson) for sentiment signal writes (audit-A2 lineage).

### OQ-D16-4. `Storage::put_if_version` LocalFsStorage impl strategy

Day 14 stubbed. Day 16 lifts the existing `lessons/lock.rs` sidecar-flock pattern. Pre-research must decide: (a) lift into `LocalFsStorage::put_if_version` as a generic key-pattern, OR (b) keep flock+sidecar in `lessons` and have `LocalFsStorage::put_if_version` use a different mechanism (e.g. rename-and-stat-version). Option (a) is cleaner conceptually but couples flock semantics to all Storage callers.

### OQ-D16-5. 16a/16b split

Per L2 above. Pre-research should weigh whether two cycles vs one is justifiable; if yes, what's in each.

### OQ-D16-6. Migration of lessons tests away from ENV_LOCK

Day 14 D7/D8 said this happens at lessons-migration time. With Storage now first-class, lesson tests can move to `TestHarness { context, storage: MemoryStorage }`. Risk: 4 modules (loader, signals, lock, lifecycle) of tests to migrate; Big-Bang vs incremental needs decision.

### OQ-D16-7. Pretrigger pattern audit

50-case full fixture set from sentiment-design-rules — Day 17 audit territory per OQ8, but pre-research could surface fixtures that touch the orchestrator (i.e. pretrigger fires but should be hazard-abstained by orchestrator).

---

## Patterns to reuse from Day 15

1. **PURE FUNCTIONS over state machines** when invariants are local to one call
2. **Closure-generic `<F: FnOnce(...)>`** for caller-supplied hooks instead of `Box<dyn FnMut>` or `Option<FnOnce>`
3. **Distinct confidence newtypes** for related-but-different f32 ranges
4. **`test-fixtures` Cargo feature** for cross-crate mock visibility
5. **Adversarial fixture-driven tests** (positive/negative/edge cells)
6. **Smart-quote character class** `['‘’]?` everywhere apostrophe-tolerance matters

## Patterns to NOT repeat

1. ASCII `[']` character class when smart-quote tolerance is intended
2. `pub fn new()` constructor when `Default` is the same thing (M7 from Day 14)
3. Forgetting to `cfg`-gate imports used only in test/feature blocks
4. Defining a type in module A and trying to import it from module B (Attribution struct started in types.rs in the design sketch; moved to attribution.rs at build time)

---

## Workflow cycle status

| Phase | Status | Artifact |
|---|---|---|
| 1. Pre-research | ✅ done | `day-15-pre-research.md` (744 lines, agent-produced) |
| 2. Learn | ✅ done | `day-15-learn-notes.md` (locked decisions) |
| 3. Build | ✅ done | commit `dab70fc` (+5 new files, 1330 insertions) |
| 4. Post-research | ✅ done | this file |
| 5. Audit | 🟡 about to spawn | output to `day-15-audit-report.md` |
| 6. Commit | ⏳ pending | will close cycle once audit findings are applied |

Test count: 197 unit + 3 integration = 200. All green.

Related: [[feedback-workflow-cycle]], [[feedback-rust-idiomatic-refactor]], [[feedback-execute-the-plan]], `docs/research/day-15-pre-research.md`, `docs/research/day-15-learn-notes.md`
