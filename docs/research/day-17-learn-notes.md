# Day 17 Learn Notes — Locked Decisions for Build Phase

**Date:** 2026-05-13
**Cycle phase:** Learn (workflow cycle phase 2)
**Cycle:** Day 17 — solicitor + HostVersion tripwire + engine-level integration tests + (scope-tightened) sentiment loop closing the agreed plan
**Source pre-research:** `docs/research/day-17-pre-research.md` (902 lines, 9 Q sections)

Day 17 is the **FINAL cycle** of the agreed plan. Per the user's directive ("we should discuss the adapters before we start post-17"), this cycle closes with a pause for adapter-design discussion.

---

## Scope tightening rationale (similar to Day 16b)

Pre-research projected ~960 LOC across 9 commits with explicit two-cycle backup plan (17a + 17b). Building all 9 in one cycle would:
- Push audit surface past Day 16b's already-manageable size
- Mix three concerns (solicitor primary scope + lessons migration sweep + integration tests + persistence aggregation)
- Risk an incomplete cycle close blocking the adapter discussion

**Decision: ship Day 17's namesake deliverable + minimum verification (Q1-Q3 + Q6 minimal).** Route the lessons-migration sweep + signal aggregation + TestHarness + main.rs wiring to a "post-adapter-discussion" follow-up cycle (will be defined alongside adapter work).

This is consistent with Day 16b's pattern: scope-tighten when the audit surface or the cycle's coherence would otherwise suffer.

---

## Locked decisions (Day 17 minimum-viable scope)

### D1. Solicitor shape (per Q1)
Pure async function:
```rust
pub async fn solicit_stale_lessons(
    ctx: &Context,
    storage: &dyn Storage,
    config: &SolicitorConfig,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<SolicitorOutput, EngineError>
```
- NOT a tower::Service, NOT a tokio::spawn task, NOT a stream consumer
- Preserves Day 14 "engine never owns its executor" invariant
- Host (daemon main.rs or a cron caller) owns cadence policy

### D2. Staleness algorithm
- Age: use lesson frontmatter `created_at` (NOT filesystem birthtime — flatter porting)
- Signal density proxy: `external_signal_sources.len()` (existing frontmatter field)
- Default `SolicitorConfig`:
  - `min_age_days: 7` — newer lessons skipped
  - `min_signals_threshold: 1` — lessons with ≥1 signal are not stale
  - `window_days: 14` — only signals within this window count toward density
  - `max_candidates_per_call: 1` — one solicitation per invocation

### D3. SolicitorOutput shape
```rust
pub struct SolicitorOutput {
    pub stale_candidates: Vec<StaleCandidate>,
    pub scanned_count: usize,
    pub skipped_count: usize,
}

pub struct StaleCandidate {
    pub lesson_id: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub age_days: u64,
    pub signal_count: usize,
    pub reason: StaleReason,
}
```
- `#[non_exhaustive]` on the output + candidate structs.
- `StaleReason` enum: `NoSignalsInWindow`, `BelowDensityThreshold`.

### D4. HostVersion tripwire (per Q3)
- Add `Hazard::UntestedHostVersion`? No — use `AbstainReason::UntestedHostVersion` instead. The tripwire fires BEFORE classifier call in the orchestrator, abstaining the whole turn.
- `HostVersionPolicy` struct (warn-only default) on `OrchestratorConfig`:
  ```rust
  pub struct HostVersionPolicy {
      pub tested_range: Option<RangeInclusive<String>>,
      pub action: HostVersionAction,
  }
  pub enum HostVersionAction { Warn, Abstain }
  ```
- Default action: Warn (don't abstain by default — tested-range is empty in dev).
- Tripwire is OFF by default (when `tested_range` is None).

### D5. Integration test (per Q6 — only one scenario in 17)
Single integration test in `tests/orchestrator_e2e.rs`:
- Scenario (a): `MockSentimentClassifier → Orchestrator → MockSignalWriter → assert signal emitted`
- Uses Day 16a's `update_manifest` + (orchestrator-driven) UserTurn flow
- Verifies the full sentiment loop without any FSEvents dependency (avoids flakiness)

Scenario (b) from pre-research (JsonlWatcherSource → Orchestrator end-to-end) DEFERRED to post-adapter-discussion.

### D6. Module organization
- `engine::sentiment::solicitor` — new module
  - `solicitor.rs` containing `solicit_stale_lessons` + `SolicitorConfig` + `SolicitorOutput` + `StaleCandidate` + `StaleReason`
- `OrchestratorConfig` gets a `host_version_policy: HostVersionPolicy` field
- `AbstainReason::UntestedHostVersion` variant added to existing enum (non-breaking via `#[non_exhaustive]`)

### D7. DEFERRED to post-adapter-discussion (explicit)
- **Lessons module migration** to async (loader + signals to `&Context, &dyn Storage` API)
- **TestHarness** in engine::test_support
- **ENV_LOCK retirement**
- **Signal-array aggregation** in StorageBackedSignalWriter (lesson YAML append)
- **main.rs orchestrator stub wiring**
- **Scenario (b)** integration test (JsonlWatcherSource end-to-end)
- **Sync-wrapper retirement** (Day 16b deferral)

These items remain documented in the `Day 17 deferrals` section of Day 17's post-research; will be sequenced after the adapter work alignment with the user.

### D8. No new dependencies
All Day 17 deps already in tree.

### D9. Audit smells S44-S48 most relevant (subset of pre-research's S44-S72)
- S44: solicitor owning Interval + CancellationToken (the "task" anti-pattern — pure function instead)
- S46: stringly-typed StaleReason
- S51: tripwire firing in wrong layer (must be in orchestrator, not classifier)
- S52: HostVersionAction as bool (must be enum)
- The rest (S53-S72) apply mostly to deferred work.

---

## Build phase scope (Day 17 minimum)

### Step 1 — Solicitor module
- `src/engine/sentiment/solicitor.rs` (new file, ~200 LOC)
- Wire into `engine::sentiment::mod.rs`
- Inline unit tests (~6 tests covering scan-no-signals, scan-with-signals, age-cutoff, density-cutoff, max-candidates)

### Step 2 — HostVersion tripwire
- Add `HostVersionPolicy` + `HostVersionAction` to `engine::sentiment::orchestrator::config`
- Add `host_version_policy` field to `OrchestratorConfig`
- Add `AbstainReason::UntestedHostVersion` variant
- Wire into `Orchestrator::handle_user_turn` (check BEFORE classifier call)
- 3 tests: policy disabled (no abstain), policy enabled + in-range (no abstain), policy enabled + out-of-range (abstain)

### Step 3 — Integration test
- `tests/orchestrator_e2e.rs` — MockClassifier + Orchestrator + MockSignalWriter scenario

### Step 4 — Verify + commit

---

## Audit checklist for Day 17 audit phase

- [ ] 239+ prior tests still pass
- [ ] ~10 new tests (solicitor + tripwire + integration)
- [ ] No `crate::host` inside `src/engine/`
- [ ] Solicitor is a pure async function (not a Service, not a task)
- [ ] `HostVersionPolicy.tested_range = None` is the silent-noop default
- [ ] AbstainReason::UntestedHostVersion routes through OrchestratorOutput
- [ ] File-size: new files under 500 prod LOC
- [ ] No new deps
- [ ] License: clean
- [ ] Day 17 deferrals documented in post-research

---

Related: [[feedback-workflow-cycle]], [[feedback-rust-idiomatic-refactor]], [[feedback-execute-the-plan]], `docs/research/day-17-pre-research.md`.
