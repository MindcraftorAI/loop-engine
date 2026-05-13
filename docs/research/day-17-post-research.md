# Day 17 Post-Research Notes — Final Cycle Before Adapter Discussion

**Date:** 2026-05-13
**Phase:** Post-research (workflow cycle phase 4 — forward-looking)
**Cycle:** Day 17 — final cycle of the agreed plan
**Build commit (TBD final hash):** Day 17 phase 3 commit with solicitor + tripwire + e2e
**Total tests at cycle close (pre-audit):** 249 unit + 4 integration = **253**; clippy clean both modes.

---

## What shipped vs. locked decisions

| Decision | Status | Notes |
|---|---|---|
| D1. Solicitor as pure async fn | ✅ shipped | Not a Service / task / stream consumer |
| D2. Staleness algorithm | ✅ shipped | created_at + external_signal_sources |
| D3. SolicitorOutput shape | ✅ shipped | scanned + skipped + Vec<StaleCandidate> |
| D4. HostVersion tripwire | ✅ shipped | HostVersionPolicy + AbstainReason variant + handle_user_turn wiring |
| D5. Scenario (a) integration test | ✅ shipped | `tests/orchestrator_e2e.rs` |
| D6. Module organization | ✅ shipped | engine::sentiment::solicitor |
| D7. DEFERRED items | ✅ documented | routed to post-adapter-discussion |
| D8. No new deps | ✅ shipped | |
| D9. Audit smells subset | ✅ shipped | S44/S46/S51/S52 absent in code |

---

## The agreed plan is COMPLETE through Day 17

This is the boundary the user set: "you keep going until you're done with day 17. we should discuss the adapters before we start post-17."

The shipped engine now has:
- **Day 14**: Context + Storage + EventSource abstractions (multi-tenant ready)
- **Day 15**: pretrigger + SentimentClassifier trait + attribution
- **Day 16a**: orchestrator + JsonlWatcher EventSource impl + SignalWriter trait + tripwire-ready integration
- **Day 16b**: LocalFsStorage CAS (put_if_version / get_with_version) + EngineError + StorageBackedSignalWriter
- **Day 17**: solicitor + HostVersion tripwire + e2e integration test

**The engine can functionally process a sentiment loop end-to-end** (proven by the e2e integration test) — pretrigger → classifier → attribution → orchestrator → SignalWriter → Storage. The only thing missing for a fully-functional production daemon is the **Anthropic Haiku classifier adapter** (post-17 work).

---

## Forward-feeding to post-adapter-discussion work

### 16a/16b/17 carryover (all routed to "after the adapter design alignment")

Deferred from various cycles, now consolidated:

1. **Lessons module migration** to async `(&Context, &dyn Storage) -> Result<_, EngineError>` API
   - `loader.rs` async `get_by_id`
   - `signals.rs` async `record_sentiment_signal` with bounded 5-retry CAS loop
   - `lock.rs` retire / route to `storage::lock`
2. **TestHarness** in `engine::test_support` behind `test-fixtures` feature
3. **ENV_LOCK retirement** for the migrated module tests (~15 tests)
4. **Signal-array aggregation**: StorageBackedSignalWriter appends signals to the lesson YAML's `signals:` array (currently writes standalone files)
5. **main.rs orchestrator stub wiring** (daemon binary stays non-functional until classifier lands)
6. **Scenario (b) integration test**: JsonlWatcherSource → Orchestrator → MemoryStorage end-to-end (Day 16a L4 deferral)
7. **Sync-wrapper retirement** for lessons API (Day 16b deferral)
8. **render_signal_yaml** → real serializer (Day 16b L3 deferral; Debug-format-as-data smell)
9. **cargo-public-api gating** (Day 14 OQ4 deferral — opt-in → gating)
10. **Semver-aware HostVersionPolicy comparison** (Day 17 deferral; lexicographic is adequate today)

### Adapter design questions (PAUSE point)

The user flagged adapter work needs discussion. Specific questions blocked on user input:

- **Anthropic Haiku adapter**: API key handling (env var? config file? credential helper?); retry policy; fallback strategy when Haiku is down; PII/secret redactor scope; response-shape parsing
- **Claude Code e2e integration test**: Haiku stubbing strategy (mock server vs real call vs canned responses); test runtime budget; CI integration approach

These are pre-build alignment questions, not implementation questions.

---

## Patterns established across Days 10-17

The engine codebase has converged on consistent patterns worth documenting for future contributors:

1. **Sealed traits** for engine-internal abstractions (`Storage`, `SentimentClassifier`)
2. **`#[non_exhaustive]`** on all growth-prone public types
3. **`Arc<dyn Trait>`** for shared singletons (classifier, writer, storage)
4. **Object-safe `async fn` via `async_trait`** macro
5. **`Arc<str>` newtypes** for cheap-clone identity types (SessionId, TenantId, LoadedItemId, HostVersion, ProjectTag)
6. **Critical-section discipline**: `lock → snapshot → drop → await → re-lock` (clippy::await_holding_lock = deny)
7. **`tokio::task::spawn_blocking`** for sync I/O wrapped in async APIs (CAS path, fd_lock)
8. **Pure functions** for algorithmic work (attribute_signal, derive_signals, solicit_stale_lessons)
9. **Builder-chain mocks** with one-shot fault injection
10. **5-phase locked workflow cycle**: pre-research → learn → build → post-research → audit → commit

---

## Workflow cycle status

| Phase | Status | Artifact |
|---|---|---|
| 1. Pre-research | ✅ done | `day-17-pre-research.md` (902 lines) |
| 2. Learn | ✅ done | `day-17-learn-notes.md` (scope-tightened) |
| 3. Build | ✅ done | commit (solicitor + tripwire + e2e) |
| 4. Post-research | ✅ done | this file |
| 5. Audit | 🟡 running | output to `day-17-audit-report.md` |
| 6. Commit | ⏳ pending | will close cycle once audit findings applied |

Test count: 249 unit + 4 integration = **253**. All green.

---

## Next action: PAUSE for adapter discussion

Per the user's directive 2026-05-13: "we should discuss the adapters before we start post-17." Day 17 cycle closes; the next exchange is alignment on the Anthropic Haiku adapter design + the Claude Code e2e integration test approach.

Related: [[feedback-workflow-cycle]], [[feedback-rust-idiomatic-refactor]], [[feedback-execute-the-plan]], `docs/research/day-17-pre-research.md`, `docs/research/day-17-learn-notes.md`
