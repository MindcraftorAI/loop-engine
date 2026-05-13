# Day 17 Pre-Research: Solicitor + Lessons Migration + Engine-Level Integration Tests

**Date:** 2026-05-13
**Cycle phase:** Pre-research (workflow cycle phase 1)
**Cycle:** Day 17 — solicitor + inherited Day 16a/16b deferrals (lessons migration, TestHarness, ENV_LOCK retirement, lesson-array aggregation, main.rs orchestrator stub, JsonlWatcher e2e smoke) + full-pipeline integration tests
**Toolchain assumed:** Rust 1.85 (MSRV), Cargo 1.95.0, edition = "2021" (Day 14 D9 still locked).
**Inputs read:**
- `docs/research/day-16b-post-research.md` (L1-L6 + OQ-D17-X1..X3)
- `docs/research/day-16b-audit-report.md` (M1, M2, m1-m8)
- `docs/research/day-16b-pre-research.md` (Q3 migration order, Q4 StorageBackedSignalWriter, Q6 TestHarness, Q7 main.rs wiring)
- `docs/research/day-16a-post-research.md` (L1-L8; especially L4 JsonlWatcher smoke + L5 path leak + L6 OrchestratorOutput shape)
- `docs/research/day-16-pre-research.md` (Q6 lessons migration design, Q7 JsonlWatcherSource)
- `docs/research/sentiment-design-rules.md` (rules 8-12 solicitation, rules 13-16 attribution, hazards + evaluation strategy)
- `docs/research/day-14-learn-notes.md` (D7 TestHarness shape, D8 two-phase migration)
- TS reference: archive not present in workspace (verified `loop-archive-2026-05-13/` absent — TS solicitor design is reconstructed from `sentiment-design-rules.md` rules 8-12 only)
- Current Rust: `src/engine/{lessons, sentiment, storage, events, lifecycle}.rs`, `src/host/claude_code/jsonl_watcher/*`

---

## Executive summary

Day 17 is the heaviest cycle of the engine restructure — it must close five inherited deferrals (lessons migration, TestHarness, ENV_LOCK retirement for migrated tests, lesson-array signal aggregation, `main.rs` orchestrator stub wiring) AND ship two net-new deliverables (the solicitor + engine-level integration tests). Pre-research D-D17-1 below recommends keeping all of this in one cycle with a strict commit-cadence ordering, because the dependencies form a single linear chain — solicitor can't be tested end-to-end without TestHarness; integration tests can't drive the pipeline without main.rs wiring; lesson-array aggregation needs the lessons migration first.

**Five-question summary up front:**

1. **Solicitor shape (Q1):** Plain async function `solicit_stale_lessons(&ctx, &dyn Storage, now, &SolicitorConfig) -> Result<SolicitorOutput, EngineError>` + a separate `is_host_version_in_tested_range(&HostVersion) -> bool` free function. NO `tower::Service`, NO `tokio::spawn` background task. The "periodic" framing in Day 16a L6 is the wrong framing — the function is pure-by-design (no state), so a host can call it on a `tokio::time::interval` tick OR on an `OrchestratorOutput` arrival OR from a CLI invocation. Owning the timer would couple the engine to its executor (Day 14 audit smell "engine receives work; doesn't own its executor").
2. **Lessons migration (Q4):** Three-step leaf-first cadence: (a) `loader.rs` async `get_by_id(&ctx, &dyn Storage, id) -> Result<Option<LoadedLesson>, EngineError>` with sync wrapper preserved; (b) `signals.rs` async `record_sentiment_signal(&ctx, &dyn Storage, id, polarity) -> Result<LoadedLesson, EngineError>` with bounded 5-retry CAS loop; (c) retire `lessons/lock.rs` as a 4-line re-export of `storage::lock::with_sidecar_lock` (audit M1 fix + retirement collapsed). Sync wrappers stay deprecated until **Day 18** (not retired in 17) because `tests/concurrent_signal_writes.rs` still uses them and that's a separate integration-test rewrite.
3. **TestHarness (Q5):** Ship as designed in Day 16b pre-research Q6 (`in_memory()` + `on_disk()` constructors, `Arc<dyn Storage>` field, optional `_tempdir: TempDir`) BUT make the constructors synchronous (no async needed — both `MemoryStorage::default()` and `LocalFsStorage::new(...)` are sync) and make `seed_lesson` async because it calls `Storage::put`. Drop semantics: `_tempdir` declared after `storage` so it drops last (Rust drops in declaration order, last field first). Migrate the 7 tests in `lessons/loader.rs::tests` + 8 tests in `lessons/signals.rs::tests` to TestHarness; ENV_LOCK shrinks from 10 callers to ~5 (paths + lifecycle keep it).
4. **Integration tests (Q6):** Add `tests/orchestrator_e2e.rs` with two scenarios: (a) `MockEventSource → Orchestrator(MockClassifier) → StorageBackedSignalWriter → MemoryStorage` — verifies the engine spine without filesystem dependencies; (b) `JsonlWatcherSource → Orchestrator → MemoryStorage` over a TempDir — the L4-deferred end-to-end smoke. Use `JsonlWatcherSource` as authored Day 16a, write a synthetic JSONL line into the TempDir, drive the orchestrator manually via `update_manifest` + `process_event`, assert signal file exists at `signals/<session>/<event>.yaml` with expected polarity.
5. **Audit smells (Q9):** S44-S55 enumerated. The biggest one: solicitor that owns its own `tokio::time::Interval` + `CancellationToken` (S44). Don't.

Hard constraints respected: no AGPL/GPL/SSPL deps (zero new deps; everything Day 17 needs is in tree); ≤500 prod LOC per file (largest projected file is `lessons/signals.rs` at ~290 prod LOC after CAS-loop addition); `#[non_exhaustive]` on growth-prone types (`SolicitorConfig`, `SolicitorOutput`, `StaleLesson`); Day 14-16b foundations mandatory.

**Scope risk (Q3 + scope-concerns):** The cycle is at ~1100-1300 LOC including test rewrites — Day 16b's "scope-tightening" warning applies. Recommend deferring **2 items to Day 18+ adapter-discussion-pause cycle**: (a) `paths.rs` ENV_LOCK retirement, (b) `lifecycle.rs` ENV_LOCK retirement, (c) `tests/concurrent_signal_writes.rs` rewrite, (d) `render_signal_yaml` `{:?}` → `Display` migration (audit m5/L3). All four are independent of solicitor and integration tests; deferring keeps the audit surface under Day 15's 5-MAJOR-findings ceiling.

---

## Q1: Solicitor shape — periodic task, function, or stream consumer?

### Survey

The solicitor's responsibilities from `sentiment-design-rules.md`:
- **Rule 8.** Max 1 solicited prompt per ~20 turns (reactance ceiling).
- **Rule 15.** Hazards-section: low register volatility → auto-abstain.
- **Hazards.** Sarcasm / ambiguous referent → auto-abstain.
- **Day 17 nominal scope.** Stale-lesson detection (low-signal-density), host-version tripwire (`HostVersion::is_in_tested_range`).

The TS archive is absent from the workspace, so the design recommendation here is reconstructed from `sentiment-design-rules.md` rules 8-12 + Day 16a L6 (the locked OrchestratorOutput shape).

Three architecture options:

#### Option A — Periodic async task spawned with orchestrator

```rust
pub fn spawn_solicitor(
    ctx: Context,
    storage: Arc<dyn Storage>,
    config: SolicitorConfig,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(config.cadence);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let _ = run_pass(&ctx, &*storage, &config).await;
                }
                _ = shutdown.cancelled() => return,
            }
        }
    })
}
```

**Pros:** Self-driving; lifecycle clear (spawn at boot, drop on shutdown).
**Cons:**
- Couples engine to its executor (Day 14 audit smell: "engine receives work; doesn't own its executor"). The engine has been deliberately designed not to spawn its own tasks.
- Lifecycle complexity (cancel + join + error reporting) costs ~50 LOC for a marginal ergonomic gain.
- Tests need `tokio::time::pause()` + `advance()` shenanigans; testability suffers.
- The "cadence" decision (15s? 60s? 5 minutes?) is a host-policy decision, not an engine concern. Single-user Mac daemon wants a slow cadence; SaaS wants fast.

#### Option B — Function called by external scheduler

```rust
pub async fn solicit_stale_lessons(
    ctx: &Context,
    storage: &dyn Storage,
    config: &SolicitorConfig,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<SolicitorOutput, EngineError> {
    // 1. List all lesson keys under `lessons/active/`
    // 2. For each lesson, compute signal density over the last N days
    // 3. Filter to stale candidates (signal_density < threshold AND age >= min_age)
    // 4. Return Vec<StaleLesson>
}

pub fn is_host_version_in_tested_range(
    v: &HostVersion,
    tested_range: &HostVersionRange,
) -> bool { ... }
```

**Pros:**
- Pure function (modulo Storage I/O); easy to test with TestHarness.
- Host owns cadence policy (CLI may call once; daemon may call on a 5-min `Interval`; SaaS may call as part of a batch job).
- Composes with anything (orchestrator output trigger, periodic timer, manual CLI invocation).
- Zero lifecycle complexity — no spawn, no join, no shutdown.
- `now` injected as a parameter = trivially testable without `tokio::time::pause()`.

**Cons:**
- Caller has to write the "every N seconds, call this function" loop. ~15 LOC in `main.rs`.
- No internal state for "I last ran at T, skip if not 20 turns elapsed" — caller tracks. Acceptable: rule 8 ("max 1 prompt per 20 turns") is a session-scoped rule, not a global solicitor-scoped one; orchestrator already has the turn count in `SessionState.turn_count`.

#### Option C — Stream consumer of OrchestratorOutput

```rust
pub fn solicitor_stream(
    input: BoxStream<'static, OrchestratorOutput>,
    storage: Arc<dyn Storage>,
    config: SolicitorConfig,
) -> BoxStream<'static, Result<SolicitorOutput, EngineError>> {
    input.filter_map(|out| async move { ... }).boxed()
}
```

**Pros:** Reactive; emits only when something changed.
**Cons:**
- Stale-lesson detection is **time-driven**, not event-driven. A lesson becomes stale because TIME passed without signals — no event tells you about that. Stream is a wrong fit.
- Host-version tripwire IS event-driven (fires when a UserTurn with unexpected version arrives) — but that's better placed inside the orchestrator's UserTurn handler, not in a separate stream consumer.
- Combining time-driven + event-driven inputs needs a `select_all` upstream, which is what Option A is.

### Survey of real Rust crates

- **`tower::Service<Request, Response>`** — designed for request/response; periodic background work is `tokio::spawn` + `Service::call`. Wrong fit; orchestrator already isn't a Service (Day 16 pre-research Q4 rejected this).
- **`axum::extract::FromRequestParts`** — extractor pattern; periodic work isn't HTTP-shaped.
- **`tokio_cron_scheduler`** — `Apache-2.0`, OK on license, but introduces a 200KB+ scheduler dep for "call this function every N seconds." Overkill.
- **`tracing-subscriber::layer::Filter`** — closest pattern to "function called by the framework on every event." Not directly applicable but reinforces the function-not-actor design.
- **`tokio::time::Interval` + `select!`** — the idiomatic Rust pattern for periodic work. ~10 LOC inline in the caller; this is what `heartbeat_loop` in `lifecycle.rs:109` already does.

### Recommendation

**Option B: pure async function.**

The engine ships `solicit_stale_lessons(&ctx, &dyn Storage, &SolicitorConfig, now) -> Result<SolicitorOutput, EngineError>` plus a free function `is_host_version_in_tested_range(&HostVersion, &HostVersionRange) -> bool`. The host (today, `lifecycle.rs`'s `run_body`; eventually `main.rs`'s wired daemon) is responsible for calling these on whatever cadence its deployment demands.

**Rationale (decision-locking grade):**

1. **Engine policy: engine never owns its executor.** Day 14 audit-smell list (line 185 of learn-notes) explicitly bans `tokio::runtime::Handle` fields: "engine receives work; doesn't own its executor." A `spawn_solicitor` function that internally spawns is a softer violation but the same shape. The function-on-demand design preserves the boundary.
2. **Cadence is host policy.** A CLI invocation (`loop-daemon solicit`) wants single-shot; a daemon wants periodic; a CI run-once-and-exit wants single-shot. The function shape accommodates all three; the spawned task shape forces a fake `CancellationToken::new()` on the CLI path.
3. **Testability.** `now: DateTime<Utc>` parameter means `assert_eq!(solicit_stale_lessons(&ctx, &storage, &cfg, fake_now).await?, expected)` works without `tokio::time::pause()`. Compare to Option A which needs `pause()` + `advance()` + careful interaction with `Interval`'s missed-tick behavior.
4. **Composability with future Adapter cycle.** Day 17 closes before the adapter-discussion pause. The next cycles MAY shift solicitor to a different host shape (Anthropic Auto-Memory ingest callback, MCP RPC endpoint). The function-on-demand design makes that pivot a 0-LOC engine change.

### Code sketch — solicitor module

Lives at `src/engine/sentiment/solicitor.rs` (NOT a sub-module of `orchestrator/` — orchestrator processes events, solicitor surveys state).

Module pieces (each `#[non_exhaustive]` where growth-prone):

- `pub struct SolicitorConfig { min_lesson_age: Duration, min_signal_count: u32, signal_window: Duration, max_candidates_per_call: usize }` with `Default` = `(7 days, 1, 14 days, 1)`. Rationale: rule 8 reactance ceiling = max 1 candidate per call; `min_signal_count = 1` because today `external_signal_sources` is a Set-semantics array (Day 17 Q7 keeps that shape).
- `pub struct StaleLesson { lesson_id, status_dir, age: Duration, signal_count: u32, reason: StaleReason }`.
- `pub enum StaleReason { LowSignalDensity, PromotedButNegative /* Day 18+ */ }`.
- `pub struct SolicitorOutput { stale_candidates: Vec<StaleLesson> }`.

Function body (sketch):

```rust
pub async fn solicit_stale_lessons(
    ctx: &Context, storage: &dyn Storage,
    config: &SolicitorConfig, now: DateTime<Utc>,
) -> Result<SolicitorOutput, EngineError> {
    let mut candidates = Vec::new();
    for status in &["active", "promoted"] {
        let prefix = StorageKey::lessons_status_prefix(ctx, status); // new helper
        for key in storage.list(&prefix).await? {
            let Some(lesson) = lessons::get_by_id_via_key(ctx, storage, &key).await? else { continue };
            let Some(stale) = evaluate_staleness(&lesson, config, now) else { continue };
            candidates.push(stale);
            if candidates.len() >= config.max_candidates_per_call {
                return Ok(SolicitorOutput { stale_candidates: candidates });
            }
        }
    }
    Ok(SolicitorOutput { stale_candidates: candidates })
}

fn evaluate_staleness(lesson: &LoadedLesson, cfg: &SolicitorConfig, now: DateTime<Utc>) -> Option<StaleLesson> {
    let created_at = DateTime::parse_from_rfc3339(&lesson.frontmatter.created_at).ok()?.with_timezone(&Utc);
    let age = (now - created_at).to_std().ok()?;
    if age < cfg.min_lesson_age { return None; }
    let signal_count = lesson.frontmatter.external_signal_sources.len() as u32;
    if signal_count >= cfg.min_signal_count { return None; }
    Some(StaleLesson { lesson_id: lesson.frontmatter.id.clone(), status_dir: lesson.status_dir.clone(),
        age, signal_count, reason: StaleReason::LowSignalDensity })
}
```

Scans only `active/` + `promoted/`: `pending/` is too fresh, `discarded/` + `superseded/` are out of play.

### Host-version tripwire (separate free function — Q3 below)

The tripwire is its own concern (event-driven, not time-driven). Sketch in Q3.

### Tests for solicitor (~5 tests)

1. `solicit_finds_zero_candidates_when_all_lessons_fresh` — seed lessons with `created_at = now - 1 day` and `min_lesson_age = 7 days`; expect empty output.
2. `solicit_returns_low_signal_density_candidate` — seed lesson aged 10 days with 0 signals; expect one candidate with `LowSignalDensity`.
3. `solicit_respects_max_candidates_per_call` — seed 5 stale lessons; with `max_candidates_per_call = 1`, expect output.len() == 1.
4. `solicit_skips_pending_and_discarded_dirs` — seed stale lessons in `pending/`, `discarded/`, `superseded/`; expect empty output (only active + promoted scanned).
5. `solicit_returns_engine_err_on_storage_failure` — `FaultyMemoryStorage` returning `StorageError` from `list`; expect `Err(EngineError::Storage(_))`.

### Trade-offs

Function-on-demand (chosen — engine-doesn't-own-executor invariant + composability + testability) over: spawned task (lifecycle complexity, executor coupling), stream consumer (wrong shape — time-driven not event-driven), tower::Service (rejected Day 16 Q4).

### Audit smells (solicitor-specific)

- **S44 — Solicitor owns its own `tokio::time::Interval` + `CancellationToken`.** Wrong shape; engine doesn't own its executor. If the solicitor exposes a `spawn_solicitor` convenience, that helper lives in `lifecycle.rs` not the engine sentiment module.
- **S45 — `solicit_stale_lessons` taking `SolicitorConfig` by value not by reference.** Cheap config, but the function is called every N seconds in production; `&SolicitorConfig` is the discipline.
- **S46 — Mixing `Instant` (orchestrator) and `DateTime<Utc>` (solicitor) wall-clock representations without justification.** The orchestrator uses `Instant` (monotonic, can't go backwards, but doesn't survive restart). The solicitor uses `DateTime<Utc>` (calendar-time, survives restart). They serve different invariants. Document this.
- **S47 — Solicitor reading lesson YAML directly via storage::get + manual YAML split instead of routing through `lessons::get_by_id`.** Lessons-layer logic must stay in lessons-module. Solicitor calls the lesson loader; it doesn't reimplement it.
- **S48 — `tokio::spawn` inside solicitor module.** Anywhere. The engine doesn't `tokio::spawn`. (Audit Day 16a's orchestrator also has no spawn; we extend the invariant.)

---

## Q2: Stale-lesson detection algorithm

### Signal density: what does it mean?

`sentiment-design-rules.md` references two signal types:

- Rule 7: "`sum(strength) ≥ 0.75` budget for `external_signal_sources`"
- Evaluation strategy line 67: "Co-occurrence proxy: sentiment-positive + later thumbs-up within 14 days = TP"

After Day 16b: signals are stored as standalone YAML files at `signals/<session>/<event>.yaml`. After Day 17 (L4 + lesson-array aggregation): signals get appended to `lesson.frontmatter.external_signal_sources` (the existing string array, kept for TS-compat).

**Decision:** Day 17 solicitor uses `external_signal_sources.len()` as the signal-density proxy, deferring "weighted-by-recency" until Day 18+ (when per-signal timestamps exist on disk).

Tradeoff: a lesson with 100 old signals + 0 new ones looks fresh. Acceptable for Day 17 because (a) lessons are created at most every few days, so 100 signals on a fresh lesson is implausible; (b) Day 17 is a coarse-grained pass.

### Lesson age — filesystem birthtime vs `frontmatter.created_at`

Day 14 anti-self-grading gate uses filesystem birthtime. Why?

Anti-self-grading rule: the daemon must NOT grade a lesson it wrote ≤24 hours ago, because the daemon's signal-generation may be biased by the lesson body. The 24-hour threshold matches "promotion-eligible-after." Birthtime is harder to forge than `frontmatter.created_at` (which is a string field the daemon writes).

For the solicitor:
- The solicitor isn't grading; it's surveying. The bias concern doesn't apply.
- `frontmatter.created_at` is the right field because it's deterministic across cross-process writes, survives backups, and matches the TS-side `creation_at` for cross-impl test parity.
- Birthtime would be a Linux/macOS-specific syscall (`statx` / `creation_time`), adding platform complexity.

**Decision:** Solicitor uses `frontmatter.created_at` for age. The anti-self-grading gate is a separate concern (gate, not solicitor).

### Decision: data shape recommendation

```rust
pub struct StaleLesson {
    pub lesson_id: String,
    pub status_dir: String,
    pub age: Duration,
    pub signal_count: u32,
    pub reason: StaleReason,
}

pub enum StaleReason {
    LowSignalDensity,
    PromotedButNegative,  // Day 18+
}
```

`#[non_exhaustive]` on the enum + the struct (Day 14 D2 pattern).

### Implementation note — new StorageKey constructor needed

The solicitor needs `Storage::list(prefix)` over `lessons/active/`. Current `StorageKey::lesson(ctx, status, id)` doesn't generate a status-prefix key (it always includes a lesson id). Day 17 adds:

```rust
impl StorageKey {
    /// Prefix for all lessons in a given status dir. Used by Storage::list.
    pub fn lessons_status_prefix(ctx: &Context, status: &str) -> Self {
        Self(prefixed(ctx, &format!("lessons/{status}/")))
    }
}
```

The trailing slash matters for `MemoryStorage::list`'s `starts_with` prefix-match. Tested.

### Tests (in addition to solicitor tests above)

1. `evaluate_staleness_returns_none_when_lesson_is_young` — age < min_lesson_age.
2. `evaluate_staleness_returns_none_when_signal_count_meets_threshold` — signals.len() >= min.
3. `evaluate_staleness_uses_created_at_not_updated_at` — set `updated_at = now`, `created_at = now - 10 days`; expect age = 10 days.
4. `evaluate_staleness_returns_none_for_malformed_created_at` — `created_at = "garbage"`; expect None (defensive, don't error the whole solicitor pass on one bad lesson).

### Trade-offs

`frontmatter.created_at` (chosen — TS-compat, deterministic, schema-friendly) over: filesystem birthtime (platform-specific syscall), filesystem mtime (changes on every signal write — wrong semantics).

### Audit smells

- **S49 — Solicitor panicking on `created_at` parse failure.** Use `.ok()?` early-return per the sketch.
- **S50 — Counting signals via `external_signal_sources.iter().filter(|s| s.starts_with("sentiment_"))` and missing other signal types.** Use `.len()` directly — all signals in the array are tracked regardless of source. The threshold semantics is "total signals" not "sentiment signals."

---

## Q3: HostVersion tripwire

### Source of "tested range"

Three options:

#### Option A — Static const in source

```rust
const TESTED_RANGE: HostVersionRange = HostVersionRange::exact("2.1.139");
```

Pro: zero config. Con: requires recompile to bump.

#### Option B — Config file (`~/.loop/config.yaml`)

```yaml
host_compat:
  claude_code:
    tested_versions: ["2.1.139", "2.1.140"]
```

Pro: hot-swappable. Con: another config-file concern.

#### Option C — Runtime probe

The daemon launches `claude --version` at startup, records, asserts on first observed event.

Pro: zero config. Con: shells out, fragile, claude-code-specific.

### Recommendation

**Option B — config file**, but read it once at daemon startup and pass into a `HostVersionPolicy` struct that the engine accepts at orchestrator-construction time. Falls back to a built-in default if config absent.

```rust
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HostVersionPolicy {
    /// Versions explicitly tested. Empty = no tripwire (warn-only on
    /// unrecognized).
    pub tested_versions: Vec<HostVersion>,
    /// What to do on mismatch.
    pub on_mismatch: VersionMismatchAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VersionMismatchAction {
    Warn,      // log + continue (default)
    Abstain,   // suppress signal emission for that turn
    Exit,      // shut the daemon down
}

impl Default for HostVersionPolicy {
    fn default() -> Self {
        Self {
            tested_versions: vec![HostVersion::new("2.1.139")],
            on_mismatch: VersionMismatchAction::Warn,
        }
    }
}

pub fn is_host_version_in_tested_range(
    observed: &HostVersion,
    policy: &HostVersionPolicy,
) -> bool {
    policy.tested_versions.iter().any(|v| v == observed)
}
```

### Tripwire firing — what's the action?

Per `sentiment-design-rules.md` Hard rules line 4: "Default = abstain. Mis-attribution is more expensive than silence."

If the daemon is observing an UNTESTED claude-code version, we don't know if the JSONL schema has shifted under us. Wrong polarity from a parse drift is exactly the kind of mistake the rule guards against.

**Decision:** Default action = `Warn` (log + continue). Production deployment can flip to `Abstain` once we have a few weeks of unknown-version data. `Exit` is reserved for known-incompatible versions (added by config later).

### Where the tripwire fires

NOT in the solicitor. NOT in `EventSource::translate`. Fires inside `Orchestrator::handle_user_turn` BEFORE the classifier call. Branches on `policy.on_mismatch`: `Warn` logs + continues; `Abstain` short-circuits with new `AbstainReason::UntestedHostVersion`; `Exit` logs error + abstains (engine never exits — host decides). New `AbstainReason::UntestedHostVersion` variant added (non-breaking; enum is already `#[non_exhaustive]`).

### Adding `host_version_policy` to OrchestratorConfig

`OrchestratorConfig` (`src/engine/sentiment/orchestrator/config.rs`) grows a `host_version_policy: HostVersionPolicy` field with `Default::default()`.

### Tests

1. `is_host_version_in_tested_range_matches_exact` — `["2.1.139"]` vs `"2.1.139"` → true.
2. `is_host_version_in_tested_range_rejects_unknown` — `["2.1.139"]` vs `"3.0.0"` → false.
3. `is_host_version_in_tested_range_with_empty_policy_is_permissive` — `[]` returns true (no tripwire if no tested versions configured).
4. `orchestrator_warn_action_continues_to_classifier` — orchestrator integration test confirms classifier still called.
5. `orchestrator_abstain_action_skips_classifier` — orchestrator integration test confirms classifier NOT called + abstain reason returned.

### Trade-offs

Config-driven policy with default (chosen — composes with future SaaS where versions get bumped weekly without recompile) over: static const (recompile-to-bump), runtime probe (process-shell fragility).

### Audit smells

- **S51 — Tripwire firing in `EventSource::translate`.** Wrong layer; translation doesn't know about classifier policy. Lives in orchestrator handler.
- **S52 — `is_in_tested_range` as a method on `HostVersion` that hard-codes the range.** Per the existing comment on `HostVersion`, the *type* shouldn't carry policy; policy lives in `HostVersionPolicy`. Method-on-type is the wrong shape.

---

## Q4: Lessons migration — order, ENV_LOCK retirement, test rewrites

### Order (leaf-first per Day 14 D8)

| Step | Module | LOC est. | Verification |
|---|---|---|---|
| 1 | `lessons/loader.rs` async `get_by_id(&ctx, &dyn Storage, id)` + delegating wrapper for `get_lesson_by_id` | ~80 prod + ~30 test rewrites | All loader tests pass under TestHarness |
| 2 | `lessons/signals.rs` async `record_sentiment_signal(&ctx, &dyn Storage, id, polarity)` with bounded 5-retry CAS loop + delegating wrapper | ~150 prod (CAS loop + helpers) + ~80 test rewrites | All signals tests pass under TestHarness |
| 3 | `lessons/lock.rs` → 4-line re-export of `storage::lock::with_sidecar_lock` (audit M1 + audit m6 fix) | -90 prod (delete duplicate) + 0 test changes (storage/lock.rs already has the tests) | All existing callers compile |
| 4 | `StorageBackedSignalWriter` migration: append to lesson `external_signal_sources` array | ~60 prod + ~3 test additions | Lesson-array aggregation verified (Q7 below) |
| 5 | `lessons::get_by_id_via_key` helper for solicitor (loads by StorageKey directly, skipping the 5-status scan) | ~30 prod + 2 tests | Solicitor list-and-load path |

### Step 1: `lessons/loader.rs`

New API: `pub async fn get_by_id(ctx: &Context, storage: &dyn Storage, id: &str) -> Result<Option<LoadedLesson>, EngineError>`. Iterates `paths::LESSON_STATUS_DIRS`, calls `storage.get(&StorageKey::lesson(ctx, status, id))`, parses bytes via `std::str::from_utf8` + `split_frontmatter_normalized` + `parse_lesson_frontmatter` (each error mapped via `EngineError::yaml`). Plus a `get_by_id_via_key(ctx, storage, &StorageKey)` variant for the solicitor (skips the 5-status scan).

**Field change:** `LoadedLesson::path: PathBuf` becomes `key: StorageKey`. Breaking; the legacy sync wrapper returns a `LegacyLoadedLesson` newtype that retains `path: PathBuf` for `tests/concurrent_signal_writes.rs` (retires Day 18). The wrapper uses `tokio::runtime::Handle::try_current()` + fallback `Builder::new_current_thread()` for sync-from-async safety (S53). The wrapper carries `#[deprecated(since = "0.0.1", note = "use get_by_id — retires Day 18")]`.

Alternative considered: keep both `path` AND `key` on `LoadedLesson`. Rejected — dual maintenance + ambiguous source-of-truth (S55).

### Step 2: `lessons/signals.rs`

New API: `pub async fn record_sentiment_signal(ctx, storage, id, polarity) -> Result<LoadedLesson, EngineError>`. Bounded 5-retry CAS loop:

```rust
const MAX_RETRIES: u32 = 5;
for retry in 0..=MAX_RETRIES {
    let (bytes, version) = storage.get_with_version(&key).await?
        .ok_or_else(|| EngineError::LessonNotFound { id: id.into() })?;
    let updated = apply_sentiment_signal(parse_loaded_lesson(&key, &status, &bytes)?, polarity)?;
    let new_bytes = render_lesson_bytes(&updated)?;
    if storage.put_if_version(&key, new_bytes, Some(&version)).await? {
        return Ok(updated);
    }
    if retry == MAX_RETRIES {
        return Err(EngineError::CasContended { key: key.as_str().into(), retries: MAX_RETRIES });
    }
    // No sleep — flock serializes; hot retry bounded by MAX_RETRIES.
}
unreachable!()
```

The retry loop uses `EngineError::CasContended` (declared Day 16b error.rs:43-44 with no callers per audit m3; Day 17 fills it). Sync wrapper `record_sentiment_signal_sync` carries `#[deprecated]` until Day 18.

### Step 3: `lessons/lock.rs` retirement

Currently 210 lines of duplicate sidecar-flock logic + 5 tests. Day 16b lifted the helper into `storage/lock.rs` but didn't retire the old copy (audit m6 + post-research L2).

Day 17 collapses to:

```rust
//! Sidecar-flock helper. Retired Day 17 — re-exports from `storage::lock`
//! for any pre-Day-16b caller. Deprecated; remove Day 19+.
#[deprecated(since = "0.0.1", note = "use crate::engine::storage::lock::with_sidecar_lock")]
pub use crate::engine::storage::lock::{sidecar_lock_path, with_sidecar_lock as with_lock};
```

The 5 tests in `lessons/lock.rs` get **deleted**, not migrated — `storage/lock.rs` already has equivalents post-audit-M1-fix. Net: -200 LOC delete; +5 LOC re-export.

This also addresses **audit M1** (the `lock_survives_target_rename` regression test port that audit recommended — already done in `storage/lock.rs:143` per audit-fix in 16b learn-notes per inspection).

### Sync wrapper retirement: Day 17 vs Day 18 question

**Decision: KEEP sync wrappers in Day 17 with `#[deprecated]` attribute.**

Reasons:
1. `tests/concurrent_signal_writes.rs` still uses `record_sentiment_signal(id, polarity)` (sync, by-id only). Migrating that test requires async test harness changes (currently uses raw `thread::spawn`); deferring the migration to Day 18 keeps Day 17's audit surface bounded.
2. `#[deprecated]` produces compiler warnings → visible signals that Day 18 must close.
3. Day 16b D4 already deferred sync wrappers; chaining one more cycle keeps the cycle bandwidth balanced.

Day 18 retires sync wrappers + migrates `tests/concurrent_signal_writes.rs`.

### Test migration scope

| Test file | Today | After Day 17 |
|---|---|---|
| `src/engine/lessons/loader.rs::tests` (7 tests) | `with_temp_loop_home` + `ENV_LOCK` | `TestHarness::on_disk()` |
| `src/engine/lessons/signals.rs::tests` (8 tests) | `with_temp_loop_home` + `ENV_LOCK` | `TestHarness::on_disk()` |
| `src/engine/lessons/lock.rs::tests` (5 tests) | TempDir, no ENV_LOCK | DELETED (covered by storage/lock.rs equivalents) |
| `src/engine/paths.rs::tests` (2 tests) | `ENV_LOCK` (env mutation) | STAYS — env-var test cannot avoid env mutation |
| `src/engine/lifecycle.rs::tests` (2 tests using ENV_LOCK) | `ENV_LOCK` (env mutation) | STAYS — env-var test cannot avoid env mutation |
| `tests/concurrent_signal_writes.rs` (1 integration test) | sync API + thread::spawn | STAYS for Day 17, migrates Day 18 |

**Net ENV_LOCK churn:** 10 → 5 callers (paths × 2 + lifecycle × 2 + concurrent_signal_writes file-internal mutex = 4-5).

The `paths.rs` and `lifecycle.rs` tests use `ENV_LOCK` because they directly mutate `LOOP_HOME_ENV` to test path resolution. That's fundamentally what they're testing; no harness can abstract it. **ENV_LOCK lives forever** (unless Day 18+ finds a way to dependency-inject `loop_home()`, which is out of scope).

### Commit cadence (Day 17 build phase)

| # | Commit | LOC est. | Verifies |
|---|---|---|---|
| 1 | `engine/test_support.rs` ships (Q5) | ~150 prod + 0 test rewrites (just landing the type) | New file compiles; existing tests untouched |
| 2 | `lessons/loader.rs` async migration + test rewrites | +80 prod / -30 prod (sync delete) + 7 test rewrites | All loader tests green; new `get_by_id` tested via TestHarness |
| 3 | `lessons/signals.rs` async migration + bounded CAS + test rewrites | +150 prod / -80 prod (sync delete) + 8 test rewrites | All signals tests green; CAS-contention test green |
| 4 | `lessons/lock.rs` retirement (re-export) | -200 prod + 0 test rewrites (delete tests) | All existing callers compile |
| 5 | `engine/sentiment/solicitor.rs` lands | +200 prod + 5 tests | Solicitor unit tests green |
| 6 | HostVersion tripwire in `OrchestratorConfig` + orchestrator handler | +40 prod + 5 tests | Tripwire abstain-path test green |
| 7 | Lesson-array signal aggregation in `StorageBackedSignalWriter` (Q7) | +60 prod + 3 tests | Aggregation round-trip green |
| 8 | `main.rs` orchestrator stub wiring (Q8) | +80 prod + 0 tests | `cargo build --release` succeeds |
| 9 | `tests/orchestrator_e2e.rs` integration test (Q6) | +200 test LOC | Full pipeline smoke green |

**Total: ~960 LOC across 9 commits.** Each commit `cargo test --all-features` green at HEAD. Audit surface is bounded because commits 1-4 are migration mechanics (low net new LOC; high test churn) and commits 5-9 are net-new feature LOC.

### Trade-offs

Three-commit migration mechanics (chosen — leaf-first per D8; each commit revertable independently) over: big-bang single-commit migration (audit risk, one bug = full revert).

### Audit smells

- **S53 — Sync wrapper that calls `tokio::runtime::Handle::block_on` from inside another async context.** Deadlocks. Use `try_current()` + fallback to a new current-thread runtime when outside async. The pattern is fiddly; ensure it's actually called from sync test/binary code only.
- **S54 — Async wrapper that reads bytes, parses YAML, modifies, then writes — without a CAS loop.** Lost-update vulnerability. The CAS loop in Step 2 is what gives the new API its correctness guarantee.
- **S55 — `LoadedLesson::path` retained alongside `LoadedLesson::key`** (both fields). Forces dual maintenance + ambiguous source-of-truth. Use the wrapper newtype approach.

---

## Q5: TestHarness implementation

### Final shape (per Day 16b pre-research Q6, refined)

`src/engine/test_support.rs`, gated `#![cfg(any(test, feature = "test-fixtures"))]`:

```rust
pub struct TestHarness {
    pub ctx: Context,
    pub storage: Arc<dyn Storage>,
    _tempdir: Option<TempDir>,  // declared AFTER storage — drop order matters
}

impl TestHarness {
    pub fn in_memory() -> Self { /* MemoryStorage; tempdir = None */ }
    pub fn on_disk() -> Self { /* TempDir::new() + LocalFsStorage::new(td.path()) */ }
    pub fn in_memory_for_tenant(tenant: &str, user: &str) -> Self { /* multi-tenant Context */ }
    pub async fn seed_lesson(&self, status: &str, id: &str) -> StorageKey {
        self.seed_lesson_with_body(status, id, "test body\n").await
    }
    pub async fn seed_lesson_with_body(&self, status: &str, id: &str, body: &str) -> StorageKey {
        let fm = minimum_frontmatter(id);
        let contents = combine_frontmatter(&serialize_lesson_frontmatter(&fm), body);
        let key = StorageKey::lesson(&self.ctx, status, id);
        self.storage.put(&key, Bytes::from(contents.into_bytes())).await.expect("seed_lesson put failed");
        key
    }
}
```

`minimum_frontmatter(id)` returns a `LessonFrontmatter` with sensible defaults (active status, fixed `created_at`, empty everything else).

### Sub-question answers

#### Drop semantics — does the TempDir die when TestHarness drops?

Yes. `TempDir` implements `Drop` by recursive directory removal. Field declaration order is `ctx → storage → _tempdir`; Rust drops fields in declaration order (first field dropped first per RFC 1857). So `storage: Arc<dyn Storage>` drops before `_tempdir`. If a test held the harness and forgot to drop the storage Arc separately, the inner `LocalFsStorage` would still get dropped because the harness owns the only Arc clone (unless the test cloned it for tasks).

**Decision check:** TempDir cleanup works correctly even when LocalFsStorage doesn't close any persistent file handles (it doesn't — `tokio::fs::*` opens-and-closes per call). No special drop-order discipline needed.

#### Async constructor for `in_memory`?

NO. `MemoryStorage::default()` is sync. `LocalFsStorage::new(path)` is sync. Both constructors should be sync to avoid forcing tests to be `#[tokio::test]` just to construct a harness — many pure-logic tests don't need async at all.

Tests that need async (anything calling `seed_lesson` or `Storage::*`) use `#[tokio::test]` already.

#### `seed_lesson` async?

YES. It calls `Storage::put`, which is async. Returns the `StorageKey` for chained assertions.

### Drop-side-effects (audit smell S39 from Day 16b)

Day 16b audit-smell S39 warned: "TestHarness Drop-side-effects." The concern: if `Drop` for TestHarness tried to do async cleanup, we'd have a Tokio-runtime-not-running panic. Our design has NO custom Drop — TempDir's Drop is sync (rm -rf), Arc's Drop is reference-counting (sync), Context's Drop is dropping Arc<str> (sync). All safe.

### Migration to TestHarness — tests touched in Day 17

Total: 15 tests across two files (loader + signals). See Q4 table above. Rewrite pattern: replace `with_temp_loop_home(|tmp| { write_lesson(tmp, status, id, ...); /* call sync API */ })` with `let h = TestHarness::on_disk(); h.seed_lesson(status, id).await; /* call async API with &h.ctx, h.storage.as_ref() */`. The new tests are `#[tokio::test]` async fns, run in parallel, no global env mutation.

### Tests on TestHarness itself

1. `harness_in_memory_seeds_lesson_round_trip` — seed via harness, retrieve via storage; assert content matches.
2. `harness_on_disk_creates_tempdir_under_path` — seed via harness; assert TempDir's path exists and lesson is at the expected sub-path.
3. `harness_drop_cleans_up_tempdir` — drop harness, assert TempDir path no longer exists.
4. `harness_multi_tenant_routes_through_prefixed_key` — `in_memory_for_tenant("acme", "alice")`; assert key prefix is `tenants/acme/users/alice/...`.

### Trade-offs

Single struct + two constructors (chosen — D14 D7 + D16b OQ5) over: typestate-encoded variant (`InMemoryHarness` / `OnDiskHarness` — type explosion), trait-based (`Harness::storage()` — overkill for two impls), builder (`HarnessBuilder::memory().build()` — boilerplate for two-option choice).

### Audit smells

- **S56 — TestHarness with `Drop` impl that does async work.** Don't. Use sync-only Drop (TempDir handles it).
- **S57 — `_tempdir: Option<TempDir>` declared BEFORE `storage`.** Wrong drop order — storage might reference the temp path during its own drop. Declare storage first.
- **S58 — `seed_lesson` that ignores write failure** (e.g. `let _ = self.storage.put(...)`). Use `.expect("seed_lesson: storage put failed")` — test setup failures should panic visibly.

---

## Q6: Engine-level integration test design

### Recommended file structure

Add `tests/orchestrator_e2e.rs` with two end-to-end scenarios. Use the existing `loop-daemon` self-reference with `features = ["test-fixtures"]` (already in `Cargo.toml`).

#### Scenario 1: `MockClassifier → Orchestrator → StorageBackedSignalWriter → MemoryStorage`

Drives the engine spine without filesystem. Construct `Arc<MemoryStorage>`, `Arc<MockSentimentClassifier::default().with_response(positive_canned_response)>`, `Arc<StorageBackedSignalWriter::new(storage))`, then `Orchestrator::new(...)`. Call `orch.update_manifest(&session, vec![LoadedItem{...}])` to seed; then `orch.process_event(&ctx, &UserTurn_event).await`. Assertions: `out.signals.len() == 1`, plus `storage.get(&StorageKey::sentiment_signal(&ctx, session, evt_uuid)).await?.expect(...)` contains `item_id: les-quokka-special` and `polarity: Positive`. ~80 LOC test.

#### Scenario 2: `JsonlWatcherSource → Orchestrator → MemoryStorage` (L4-deferred smoke)

End-to-end test that actually drives the watcher. Construct a TempDir for the watch path; construct `JsonlWatcherSource::new(dir)`; call `source.run(&ctx, shutdown).await` to get the BoxStream; write a synthetic UserTurn JSONL line to `dir/<session>.jsonl`; consume the stream with `tokio::time::timeout(stream.next(), Duration::from_secs(2))` until a `UserTurn` event arrives (skip the leading `SessionStarted` per Day 13 audit A5); seed orchestrator manifest; call `orch.process_event(&ctx, &user_turn).await`; assert one signal emitted; `shutdown.cancel()` at test end (S61).

Synthetic JSONL line (Claude Code format, per Day 13 byte_fixture.rs):
```json
{"sessionId":"sess-e2e","uuid":"evt-1","parentUuid":null,"cwd":"/tmp/test","gitBranch":"main","timestamp":"2026-05-13T18:00:00Z","type":"user","message":{"role":"user","content":"thanks"},"version":"2.1.139"}
```

### Where the test lives

`tests/orchestrator_e2e.rs` — separate from existing `tests/byte_fixture.rs`, `tests/ts_lesson_roundtrip.rs`, `tests/concurrent_signal_writes.rs`. The new file is the engine integration story.

### Synthetic JSONL line shape

The watcher parses Claude Code's JSONL format. Minimum viable line:

```json
{"sessionId":"sess-e2e","uuid":"evt-1","parentUuid":null,"cwd":"/tmp/test","gitBranch":"main","timestamp":"2026-05-13T18:00:00Z","type":"user","message":{"role":"user","content":"thanks"},"version":"2.1.139"}
```

Existing `byte_fixture.rs` integration test (Day 13) shows the exact field names. Reuse its fixtures via a shared `tests/common/` module if it grows; for Day 17, inline a `synthetic_user_turn_jsonl` helper in `tests/orchestrator_e2e.rs`.

### Driving `update_manifest` + `push_assistant_turn` from the test

Both are direct method calls on `Orchestrator`. The test seeds state by calling them before `process_event`. The orchestrator integration tests inside `src/engine/sentiment/orchestrator/mod.rs:443+` already follow this pattern; the integration tests are the same shape, just at the `tests/*.rs` layer.

### Tests count

2 scenarios. Adding more (e.g., correction-window flow, abstain-on-hazard) would duplicate existing inline orchestrator tests; keep `tests/orchestrator_e2e.rs` lean.

### Trade-offs

Two scenarios — one filesystem-free + one filesystem-driven (chosen — covers both the "engine spine" and the "watcher integration" sides of the deferral) over: a single scenario (insufficient coverage), 5+ scenarios (duplicates orchestrator inline tests).

### Audit smells

- **S59 — Integration test that asserts filesystem state without TempDir RAII.** Use TestHarness; let TempDir clean up on drop.
- **S60 — JsonlWatcher test that polls with `thread::sleep` for events.** Use `tokio::time::timeout(stream.next())` instead — bounded wait, no busy-loop, fails clearly on timeout.
- **S61 — Integration test that asserts signal exists then exits without `shutdown.cancel()`.** Leaks a watcher task. Cancel the shutdown token at test end.

---

## Q7: Signal-array aggregation (Day 16b L4 deferral)

### Background

Day 16b emits one YAML file per signal at `signals/<session>/<event>.yaml`. The lesson's `external_signal_sources` array (where TS-side persists the same info) is NOT updated.

### Where aggregation happens

Three options:

#### Option A — Write-time (in `StorageBackedSignalWriter::record`)

`StorageBackedSignalWriter::record` does two storage operations in sequence: (1) call `lessons::record_sentiment_signal(ctx, storage, id, polarity)` (which runs the CAS-loop and appends to `external_signal_sources`); (2) write the standalone signal file at `StorageKey::sentiment_signal(...)` create-only. `Polarity::Neutral` returns `Ok(())` early (defensive — orchestrator should abstain).

Pro: orchestrator emits → lesson reflects in single async call. Idempotent (CAS dedup on Set semantics; standalone file is create-only).
Con: write-amplification (one logical signal → two storage operations); standalone signal file becomes ledger, lesson is materialized view.

#### Option B — Read-time (separate aggregator reads signals/* on demand)

Pro: signal write is fast (one storage op).
Con: every reader needs to scan signals/*; lesson YAML drifts from authoritative state; cross-impl parity with TS breaks (TS appends to lesson).

#### Option C — Drop the standalone signal file (only append to lesson)

Pro: simplest; matches TS.
Con: loses the per-event audit trail Day 16b deliberately captured.

### Recommendation

**Option A — both: standalone signal file (for audit/replay) + lesson-array append (for read-time aggregation + TS-compat).**

This is the "lesson is the materialized view; signals are the ledger" pattern. The standalone signal file is a fact; the lesson is a summary. Replay-from-signals could reconstruct the lesson on its own — useful for verification and debugging.

### Migration story for Day 16b-emitted standalone files

Day 16b emitted standalone signal files at `signals/<session>/<event>.yaml`. Day 17 keeps writing them AND adds the lesson append. Day 17 does NOT backfill the lesson from existing standalone files — that's a separate Day 18+ migration if needed.

### Schema for the lesson YAML signals array

Existing `external_signal_sources: Vec<String>` — strings like `sentiment_positive`, `sentiment_negative`, `user_thumbs_up`. This matches TS-side.

**Decision:** keep the existing string-array shape. The `record_sentiment_signal` function in Step 2 of Q4 already maps Polarity → string. No schema change.

Future: a `signal_evidence: Vec<SignalEvidence>` array with `{ method, hazards, confidence, source_event_uuid }` per signal — Day 18+ enhancement (per Day 16b L3).

### Tests

1. `signal_writer_appends_to_lesson_external_signal_sources` — seed a lesson via TestHarness; orchestrator emits one signal; assert lesson `external_signal_sources` contains "sentiment_positive".
2. `signal_writer_is_idempotent_on_duplicate_event_uuid` — emit same signal twice (same event_uuid); assert lesson has "sentiment_positive" exactly once AND standalone file content matches first write.
3. `signal_writer_keeps_both_signal_file_and_lesson_array` — verify both `signals/<sess>/<evt>.yaml` exists AND lesson YAML contains "sentiment_positive".

### Trade-offs

Both standalone file + lesson append (chosen — preserves audit trail + cross-impl parity) over: only lesson append (loses ledger), only signal file (TS-compat regression).

### Audit smells

- **S62 — Lesson-append CAS loop that doesn't bound retries.** Already addressed: `lessons::record_sentiment_signal` has a 5-retry cap (Q4 Step 2).
- **S63 — Write-amplification doubling storage calls without tracking it in observability.** Fine for Day 17 (we don't have metrics yet), but flag for Day 18+ when metrics land.
- **S64 — Signal-file write failure silently dropped** via `let _ = ...`. The lesson append IS the critical write (TS-compat); signal-file is supplementary. But silent-drop loses observability. Use `tracing::warn!(...)` on the failed signal-file write.

---

## Q8: main.rs orchestrator stub wiring

### Goal

Make `cargo build --release` produce a binary that constructs all the Day 17 engine types — even if it doesn't yet have a real classifier. Wire-up readiness check, not a production-ready daemon.

### Approach

Add an Orchestrator construction in `lifecycle::run_body` BEHIND A CONFIG FLAG `DaemonConfig::enable_sentiment_stub: bool` (default `false`). The wiring helper `wire_engine_stub(cfg, shutdown).await?` returns an `EngineHolder` struct holding `_orchestrator`, `_storage`, `_ctx` (all underscored — held-for-lifetime, not read). The holder lives for the duration of `heartbeat_loop`; drops when the loop exits.

Two cfg-gated impls of `wire_engine_stub`:
- `#[cfg(feature = "test-fixtures")]` — constructs `Orchestrator::new(MockSentimentClassifier, StorageBackedSignalWriter, OrchestratorConfig::default())` with `LocalFsStorage::new(paths::loop_home()?)`.
- `#[cfg(not(feature = "test-fixtures"))]` — returns `anyhow::bail!("enable_sentiment_stub requires test-fixtures feature")`.

This means production builds (without `test-fixtures`) can have `enable_sentiment_stub: false` and never hit the runtime error path. Dev builds with `--features test-fixtures` can flip the flag and smoke-test the wiring.

### Config flag

`DaemonConfig::enable_sentiment_stub: bool` — defaults to `false`. Lives in `src/config.rs`.

### Why behind a feature flag

`MockSentimentClassifier` is gated behind `test-fixtures` (Day 15 D13). Production builds without that feature don't have it, so the wire-up function refuses to construct. This:
1. Prevents accidental production deploy of mock-classifier
2. Documents the intent ("this is a stub — flip on for dev, off for prod")
3. Closes Day 16b "what's still missing" item

### Where the config switch lives

`~/.loop/config.yaml`:

```yaml
enable_sentiment_stub: false  # set to true for local dev; needs --features test-fixtures
heartbeat_interval_secs: 30
```

### Trade-offs

Feature-gated stub wiring (chosen — explicit "this is dev-only" guard + buildable in dev + zero-cost in production) over: always-construct-but-don't-drive (production binary carries dead code), unconditional refuse (can't smoke-test wiring in CI).

### Audit smells

- **S65 — `wire_engine_stub` called when `cfg.enable_sentiment_stub` is true but `test-fixtures` not compiled in.** Need a `cfg_attr` or compile-time check. The `#[cfg(not(feature = "test-fixtures"))]` variant of `wire_engine_stub` returns a runtime error — acceptable, surfaces clearly.
- **S66 — `EngineHolder` with all-leading-underscore fields.** Convention for held-but-not-read. OK; documents that this is structural.
- **S67 — Stub wiring that DOES drive event source.** No — the EventSource needs `JsonlWatcherSource::run` + a consumer loop, which would be production code. Day 17 stops at construction.

---

## Q9: Day 17 audit smells (S44+)

Continuing the smell-list from prior cycles. Each is a TS-with-Rust-syntax pattern that compiles but isn't idiomatic Rust.

### Solicitor + tripwire

- **S44.** Solicitor owns its own `tokio::time::Interval` + `CancellationToken` — wrong shape; engine doesn't own its executor.
- **S45.** `solicit_stale_lessons` taking `SolicitorConfig` by value — &SolicitorConfig is the discipline.
- **S46.** Mixing `Instant` (orchestrator monotonic) and `DateTime<Utc>` (solicitor calendar) wall-clock representations without documenting why both exist.
- **S47.** Solicitor reading lesson YAML via `storage::get` + manual YAML split instead of `lessons::get_by_id`.
- **S48.** `tokio::spawn` inside any engine module. Anywhere.
- **S49.** Solicitor panicking on bad `created_at` parse — `.ok()?` early-return.
- **S50.** Counting only sentiment-prefixed signals via string filter instead of array `.len()`.
- **S51.** Host-version tripwire firing inside `EventSource::translate` — wrong layer.
- **S52.** `is_in_tested_range` method on `HostVersion` that hard-codes the range — policy belongs in `HostVersionPolicy`, not the value type.

### Lessons migration

- **S53.** Sync wrapper calling `tokio::runtime::Handle::block_on` from inside an async context — deadlock. Use `try_current()` + fallback runtime.
- **S54.** Async `record_sentiment_signal` without a CAS loop — lost-update window. The 5-retry CAS loop is the correctness gate.
- **S55.** `LoadedLesson` retaining `path: PathBuf` alongside new `key: StorageKey` — dual maintenance + ambiguous source-of-truth.

### TestHarness

- **S56.** TestHarness with `Drop` impl that does async work — runtime-not-running panic.
- **S57.** `_tempdir` declared BEFORE `storage` — wrong drop order; storage might reference temp path during drop.
- **S58.** `seed_lesson` ignoring write failure via `let _ =` — silent test setup failures.

### Integration tests

- **S59.** Integration test asserting filesystem state without TempDir RAII.
- **S60.** Watcher integration test polling with `thread::sleep` — use `tokio::time::timeout(stream.next())`.
- **S61.** Integration test omitting `shutdown.cancel()` at test end — leaks watcher task.

### Signal aggregation

- **S62.** Lesson-append CAS loop without retry bound — already mitigated by Day 16b error::CasContended + 5-retry cap.
- **S63.** Write-amplification (2 storage calls per signal) without observability.
- **S64.** Signal-file write failure silently dropped via `let _ = ...` — use `tracing::warn!`.

### Main.rs wiring

- **S65.** Stub wiring called when `enable_sentiment_stub: true` but `test-fixtures` not compiled — surfaces at runtime; acceptable but ensure clear error.
- **S66.** `EngineHolder` with all-`_`-prefixed fields — convention for held-but-not-read; OK.
- **S67.** Stub wiring that drives an EventSource — should stop at construction only.

### Cargo-public-api snapshot

- **S68.** Day 14 OQ4 promoted `cargo-public-api` from opt-in to gating at Day 17. Day 17 doesn't ship CI gating yet — flag for Day 18 if not done. Migration plan: run `cargo public-api --diff-git-checkouts day-15..day-17` and commit the snapshot diff under `docs/public-api-snapshots/`. Day 18 adds the CI step.

### General

- **S69.** `Vec<Box<dyn Error + Send + Sync>>` returned from a solicitor variant — closed-set enum like `StaleReason` is more precise.
- **S70.** Solicitor as a generic `<S: Storage>` impl — Day 14 D3 chose object-safe `dyn Storage`. Match the existing decision.
- **S71.** Solicitor exposing internal `StaleLesson` fields as `pub` without `#[non_exhaustive]` — growth-prone struct.
- **S72.** `HostVersionPolicy::tested_versions: Vec<String>` — should be `Vec<HostVersion>` for type-safety; comparisons should be `HostVersion == HostVersion`, not string.

### Day 17 audit smells summary table

| Smell | Layer | Severity if missed |
|---|---|---|
| S44 | Solicitor | Major (executor coupling) |
| S45-S50 | Solicitor | Minor (idiom) |
| S51-S52 | Tripwire | Major (policy-in-value-type, wrong layer) |
| S53-S55 | Lessons migration | Major (deadlock, lost-update, dual state) |
| S56-S58 | TestHarness | Minor (subtle drop bugs) |
| S59-S61 | Integration tests | Minor (flaky tests, leaks) |
| S62-S64 | Signal aggregation | Minor (observability) |
| S65-S67 | Main.rs | Minor (config plumbing) |
| S68 | CI | Minor (defer Day 18) |
| S69-S72 | General | Minor (idiom + safety) |

---

## Hard-constraint cross-check

| Constraint | Status |
|---|---|
| NO AGPL/GPL/SSPL deps | ✅ Zero new deps in Day 17 |
| File-size ≤500 prod LOC per file | ✅ Projected largest: `lessons/signals.rs` ~290 prod LOC, `engine/sentiment/solicitor.rs` ~200 prod LOC, `engine/test_support.rs` ~150 prod LOC, `orchestrator/mod.rs` stable at ~440 prod LOC |
| `#[non_exhaustive]` on growth-prone public types | ✅ `SolicitorConfig`, `SolicitorOutput`, `StaleLesson`, `StaleReason`, `HostVersionPolicy`, `VersionMismatchAction`. New `AbstainReason::UntestedHostVersion` variant safe to add (already `#[non_exhaustive]`). |
| Day 14-16b foundations mandatory | ✅ Builds on TestHarness shape (D16b D6), CAS impl (D16b D1), EngineError chassis (D16b D5), StorageKey constructors, OrchestratorOutput shape (D16a D3) |
| Day 17 is FINAL cycle before adapter-discussion pause | ⚠️ Scope concerns — see Q3 above |

---

## Scope-concerns summary

Day 17 has 9 commits projected. Day 16b had 8 planned and shipped 3 ("scope-tightening"). Day 17 risk:

### What MUST land in Day 17 (load-bearing for the cycle close)
1. Lessons migration (Q4 commits 2-4) — UNBLOCKS solicitor, integration tests, AND closes the most prominent Day 16b deferral.
2. TestHarness (Q5 commit 1) — UNBLOCKS the migrated tests.
3. Solicitor (Q1 commit 5) — primary Day 17 deliverable.
4. HostVersion tripwire (Q3 commit 6) — closes Day 15 OQ4 deferral.
5. Integration tests (Q6 commit 9) — closes Day 16a L4 deferral.

### What CAN safely defer to Day 18 / adapter-discussion-pause
1. Sync-wrapper retirement (Q4 — explicit Day 18 placement per Q4 decision)
2. `paths.rs` + `lifecycle.rs` ENV_LOCK retirement (stays forever per Q4 table)
3. `tests/concurrent_signal_writes.rs` migration (Q4)
4. `render_signal_yaml` `{:?}` → `Display` migration (audit m5/L3) — pure refactor, not blocking
5. `cargo-public-api` CI gating (S68) — Day 18

### What COULD defer if scope blows
1. Q7 lesson-array aggregation — defer means StorageBackedSignalWriter still emits only standalone files; lesson YAML stays unchanged from Day 16b. Cross-impl TS parity breaks. **HIGH cost to defer.**
2. Q8 main.rs orchestrator stub — defer means binary still doesn't even compile-check the wiring. **MEDIUM cost to defer** (still no production benefit either way).

**Recommendation:** Keep Q1, Q4 (commits 2-4), Q5, Q6, Q7 as MUST. Q3 tripwire + Q8 main.rs are SHOULD; cuttable if cycle bandwidth tightens. The post-research phase will document any that didn't make it.

### Two-cycle backup plan

If Day 17 audit surface looks heavy on first build pass, split:
- **17a:** Q4 + Q5 + Q1 (migration + TestHarness + solicitor); ~700 LOC
- **17b:** Q3 + Q6 + Q7 + Q8 (tripwire + integration + aggregation + wiring); ~400 LOC

This is the Day 16 → 16a/16b precedent. Pre-research projects single cycle; build phase decides on first build pass.

---

## TL;DR

### Solicitor design (one paragraph)

**Ship the solicitor as a pure async function `solicit_stale_lessons(&ctx, &dyn Storage, &SolicitorConfig, now: DateTime<Utc>) -> Result<SolicitorOutput, EngineError>` plus a free `is_host_version_in_tested_range(&HostVersion, &HostVersionPolicy) -> bool` for tripwire checks fired inside `Orchestrator::handle_user_turn`.** The function-on-demand shape preserves the Day-14 invariant that the engine never owns its executor; the host (today `lifecycle::run_body`; later `main.rs` or CLI or SaaS batch) decides cadence. `now` injected as a parameter means tests don't need `tokio::time::pause()`. Staleness algorithm: filter `lessons/active/` + `lessons/promoted/` by `created_at` age >= 7 days AND `external_signal_sources.len() < min_signal_count` (= 1 by default); return at most 1 candidate per call (sentiment-design-rules rule 8 reactance ceiling). Host-version tripwire policy lives in `HostVersionPolicy` (config-file-driven, default warn-only), NOT as a method on `HostVersion` (S52). New `AbstainReason::UntestedHostVersion` variant added — non-breaking thanks to `#[non_exhaustive]`.

### Migration + test-rewrite plan (one paragraph)

**Three-commit leaf-first migration: (1) `lessons/loader.rs` async `get_by_id(&ctx, &dyn Storage, id) -> Result<Option<LoadedLesson>, EngineError>` with `LoadedLesson::key: StorageKey` replacing `path: PathBuf` (legacy wrapper `LegacyLoadedLesson` newtype preserves PathBuf for Day-18 retirement); (2) `lessons/signals.rs` async `record_sentiment_signal` with the bounded 5-retry CAS loop calling `storage.put_if_version(key, bytes, Some(&version))` and exiting on `EngineError::CasContended { key, retries }` (audit m3 fix — variant gets its first caller); (3) `lessons/lock.rs` collapses to a 4-line re-export of `storage::lock::with_sidecar_lock`, deleting the 200-LOC duplicate + 5 obsolete tests (audit M1 + m6 closed).** `TestHarness` lives at `src/engine/test_support.rs` behind `cfg(any(test, feature = "test-fixtures"))` with sync `in_memory()` + `on_disk()` constructors and async `seed_lesson(status, id) -> StorageKey` helper; field declaration order is `ctx → storage → _tempdir: Option<TempDir>` so the storage Arc releases before TempDir's recursive delete (S57). The 15 tests across loader + signals migrate to TestHarness; ENV_LOCK shrinks from 10 callers to 5 (paths × 2 + lifecycle × 2 + concurrent_signal_writes integration test stay — these test env-var resolution, can't be abstracted away). Sync wrappers stay `#[deprecated]` in Day 17 — final retirement waits for Day 18 along with the concurrent-signal-writes integration test rewrite.

### Scope concerns + push-past-Day-17 deferrals (one paragraph)

**Day 17 projects 9 commits / ~960 LOC — heavier than Day 16b (which scope-tightened from 8 to 3 commits).** The cycle's load-bearing items (lessons migration, TestHarness, solicitor, integration tests, lesson-array aggregation) are mutually dependent: solicitor's tests need TestHarness, TestHarness rewrites need lessons migration, integration tests need both migration AND main.rs stub wiring. **Recommend single-cycle Day 17 with a documented two-cycle backup (17a = migration + TestHarness + solicitor; 17b = tripwire + integration + aggregation + main.rs wiring) decided at first build pass.** Items explicitly deferred to Day 18 / adapter-discussion-pause: (1) sync-wrapper retirement (`get_lesson_by_id`, `record_sentiment_signal_sync` — tied to `tests/concurrent_signal_writes.rs` rewrite); (2) `paths.rs` + `lifecycle.rs` ENV_LOCK callers (env-var-resolution tests; fundamentally can't be harness-abstracted; stay forever); (3) `render_signal_yaml` `{:?}` → `Display` impl (audit m5 / 16b post-research L3 — pure refactor, decoupled from solicitor and integration tests); (4) `cargo-public-api` CI gating (Day 14 OQ4 promotion; S68 — Day 18 adds the CI step once Day 17 baseline lands). The biggest risk for the cycle is Q7 (lesson-array aggregation) — deferring it would break cross-impl TS parity, so it MUST land; if cycle bandwidth tightens, cut Q3 tripwire or Q8 main.rs wiring instead.

---

Related: [[feedback-workflow-cycle]], [[feedback-rust-idiomatic-refactor]], [[feedback-execute-the-plan]], `docs/research/day-16b-post-research.md`, `docs/research/day-16b-audit-report.md`, `docs/research/day-16-pre-research.md`, `docs/research/sentiment-design-rules.md`, `docs/research/day-14-learn-notes.md`.
