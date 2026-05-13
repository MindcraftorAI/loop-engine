# Day 16a Learn Notes — Locked Decisions for Build Phase

**Date:** 2026-05-13
**Cycle phase:** Learn (workflow cycle phase 2)
**Cycle:** Day 16a (orchestrator + JsonlWatcher→EventSource + smoke wiring, NO persistence)
**Source pre-research:** `docs/research/day-16-pre-research.md` (1455 lines covering 16a + 16b)

Day 16 splits into 16a / 16b per pre-research Q1. This learn-notes covers 16a only; 16b learn-notes lands at the start of that cycle.

---

## Locked decisions (verbatim from pre-research D1-D16 16a-applicable, condensed)

### D1. Split 16a / 16b
- 16a (this cycle): orchestrator + EventSource impl + smoke wiring; NO signal-write to disk
- 16b (next cycle): `put_if_version` + `get_with_version` + lessons migration + replace 16a's `LoggingSignalWriter` with `StorageBackedSignalWriter`

### D2. Orchestrator state shape
- `Arc<DashMap<SessionId, Mutex<SessionState>>>` shell
- `SessionState` = `#[non_exhaustive]` plain struct: `recent_turns: VecDeque<RecentTurn>`, `rate_limit: HashMap<LoadedItemId, Instant>`, `phase: SessionPhase`, `turn_count: u64`
- `SessionPhase` = `#[non_exhaustive] enum { Idle, AwaitingClassifier { ... } }`

### D3. Per-session keying
- Key on `SessionId` only for 16a
- Per-lesson rate limit lives INSIDE `SessionState` as a `HashMap<LoadedItemId, Instant>`, NOT a separate top-level map
- Multi-tenant `(TenantId, SessionId)` keying deferred to SaaS-mode

### D4. Rate limiting
- Hand-rolled `HashMap<LoadedItemId, Instant>` + cooldown check
- NOT `governor` (overkill for a single fixed-cooldown rule)
- Default cooldown 60s; configurable via `OrchestratorConfig`

### D5. Lock discipline
- `std::sync::Mutex` (NOT `tokio::sync::Mutex`) for `SessionState`
- Critical sections never `.await` — short snapshot/drop pattern around classifier calls
- Clippy lint `clippy::await_holding_lock` set to `deny` at the orchestrator module level

### D6. New dependency
- `dashmap = "6"` direct dep, MIT — update `THIRD_PARTY_LICENSES.md` (or rely on MIT umbrella in Cargo.toml comment)

### D7. `JsonlWatcher` → `EventSource` impl
- New `JsonlWatcherSource` struct in `src/host/claude_code/jsonl_watcher/source.rs`
- Wraps existing `spawn_watcher`; bridges mpsc to `BoxStream` via `tokio_stream::wrappers::UnboundedReceiverStream`
- Old `spawn_watcher` stays for backward compat (Day 13 integration tests keep working)

### D8. Translation rules (WatcherEvent → EngineEvent)
- `WatcherEvent::UserTurn.parent_uuid` → `EngineEvent::UserTurn.parent_event_uuid`
- `cc_version` → `Some(HostVersion::new(cc_version))`
- `git_branch.or_else(cwd.file_name())` → `Some(ProjectTag::new(derived))` — host adapter derives per Day 15 OQ5
- `WatcherEvent::ParseError` → `EventSourceError::Transient`
- Other variants: typed 1:1

### D9. Hazard auto-abstain set
- `Hazard::Sarcasm | Hazard::AmbiguousReferent | Hazard::OutOfDistribution`
- Plus `Hazard::SelfDirected` if added (see OQ-D16a-1)

### D10. Polarity-asymmetric thresholds
- `POSITIVE_MIN: f32 = 0.75`, `NEGATIVE_MIN: f32 = 0.85`
- Named consts in orchestrator module; cited to `sentiment-design-rules.md` rule 5

### D11. Attribution cross-check
- Orchestrator calls Day 15 `attribute_signal` (NO fallback in 16a — Pass 4 closure is for 16b+)
- Verify `attribution.item_id == item.item_id`; mismatch ⇒ skip (audit-A2)

### D12. Correction-window mining
- On `EngineEvent::UserInterrupt`: search recent_turns for last assistant turn that referenced items
- If within `correction_window` (default 30s), emit negative signals on those items
- Rule 15: skip when assistant didn't reference any item

### D13. SignalWriter abstraction (16a-only — replaced by 16b)
- `pub trait SignalWriter { async fn record(&self, ctx: &Context, signal: &SentimentSignal) -> Result<(), SignalWriteError>; }`
- 16a ships: `LoggingSignalWriter` (writes to `tracing`) + `MockSignalWriter` (test-fixtures)
- 16b replaces with `StorageBackedSignalWriter` (calls `lessons::record_sentiment_signal` over `Storage::put_if_version`)

### D14. Test strategy
- Inline `#[cfg(test)]` for `derive_signals`, hazard filter, correction-window pure rules
- Integration in `tests/orchestrator_*.rs` using `MockSentimentClassifier` + `MockSignalWriter` + `MemoryStorage`
- Smoke: `JsonlWatcherSource → orchestrator → MockSignalWriter` end-to-end with synthesized JSONL

### D15. File-size budget
- `orchestrator.rs` target 400-500 LOC (under 500 hard limit)
- Split into `orchestrator/{mod, state, signals, correction_window}.rs` if exceeded

### D16. Module-scoped clippy lints
- `#![deny(clippy::await_holding_lock)]` at top of `orchestrator.rs`
- `#![warn(clippy::mut_mutex_lock)]`
- `#![warn(clippy::significant_drop_in_scrutinee)]`

---

## Open-question decisions (accepting pre-research recommendations)

### OQ-D16a-1. Add `Hazard::SelfDirected` variant? → YES
`Hazard` is `#[non_exhaustive]` so non-breaking. Adds in 16a alongside auto-abstain set.

### OQ-D16a-2. `SignalWriter` shape → TRAIT
Clean test seam + clear seam for 16b replacement. Two impls (Logging + Mock) in 16a.

### OQ-D16a-3. Orchestrator output type → STRUCTURED
`OrchestratorOutput { signals: Vec<SentimentSignal>, abstained: bool, abstention_reason: Option<AbstainReason> }`. Day 17 calibration will need `abstention_reason`.

### OQ-D16a-4. `OrchestratorConfig` → MODULE-LOCAL
No global `EngineConfig` yet (Day 14 didn't introduce one). 16a ships `OrchestratorConfig` at module scope; Day 17+ may roll up.

### OQ-D16a-5. `recent_turns` capacity → 6 default, configurable
`OrchestratorConfig.recent_turn_capacity: usize = 6` (design rule: 4-6 recent turns).

### OQ-D16a-6. Turn-text truncation → CLASSIFIER's job
Orchestrator stores full text; classifier truncates when building its prompt (model windows vary).

### OQ-D16a-7. `clippy::await_holding_lock` policy → MODULE-SCOPED DENY
`#![deny(clippy::await_holding_lock)]` at top of `orchestrator.rs` only. Crate-wide stays default.

### OQ-D16a-8. `JsonlWatcherSource::run` shutdown → SPAWN
Spawn small task that waits on `shutdown.cancelled()` to drop the handle. Simpler than `select!`.

---

## Build phase scope (Day 16a)

### Phase 3a — `Hazard::SelfDirected` variant addition (~5 LOC)
- Add to `engine::sentiment::types::Hazard` enum (non-breaking via `#[non_exhaustive]`).

### Phase 3b — `SentimentSignal` type + `SignalWriter` trait + impls (~140 LOC)
- New `engine::sentiment::signals` module
- `SentimentSignal` struct (item_id, polarity, calibrated_confidence, evidence, hazards, attribution_method, timestamp)
- `SentimentSignalKind` (Direct, Interrupt, Correction)
- `SignalWriter` trait (`#[async_trait]`)
- `LoggingSignalWriter` impl (writes to `tracing::info`)
- `MockSignalWriter` impl (behind `test-fixtures`)
- `SignalWriteError` enum

### Phase 3c — `OrchestratorConfig` + `SessionState` + `SessionPhase` (~100 LOC)
- `engine::sentiment::orchestrator::config::OrchestratorConfig`
- `engine::sentiment::orchestrator::state::SessionState`, `SessionPhase`
- Helpers (rate-limit check, recent-turn ring-buffer ops)

### Phase 3d — `Orchestrator` (~300-400 LOC)
- `engine::sentiment::orchestrator::Orchestrator { state: Arc<DashMap<...>>, classifier: Arc<dyn SentimentClassifier>, writer: Arc<dyn SignalWriter>, config: OrchestratorConfig }`
- `pub fn new(...) -> Self`
- `pub async fn process_event(&self, ctx: &Context, event: &EngineEvent) -> OrchestratorOutput`
- Internal helpers: `handle_user_turn`, `handle_user_interrupt`, `derive_signals`, `apply_hazard_auto_abstain`, `apply_thresholds`, `correction_window_mining`

### Phase 3e — `JsonlWatcherSource` (~150 LOC)
- New file `src/host/claude_code/jsonl_watcher/source.rs`
- `pub struct JsonlWatcherSource { dir: PathBuf }` (constructor takes the directory to watch)
- `impl EventSource for JsonlWatcherSource`:
  - `async fn run(&self, ctx: &Context, shutdown: CancellationToken) -> BoxStream<'static, Result<EngineEvent, EventSourceError>>`
  - Internally: `spawn_watcher(self.dir.clone(), tx)` → bridge via `UnboundedReceiverStream` → `.map(translate)`
  - Spawn shutdown-watcher task that drops the handle on `shutdown.cancelled()`
- `fn name(&self) -> "claude_code_jsonl"`
- Translation helpers `WatcherEvent → Result<EngineEvent, EventSourceError>`

### Phase 3f — wiring, license, tests, smoke
- `Cargo.toml`: `dashmap = "6"`
- `src/engine/sentiment/mod.rs` re-exports for orchestrator
- `src/host/claude_code/jsonl_watcher/mod.rs`: declare `source` submodule
- `src/host/claude_code/mod.rs` and crate root re-exports updated
- Integration tests in `tests/orchestrator_smoke.rs` using fixtures + smoke
- `cargo test --all --features test-fixtures` green
- `cargo clippy --all-targets` (default + with feature) green

---

## Audit checklist for Day 16a audit phase

Will be expanded by audit agent. Key items:

- [ ] All 197+ prior tests still pass
- [ ] New tests: ≥20 orchestrator pure-rule tests + ≥3 integration smoke tests
- [ ] No `crate::host` reference inside `src/engine/`
- [ ] `await_holding_lock` deny lint active in orchestrator module
- [ ] `std::sync::Mutex` (not tokio's) for SessionState
- [ ] `Arc<DashMap<SessionId, Mutex<SessionState>>>` pattern as designed
- [ ] `SignalWriter` is a trait, has Logging + Mock impls
- [ ] `JsonlWatcherSource` impls `EventSource`; old `spawn_watcher` still public for backward compat
- [ ] All 13 sentiment-orchestrator-specific TS-with-Rust-syntax smells S18-S30 absent or accepted
- [ ] No `governor`, no `parking_lot`, no `ractor` deps added
- [ ] License: `dashmap` MIT, declared
- [ ] File-size: orchestrator.rs ≤500 LOC; if approaching, plan a split

---

## What this learn-notes does NOT decide

- 16b decisions (separate learn-notes when that cycle starts)
- Day 17 solicitor structure
- Whether `tokio::sync::Mutex` migration ever happens (Day 17+ if contention shows)

Related: [[feedback-workflow-cycle]], [[feedback-rust-idiomatic-refactor]], [[feedback-execute-the-plan]], `docs/research/day-16-pre-research.md`, `docs/research/day-15-post-research.md`
