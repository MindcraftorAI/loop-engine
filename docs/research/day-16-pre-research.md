# Day 16 Pre-Research: Sentiment Orchestrator + State + Rate Limit + Storage CAS

**Date:** 2026-05-13
**Cycle phase:** Pre-research (workflow cycle phase 1)
**Cycle:** Day 16 — engine sentiment orchestrator, per-session state, per-lesson rate limiting, lessons migration to `Storage::put_if_version`, `JsonlWatcher`→`EventSource` impl
**Toolchain assumed:** Rust 1.85 (MSRV), Cargo 1.95.0, edition = "2021" (Day 14 D9; 2024 bump is a separate audit).
**Inputs:** `docs/research/day-15-{pre,learn,post}-research.md`, `day-15-audit-report.md`, `day-14-{pre,learn,post}-research.md`, `sentiment-design-rules.md`, TS reference `loop-archive-2026-05-13/core-ts/src/sentiment/orchestrator.ts`, current Day 15 code in `src/engine/sentiment/*` and `src/engine/storage/*`, current Day 13 watcher in `src/host/claude_code/jsonl_watcher/*`.

---

## Executive summary

Day 16 has four substantive deliverables that fan out across the engine/host boundary AND across the persistence layer. The estimate from Day 15 post-research (L2) was 1200–1500 LOC plus cross-module test migration; closer reading confirms that and adds a fifth implicit deliverable (`get_with_version` impl is paired with `put_if_version`). One cycle would be feasible but audit-risky — the persistence migration touches 4 modules with 7+ tests and has a cross-process flock-vs-CAS semantics question that wants its own focus.

**Recommendation: split 16a / 16b.**

- **16a (this cycle): orchestrator + `JsonlWatcher` `EventSource` impl + smoke wiring.** Engine sentiment loop end-to-end, in-memory, no persistence side effect. ~600–800 LOC. Audit surface: orchestrator state shape + rate-limit primitive + hazard auto-abstain + correction-window mining + EventSource adapter.
- **16b (next cycle): `LocalFsStorage::put_if_version` + `get_with_version` impls + lessons migration to `Storage` + signal write from orchestrator.** ~500–700 LOC + test migration. Audit surface: cross-process flock-vs-CAS semantics + lessons-test ENV_LOCK retirement + Day 14 stubs replaced.

The split isolates the engine sentiment loop validation from the persistence-layer refactor and avoids merging two independent audit surfaces into one cycle. Per Day 13 audit precedent (5 findings in one cycle = manageable; 12 findings would have been too many), splitting now is cheaper than splitting later.

Key recommendations across the eight questions:

1. **Orchestrator state**: plain `struct SessionState { ... } + enum SessionPhase` inside an `Arc<DashMap<SessionId, Mutex<SessionState>>>` shell. No actor crate, no typestate, no per-session `tokio::spawn`. Idiomatic Rust for "key-keyed mutable state, short critical sections, no fairness requirement." Pattern lineage: `tower-http::trace::TraceLayer` per-request state, `axum::extract::State`, `governor::state::keyed::DashMapStateStore`.
2. **State keying**: keyed on `SessionId` only for 16a. The orchestrator's per-session state is short-lived; cross-tenant routing happens through `Context` at the boundary, not as a state-map key. Day 17+ multi-tenant SaaS keys add `(TenantId, SessionId)` if/when needed.
3. **Rate limiting**: hand-rolled `HashMap<LoadedItemId, Instant>` + `cooldown_duration` per session — embedded in `SessionState`. `governor` is overkill for "≤1 signal per (session, lesson) per 60s" and adds a transitive dep tree. Day 15 attribution kept things minimal; orchestrator follows.
4. **`put_if_version`**: lift the existing `engine::lessons::lock::with_sidecar_lock` pattern into `LocalFsStorage::put_if_version` directly. `Version` encodes `mtime_ns + len` as 24 bytes (16 + 8). `get_with_version` reads the path under the sidecar lock, computes the version from stat, returns both atomically. Sub-question: cross-process flock-vs-CAS — answer: the TS MCP server's flock is on the SAME sidecar inode (lock module documents this), so cross-process serialization holds; CAS is the in-Rust mechanism on top.
5. **Lessons migration**: incremental, leaf-first. `loader::get_lesson_by_id` → new `(ctx, storage)` signature with a delegating wrapper preserving the old call sites for one cycle, then a second-pass commit retires the wrappers. Same pattern Day 14 used. Tests migrate one-module-at-a-time to `TestHarness { ctx, storage: MemoryStorage }`; ENV_LOCK retires when all four modules clean.
6. **`JsonlWatcher`→`EventSource`**: new `JsonlWatcherSource { dir: PathBuf }` struct + `impl EventSource for JsonlWatcherSource`. `run()` spawns the existing `spawn_watcher` internally, bridges its mpsc to a `BoxStream` via `tokio_stream::wrappers::UnboundedReceiverStream`. The existing `spawn_watcher` stays as the internal mechanism; Day 17 audit may retire its `WatcherHandle` public API after callers migrate.
7. **Hazard auto-abstain + correction-window mining**: both are pure-function rules embedded in `Orchestrator::process_user_turn` (correction-window) and the per-item classification filter loop (hazard auto-abstain). No new types, just `match` + small helpers. Audit A2/A3 lineage preserved verbatim from TS.
8. **TS-with-Rust-syntax smells**: 13 new smells specific to orchestrator code (S18–S30) on top of Day 15's 17 + Day 14's 17 + 17 in Day 14 pre-research.

---

## Q1: 16a/16b split decision

**This is the most important deliverable of this pre-research.** State the recommendation and the question allocation up front.

### Recommendation

**Split into 16a (orchestrator + EventSource wiring) and 16b (storage CAS impl + lessons migration + signal write).**

### Rationale

#### The four deliverables in scope

1. **Orchestrator** — per-session state machine, rate limiting, hazard auto-abstain, classifier wiring, correction-window mining. Pure engine code. Estimated 400–600 LOC + ~100–150 LOC tests.
2. **`JsonlWatcher` → `EventSource` impl** — Day 14 deferred; now safe per Day 15 L8. Bridge `WatcherEvent` → `EngineEvent`. Estimated 100–150 LOC + ~50 LOC translation tests + reuse of existing integration tests.
3. **`LocalFsStorage::put_if_version` + `get_with_version` impls** — Day 14 stubs return `Backend(...)` errors today. Lifts the existing `engine::lessons::lock::with_sidecar_lock` pattern. Estimated 150–250 LOC + ~100 LOC regression tests.
4. **Lessons migration to `Storage::put_if_version`** — `lessons/loader.rs`, `lessons/signals.rs`, `lessons/lock.rs` interactions; 4 modules with 7+ tests. Plus the signal-write hook the orchestrator calls. Estimated 250–500 LOC across surgery + test migration.

**Total:** 1200–1500 LOC + cross-module test migration. Day 15 was ~1330 LOC across 5 new files in a single fresh module; that was already at the upper end of a single-cycle audit surface (Day 15 audit found 4 MAJOR + 9 MINOR findings).

#### Why splitting is right

The four deliverables decompose into two independent audit surfaces:

| Surface | Deliverables | Audit concerns |
|---|---|---|
| **Engine sentiment loop end-to-end (16a)** | Orchestrator + EventSource impl | Per-session state shape, rate limit primitive, hazard auto-abstain, correction-window mining, JsonlWatcher → EngineEvent translation, stream shutdown |
| **Persistence migration (16b)** | `put_if_version` + lessons migration | Cross-process flock-vs-CAS semantics, atomic rename + version-check race window, lessons-test ENV_LOCK retirement, Day 14 stubs replaced, signal-write hook |

These are independent: orchestrator state shape doesn't depend on whether signal-write goes through `Storage::put_if_version` or through the existing `lessons::record_sentiment_signal` shim. Orchestrator in 16a calls a `trait SignalWriter` (or just an `Arc<dyn Storage>` + a `LessonWriter` adapter) with a mock impl in 16a tests; 16b replaces the mock with the production write path.

The split also lets 16a deliver an inspectable, testable engine sentiment loop without touching the lessons module's 4-month-old flock + atomic-rename + body-drift code. That code passed Day 12's 127-test correctness audit; modifying it under cycle pressure is exactly when subtle persistence bugs leak in.

#### Why not split differently

- **One cycle covering all four**: feasible but ~1.5× Day 15's audit surface. Two independent audit surfaces sharing one report is harder to read and fix. Day 14 audit and Day 15 audit each ran ~1300 LOC and found 4–5 MAJOR findings; combining doubles the recovery cost if both go yellow.
- **16a = orchestrator only, 16b = EventSource + persistence**: rejected because the JsonlWatcher → EventSource impl is the natural smoke test for the orchestrator. Without it, 16a's orchestrator has no live event source to consume in integration tests — it'd only be testable via `MockEventSource`. The EventSource impl is small (~150 LOC), the integration tests reuse Day 13's existing watcher integration tests with a translation layer, and the audit surface is small (translation correctness, shutdown propagation). It belongs with 16a.
- **16a = persistence, 16b = orchestrator**: rejected because the orchestrator is the consumer of `put_if_version`. Shipping the CAS primitive without a caller leaves it stub-tested (like Day 14's stub-error contract test). Better to ship in service of a real caller.

#### Risk: Day 17 dependency

Day 17 is solicitor work (Phase C; per learn-notes). Solicitor consumes orchestrator output, so 16a must land before 17. 16b is independent of 17 in the same way 16a is — the signal-write path is downstream of orchestrator inference, not of solicitor solicitation. So sequencing is 16a → 16b → 17 OR 16a → 17 → 16b. Either works; recommend 16a → 16b → 17 to keep the persistence migration co-located with its design context.

### Question allocation across cycles

| Question | Cycle | Notes |
|---|---|---|
| Q1 split decision | (this question — answered here) | |
| Q2 state-machine encoding | 16a | Orchestrator state shape |
| Q3 per-session state keying | 16a | Same as Q2 — keyed by SessionId only |
| Q4 rate limiting primitive | 16a | Hand-rolled inside SessionState |
| Q5 `put_if_version` impl | **16b** | Lifts lessons/lock.rs pattern; cross-process semantics |
| Q6 Lessons migration | **16b** | Two-phase, leaf-first; ENV_LOCK retirement |
| Q7 `JsonlWatcher` → `EventSource` | 16a | Bridge `spawn_watcher` mpsc to `BoxStream` |
| Q8 Hazard auto-abstain + correction-window | 16a | Pure rules in orchestrator |

All 8 questions are answered below for completeness; the audit surface for each cycle is the subset above.

---

## Q2: Orchestrator state-machine encoding

### Survey

The orchestrator's mutable surface (per Day 15 OQ-D16-1 plus design rules):

- Recent turn buffer per session (last 4–6 turns, for attribution Pass 2/3 and the classifier's `recent_turns` input)
- Rate-limit timestamps per (session, lesson) — audit-A2 lineage; rule 8 (max 1 solicited / 20 turns) lives in Day 17, but the orchestrator's per-(session, lesson) sentiment-write cooldown is here
- In-flight classifier calls per session (so a slow classifier doesn't get re-fired by the next turn)
- Last-seen sentiment per (session, lesson) for Day 17's solicit-stale-lessons
- Turn count per session (correction-window mining + solicit-stale-lessons cadence)

Options:

#### Option A — plain `struct SessionState + match`

```rust
struct SessionState {
    recent_turns: VecDeque<RecentTurn>,           // bounded ring buffer
    rate_limit: HashMap<LoadedItemId, Instant>,    // per-lesson last-write
    phase: SessionPhase,                           // tiny enum for in-flight vs idle
    turn_count: u64,
}

enum SessionPhase {
    Idle,
    AwaitingClassifier { utterance: String, started_at: Instant },
}
```

Plain mutation under `Mutex<SessionState>`. Critical sections are short — append a turn, check the rate limit, transition the phase. No long-held locks across `.await`.

#### Option B — typestate (`Orchestrator<Idle>`, `Orchestrator<AwaitingClassifier>`)

```rust
struct Orchestrator<P: Phase> { /* state */ _phase: PhantomData<P> }
struct Idle; struct AwaitingClassifier { utterance: String };
impl Orchestrator<Idle> { fn start_classify(self, u: String) -> Orchestrator<AwaitingClassifier> ... }
```

Compile-time enforcement of phase transitions. Pro: invalid transitions don't compile. Con: typestate moves a `self` across each transition; doesn't compose with `Arc<DashMap<SessionId, _>>` (which wants stable types per key) without per-key enums. The "stable storage type per key" + "per-call mutation" combination is the textbook anti-fit for typestate. Typestate shines when the state machine is a single object's lifetime; ours is many objects keyed by session.

#### Option C — actor crate (ractor, actix-flavored, kameo)

```rust
struct OrchestratorActor { state: SessionState }
impl Actor for OrchestratorActor { type Msg = ProcessUserTurn; ... }
```

Per-session actor; each actor has its own mailbox. Pro: natural isolation between sessions; backpressure is "the mailbox." Con: pulls a heavy dep (`ractor` is ~5K LOC + downstream deps), introduces lifecycle complexity (spawn / join / die-on-drop), and the only thing it buys us over `DashMap<SessionId, Mutex<SessionState>>` is "exclusive mutation per key" — which the `Mutex` already gives us. None of the named Rust projects in our reference set (object_store, tower, axum, governor, dashmap) use actor crates for keyed state. Reject.

#### Option D — `enum SessionState { Idle, PendingClassify, RateLimited(Instant), ... }` + plain mutations

Discriminated-union state per session. Same idea as A but with the state encoded as the discriminant. Con: every field of every variant has to be present somewhere; data lives across variants by association. Awkward for "always-present" fields like `recent_turns` and `rate_limit` that aren't phase-dependent. Reject in favor of A (struct with sub-enum for the phase field).

#### Option E — channel-based actor with mpsc (one task per session)

```rust
// host wiring:
for session in active_sessions {
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(orchestrator_loop(rx, ...));
}
```

Per-session `tokio::spawn`. Pro: fan-out across cores; per-task isolation. Con: shutdown is non-trivial (need a per-task `CancellationToken` or sentinel message), spawn-per-session is wasteful (most sessions process one turn per ~30s), task-startup latency adds noise. Doesn't compose with `select_all` consumer at the engine main loop (the consumer is single-task, dispatching via DashMap is simpler than dispatching via channel-of-channels).

### Survey of real Rust crates

- **`tower-http::trace::TraceLayer`**: per-request state lives in an extension on `hyper::Request`, not a keyed map. Closest analog to our orchestrator's "process this event in the context of its session" is `tower::Service::call` itself.
- **`axum::extract::State<S>`**: handler-scoped state; per-request state via `Extension`. Doesn't address our keyed-state problem because axum doesn't have a "session" concept the framework owns.
- **`tracing-subscriber` filter state**: per-span state in a per-subscriber `Mutex<HashMap<Id, FilterState>>`. Direct precedent for our shape.
- **`governor::state::keyed::DashMapStateStore`**: per-key state for rate limiting; uses `Arc<DashMap<Key, _>>` internally. Direct precedent + serves as the survey answer for "is `DashMap` the right primitive?" Yes.
- **`bb8` / `deadpool` connection pools**: per-connection state in `Mutex<...>` inside an `Arc<Pool>`. Same shape.
- **`sled` / `redb`** key-keyed mutable state: tree-keyed; we have flat keys.

### Recommendation

**Option A: `Arc<DashMap<SessionId, Mutex<SessionState>>>` with `SessionState` as a plain struct containing a small `enum SessionPhase`.**

Specific decisions:

1. **`dashmap = "6"` direct dep** (MIT). Day 14 D10 declared it "deferred to build phase; only if `MemoryStorage` benchmarks meaningfully prefer it over `Mutex<HashMap>`". For the orchestrator the workload is "many keys, short critical sections, possibly across many sessions concurrently" — DashMap's per-shard locks are the right shape, and the API (`get_mut`, `entry()`, etc.) reads cleaner than `parking_lot::Mutex<HashMap<_>>`. License is MIT.
2. **`std::sync::Mutex<SessionState>` inside the DashMap entry**, not `tokio::sync::Mutex`. Critical sections are short and synchronous (no `.await` while holding the lock). `std::sync::Mutex` is faster than `tokio::sync::Mutex` for non-await-spanning use. Async work happens BEFORE entering and AFTER exiting the critical section (call classifier, await result, then re-acquire the session lock to record the signal).
3. **`SessionPhase` as small enum for the in-flight state**, embedded in `SessionState`. Variants: `Idle` and `AwaitingClassifier { utterance: String, started_at: Instant }`. Other state (rate limit, recent turns, turn count) is always-present so lives at the `SessionState` level, not in variants.
4. **`#[non_exhaustive]`** on `SessionState` and `SessionPhase` — orchestrator state is engine-internal but future-proof.
5. **No `#[derive(Clone)]` on `SessionState`** — explicitly forbidden to discourage accidental snapshots that race with the live state.

### Code sketch

```rust
// src/engine/sentiment/orchestrator.rs (excerpt — 16a sketch)

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::engine::context::{Context, SessionId};
use super::types::{
    LoadedItemId, RecentTurn, /* ... */
};
use super::classifier::SentimentClassifier;

/// Per-session in-memory state.
#[derive(Debug)]
#[non_exhaustive]
pub struct SessionState {
    /// Bounded ring buffer of last N turns (N defaults to 6).
    pub recent_turns: VecDeque<RecentTurn>,
    /// Per-lesson last-emit timestamp for the (session, lesson) cooldown.
    pub rate_limit: HashMap<LoadedItemId, Instant>,
    /// Idle vs in-flight phase.
    pub phase: SessionPhase,
    /// Monotone counter for turns processed in this session.
    pub turn_count: u64,
}

#[derive(Debug)]
#[non_exhaustive]
pub enum SessionPhase {
    Idle,
    AwaitingClassifier {
        utterance: String,
        started_at: Instant,
    },
}

/// Sentiment orchestrator — keyed mutable state across sessions.
#[derive(Clone)]
pub struct Orchestrator {
    inner: Arc<OrchestratorInner>,
}

struct OrchestratorInner {
    classifier: Arc<dyn SentimentClassifier>,
    sessions: DashMap<SessionId, Mutex<SessionState>>,
    config: OrchestratorConfig,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct OrchestratorConfig {
    pub recent_turn_capacity: usize,         // default 6
    pub per_lesson_cooldown: Duration,        // default 60s
    pub correction_window: Duration,          // default 30s (Day 17-aligned)
    pub classifier_timeout: Duration,         // default 5s
}
```

Critical-section discipline (audit smell S20 below): the orchestrator's process loop looks like:

```rust
async fn process_user_turn(&self, ctx: &Context, evt: &UserTurnEvent) -> Result<...> {
    // 1) Acquire short lock: append turn, check rate limit, set phase
    let (request, eligible_items) = {
        let entry = self.inner.sessions.entry(ctx.session_id.clone()).or_default();
        let mut state = entry.lock().expect("poisoned");
        state.recent_turns.push_back(...);
        if state.recent_turns.len() > self.inner.config.recent_turn_capacity {
            state.recent_turns.pop_front();
        }
        let eligible = filter_rate_limited(&state.rate_limit, items, now);
        state.phase = SessionPhase::AwaitingClassifier { ... };
        build_request_owned(state, &eligible)
    }; // <-- lock dropped here

    // 2) Async classifier call OUTSIDE the lock
    let raw = self.inner.classifier.classify(ctx, &request).await?;

    // 3) Re-acquire short lock: apply hazard filter, update rate limit, transition phase
    let signals = {
        let entry = self.inner.sessions.get_mut(&ctx.session_id).unwrap();
        let mut state = entry.lock().expect("poisoned");
        let s = derive_signals(&raw, &eligible_items, &state.recent_turns);
        for sig in &s {
            state.rate_limit.insert(sig.item_id.clone(), now);
        }
        state.phase = SessionPhase::Idle;
        state.turn_count += 1;
        s
    };

    Ok(signals)
}
```

The pattern (`lock → snapshot → drop → await → re-lock`) is the same one `tower-http` and `governor` use internally. No `MutexGuard` crosses an `.await` boundary — a clippy-lint-flaggable mistake.

### Trade-offs

`DashMap<SessionId, Mutex<SessionState>>` with plain-struct state (chosen — short critical sections, no fairness need, `Arc<dyn SentimentClassifier>` already cloneable) over: typestate (doesn't compose with keyed storage), actor crates (heavy dep, no win over Mutex), `enum SessionState` discriminated state (always-present fields awkward), per-session `tokio::spawn` (shutdown complexity, startup latency).

### Audit smells (orchestrator-specific — see S18–S30 at end)

- `tokio::sync::Mutex<SessionState>` when critical sections don't `.await` (S22)
- Holding `MutexGuard` across `.await` (S23)
- `Arc<RwLock<HashMap<SessionId, SessionState>>>` when `DashMap` fits (S25)

---

## Q3: Per-session vs per-(session, user, lesson) state

### Survey

The orchestrator's per-session state contains:

- **Recent turns**: session-scoped (one buffer per session).
- **Rate-limit timestamps**: per-(session, lesson). Inside `SessionState.rate_limit: HashMap<LoadedItemId, Instant>` — sub-map inside the session entry, not a separate top-level map.
- **In-flight classifier**: session-scoped (one phase enum per session — at most one classifier call in flight per session).
- **Turn count**: session-scoped.
- **Last-seen sentiment per (session, lesson)**: per-(session, lesson). Same shape as rate-limit; lives in `SessionState.last_sentiment: HashMap<LoadedItemId, LastSentiment>` (Day 17 adds this; 16a may stub).

Multi-tenant routing: `Context` carries `(tenant_id, user_id, session_id, team_id)`. `session_id` is globally unique by construction (UUID-derived); collisions across tenants are not a concern in single-user mode and would be a routing layer responsibility in SaaS mode.

### Recommendation

**16a keys orchestrator state on `SessionId` only.** Reasons:

1. **Single source of truth: `SessionId` is the natural primary key.** Cross-tenant routing happens at the engine boundary via `Context`; the orchestrator receives an already-routed `&Context` and processes events in that session's scope.
2. **No multi-tenant collision risk today.** `SessionId` is generated by the host (Claude Code's JSONL filename or a host-assigned UUID) and is unique per session globally. The day SaaS mode wants `(TenantId, SessionId)` keys, the change is mechanical (introduce `SessionKey { tenant: TenantId, session: SessionId }`, derive `Hash + Eq`, swap DashMap key type). Until then, `SessionId` keeps the map cheap and the code reads naturally.
3. **`(session, lesson)` rate limit lives inside `SessionState`, not as a top-level `DashMap<(SessionId, LoadedItemId), Instant>`.** Reasons:
   - Lessons are short-lived per session — when a session ends, its rate-limit entries should disappear (and they will, when the session entry is removed from the outer DashMap).
   - Lookup is `state.rate_limit.get(&lesson_id)` — same big-O, simpler to reason about.
   - "All rate limit state for session X" is one DashMap lookup, not N.

### Concurrency story

- **Multiple sessions process events concurrently** via DashMap's per-shard locks.
- **Within a session, events serialize** through the per-session Mutex. This is fine — events for the same session are sequential by nature (one user turn at a time per session); even if the host fan-out happens to deliver out-of-order, the in-session lock keeps state coherent.
- **Classifier calls happen outside the lock** (per Q2 critical-section discipline). Multiple classifier calls across sessions run concurrently.
- **Session entry lifecycle**: 16a creates the session entry on first event (via `DashMap::entry().or_default()`), drops it on `SessionEnded` event. No background GC; the engine cleans up on the SessionEnded signal.

### Code sketch

```rust
fn on_session_started(&self, session_id: &SessionId) {
    // Lazy creation; first user turn would also create it via entry().or_default().
    // Explicit on SessionStarted to support pre-warming if desired.
    self.inner.sessions
        .entry(session_id.clone())
        .or_insert_with(|| Mutex::new(SessionState::new(&self.inner.config)));
}

fn on_session_ended(&self, session_id: &SessionId) {
    self.inner.sessions.remove(session_id);
}
```

### Trade-offs

`SessionId`-keyed outer + nested per-lesson sub-maps (chosen — natural lifetime, cheap lookups) over: `(SessionId, LoadedItemId)`-keyed flat map (lifecycle management hard — when does an entry get GC'd?), separate top-level `DashMap`s per-concept (state coherence harder; need cross-map invariants).

### Audit smells

- `DashMap<(SessionId, LoadedItemId), Instant>` flat top-level — orphan entries after session end (S26)
- Per-session `Arc<Mutex<SessionState>>` cloned out of the outer map (clones the Arc, fine — but cloning the inner state struct via `.read()`-into-snapshot pattern would be wrong)

---

## Q4: Rate limiting primitive

### Survey

The orchestrator rule (audit-A2 lineage, also rule 8 design-rules but solicitor-scoped):

- **≤1 sentiment signal per (session, lesson) per N seconds.** N is configurable; default 60s.
- Not a global rate limit; per-(session, lesson).
- No burst budget; this is a "cooldown after emit," not a token bucket.

Options:

| Option | Verdict |
|---|---|
| **`governor = "0.6"`** (token bucket / GCRA, MIT/Apache) | Industry-standard Rust rate limiter. Used by `axum-governor`, `tower-governor`, many backend services. Provides `DashMapStateStore<K>` keyed limiters out of the box. Real shape match would be `RateLimiter<LoadedItemId, ...>`. **Overkill** for a fixed cooldown — governor models burst + sustained rates; we have only one rule. |
| **`tower::limit::RateLimitLayer`** | Tower middleware shape; designed for `tower::Service` integration. Wrong fit — orchestrator is not a `Service`. |
| **Hand-rolled `HashMap<LoadedItemId, Instant>` + cooldown check** | Two lines of code. Lives inside `SessionState`. No dep added. |
| **`tokio::time::Interval`-based** | For periodic ticks, not cooldown-after-emit. Wrong shape. |

### Recommendation

**Hand-rolled, inside `SessionState`.**

```rust
fn allow_signal(&self, lesson_id: &LoadedItemId, now: Instant, cooldown: Duration) -> bool {
    match self.rate_limit.get(lesson_id) {
        Some(&last) => now.duration_since(last) >= cooldown,
        None => true,
    }
}

fn record_signal(&mut self, lesson_id: LoadedItemId, now: Instant) {
    self.rate_limit.insert(lesson_id, now);
}
```

Justification:

1. **One rule, no burst.** Governor's value-add is correctly modeling burst + sustained rates; we have neither. Importing it for "≤1 per 60s" is sleeve-of-shotguns.
2. **Per-session lifetime.** The rate limit map should die with the session; integrating governor's per-key state with the per-session `SessionState` map adds complexity (two layers of keyed state).
3. **No new dep.** Day 15 added one direct dep (`regex`). Day 16 already adds `dashmap`; adding `governor` on top stacks two new deps in one cycle.
4. **Day 15 attribution kept things minimal** (pure functions, no abstraction layers). Orchestrator's rate limit follows the same KISS.
5. **Audit precedent**: in survey, `tracing-subscriber`'s per-span filter state uses a plain `HashMap` for the equivalent shape, not a rate-limit crate. Hand-roll when the rule is one line.

If Day 17 adds the solicitor "≤1 prompt per 20 turns" rule and that's a real burst pattern, we revisit; right now it's also a fixed cooldown.

### Code sketch

(See Q2 code sketch — the rate limit lives inline.)

### Trade-offs

Hand-rolled (chosen — no new dep, simple rule, per-session lifetime) over: `governor` (overkill for one cooldown rule), `tower::limit` (wrong shape), `tokio::time::Interval` (wrong shape).

### Audit smells

- `governor::RateLimiter` for a single-rule cooldown (S24)
- `tokio::sync::Mutex<HashMap<LoadedItemId, Instant>>` as a top-level shared rate-limit table (S26 — orphan-on-session-end)

---

## Q5: `LocalFsStorage::put_if_version` implementation

**This question lands in 16b.** Answered here for design completeness.

### Survey

Today's Day 14 stub returns `StorageError::Backend(...)` with the message `"put_if_version not yet implemented for LocalFsStorage (Phase 3c)"`. The semantics are locked by the trait (per `src/engine/storage/mod.rs:60`):

- `Ok(true)` on success
- `Ok(false)` if the precondition failed (current version differs)
- `expected_version = None` means "must not exist" (create-only)

Existing lock infrastructure (`src/engine/lessons/lock.rs`):

- `with_lock(target: &Path, f: F) -> Result<T>` — exclusive flock on a SIDECAR file (`.<name>.lock` in the same dir).
- Audit Day 12 caught a race where flock-on-target with atomic-rename leaks (rename swaps inodes; new readers take a flock on a different inode). Sidecar lock is on a stable inode.
- 127-test-validated; cross-process serializing demonstrated by `lock_serializes_concurrent_callers` and `lock_survives_target_rename` tests.

### Options

**(a) Lift `with_sidecar_lock` into `LocalFsStorage::put_if_version` directly.** Generic over keys, sidecar in the same directory, version-check inside the lock.

**(b) Different mechanism — e.g. content-hash-based versioning + try-rename loop.** Pro: no flock. Con: hash-compute is O(file size); race window between read-version and rename is non-trivial; doesn't compose with TS-side flock (the TS MCP server uses the existing sidecar flock for the same files).

**(c) Atomic per-key sidecar version file** — `~/.loop/lessons/active/.les-abc.md.ver` holding an mtime stamp. CAS via flock+read+compare+write. Same flock dep as (a); more files; no clear win.

### Recommendation

**Option (a): lift the existing `with_sidecar_lock` pattern.** Reasons:

1. **TS-side cross-process compat preserved.** The TS MCP server (`core/src/lessons/lock.ts` historically; whatever ships today) writes via the same sidecar flock. If we change CAS semantics in Rust without coordination, mixed-process writes can race. Sidecar-flock-then-CAS keeps both processes serializing through the same kernel mutex.
2. **127-test-validated correctness.** Day 12's audit + the sidecar test suite proved this pattern. Reuse beats re-derive.
3. **Single source of code.** The lock module's `with_lock` becomes a private helper inside `storage::filesystem` — or stays in `engine::lessons::lock` and is called from filesystem.rs. The latter creates a layer-violation (storage referencing lessons); the former is cleaner. Recommend MOVE `lock.rs` to `engine::storage::lock` and re-export from `engine::lessons::lock` during the migration window (16b first commit moves the module; second-commit retires re-export).

### `Version` encoding

Current `Version(Box<[u8]>)` is opaque. Encoding choice for filesystem:

- **`mtime_ns + len`** (16 bytes mtime as i128 nanos + 8 bytes length as u64 = 24 bytes total). Cheap to compute (one `stat` call), zero collision risk for in-process callers, low collision risk cross-process.
- **`mtime_ns + inode + len`** (24 + 8 = 32 bytes). Inode protects against the inode-reuse case (file deleted, new file created at same path with same mtime). Probably unnecessary for our atomic-rename pattern (rename swaps inodes, so the new inode differs from the old one for the version check to detect).
- **SHA-256 of content** (32 bytes). Strongest correctness; expensive O(file size) on every read. Reject — overkill for files under 64KB (lesson size cap).
- **`mtime_ns` alone** (16 bytes). Vulnerable to sub-millisecond writes (mtime resolution varies by FS — APFS ms, ext4 ns, FAT32 2s). Reject — APFS mtime resolution alone is too coarse for confident CAS.

**Recommend: `mtime_ns (i128, 16 bytes) + len (u64, 8 bytes)` = 24 bytes.** mtime in nanos protects against same-millisecond writes on APFS by virtue of the kernel returning a higher-precision value where available; len catches the rare same-mtime-different-content case (writer wrote, FS coalesced mtime, reader reads stale).

Detail: `std::fs::Metadata::modified()` returns `SystemTime`; converting to nanos-since-epoch on Linux/macOS works via `duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as i128`. APFS clamps to milliseconds, so the low ~6 digits of the i128 will be zero on macOS — not a correctness issue, but worth a comment.

### `get_with_version` atomic semantics

The version must come from the SAME read as the bytes — otherwise:

1. Reader stats file (version A).
2. Writer writes new content (version B).
3. Reader reads bytes (now version B).
4. Reader returns `(bytes_B, version_A)` — CAS will succeed when it shouldn't.

Two ways to keep them coherent:

- **(i) Hold the sidecar flock for the read.** Acquires the lock, stats, reads, releases. Reads serialize against writes via the same lock the writer takes. Cost: cross-process contention.
- **(ii) Read first, then stat AFTER read; if mtime changed, retry.** Read-stat-retry loop. Read may transiently see partial-write state if writer didn't atomic-rename — but our writer DOES atomic-rename, so the read sees either the old file or the new file, never partial. Cost: occasional retry.

Recommend (i) for consistency with `put_if_version`. The cross-process contention is only meaningful when multiple writers compete, which is rare (single user, both Rust daemon and TS MCP would write at most a few signals per minute).

### Failure modes

1. **Lock contested across processes**: blocks until released. Acceptable — flock blocks; we already accept this in `lessons/signals.rs`.
2. **Lock file unlinked between sidecar-path-resolution and lock acquisition**: race-with-cleanup. Sidecar file gets `create=true; truncate=false` so a stale path resolves to a freshly created file. Same behavior as today.
3. **Reader sees `expected_version = None` but file exists**: `put_if_version` returns `Ok(false)`; caller retries with re-read.
4. **Version encoding changes between Rust versions**: opaque `Version` is fine as long as the encoding is consistent within one daemon's run. Cross-daemon-version: callers that hold a `Version` across an upgrade may see CAS fail; recover by re-reading. This is a feature, not a bug.

### Code sketch

```rust
// src/engine/storage/filesystem.rs (excerpt, 16b)

async fn put_if_version(
    &self,
    key: &StorageKey,
    bytes: Bytes,
    expected_version: Option<&Version>,
) -> Result<bool, StorageError> {
    let path = self.resolve(key);
    let bytes_owned = bytes; // already owned
    let expected_owned = expected_version.cloned();

    // flock + version-check + atomic write under tokio::task::spawn_blocking
    // because fd_lock is sync. Same crate that lessons/lock.rs uses today.
    tokio::task::spawn_blocking(move || -> Result<bool, StorageError> {
        let lock_path = sidecar_lock_path(&path).map_err(StorageError::backend)?;
        let lock_file = open_lock_file(&lock_path)?;
        let mut lock = fd_lock::RwLock::new(lock_file);
        let _guard = lock.write().map_err(StorageError::backend)?;

        let current_version = stat_version(&path)?;
        if current_version != expected_owned {
            return Ok(false);
        }

        let tmp = tmp_path_for(&path);
        std::fs::write(&tmp, &bytes_owned).map_err(StorageError::backend)?;
        std::fs::rename(&tmp, &path).map_err(StorageError::backend)?;
        Ok(true)
    })
    .await
    .map_err(StorageError::backend)?
}

fn stat_version(path: &Path) -> Result<Option<Version>, StorageError> {
    match std::fs::metadata(path) {
        Ok(m) => {
            let mtime_ns = m.modified()
                .map_err(StorageError::backend)?
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as i128;
            let len = m.len();
            let mut bytes = [0u8; 24];
            bytes[..16].copy_from_slice(&mtime_ns.to_le_bytes());
            bytes[16..].copy_from_slice(&len.to_le_bytes());
            Ok(Some(Version::from_bytes(bytes.to_vec().into_boxed_slice())))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StorageError::backend(e)),
    }
}
```

### Regression tests (16b ships these)

1. **`put_if_version_succeeds_on_create_only_when_absent`** — `expected = None`, key doesn't exist, returns `Ok(true)`, file present.
2. **`put_if_version_fails_create_only_when_present`** — `expected = None`, key exists, returns `Ok(false)`, file unchanged.
3. **`put_if_version_succeeds_with_matching_version`** — write A, read version, write B with version, returns `Ok(true)`.
4. **`put_if_version_fails_with_stale_version`** — write A, read version V, write B-via-CAS, attempt write C-with-stale-V, returns `Ok(false)`.
5. **`cross_thread_concurrent_cas_serializes`** — 4 threads do read-modify-write loops; final state is deterministic and all writes account for (combination of `Ok(true)` and `Ok(false)` retries).
6. **`get_with_version_consistent_under_concurrent_write`** — reader and writer in tight loops; reader's `(bytes, version)` pair is always self-consistent (the bytes match what the version pin would CAS-write).
7. **`sidecar_lock_is_compatible_with_legacy_lessons_lock`** — both `engine::lessons::lock::with_lock` and `LocalFsStorage::put_if_version` use the SAME sidecar inode; verify by overlapping write attempts and confirming mutual exclusion.

### Trade-offs

Lift existing sidecar-flock pattern (chosen — 127-test-validated, TS-cross-process-compat) over: content-hash CAS (expensive O(file_size)), sidecar version-file (no win, more files), in-memory lock manager (loses cross-process).

### Audit smells (16b-flavor)

- Reading bytes outside the lock and stat'ing inside (split read) — corrupt version-bytes pair (S28)
- Holding the flock across `.await` — sync flock has no async equivalent, must `spawn_blocking` (S29)
- Encoding `Version` as `String` (e.g. mtime serialized as ISO) instead of opaque bytes (S30)

---

## Q6: Lessons module migration to Storage

**This question lands in 16b.**

### Current state

Per `src/engine/lessons/loader.rs`:

```rust
pub fn get_lesson_by_id(id: &str) -> Result<Option<LoadedLesson>> {
    for status in paths::LESSON_STATUS_DIRS {
        let candidate = paths::lessons_status_dir(status)?.join(format!("{id}{LESSON_FILE_EXT}"));
        // ...
    }
}
```

`paths::loop_home()` reads `$LOOP_HOME` env var. Tests use `with_temp_loop_home` (locks `ENV_LOCK`, sets env, runs, restores). Inherently sequential at the test-binary level.

Per `src/engine/lessons/signals.rs`:

```rust
pub fn record_sentiment_signal(id: &str, polarity: SignalPolarity) -> Result<LoadedLesson> {
    let initial = get_lesson_by_id(id)?.ok_or_else(|| anyhow!("lesson not found: {id}"))?;
    let path = initial.path.clone();
    with_lock(&path, || { /* re-read, modify, atomic-rename */ })
}
```

Returns `anyhow::Result`. Day 14 D8 / L6 deferred typed-error migration to Day 16.

### Migration target

New shape (Day 14 D8, two-phase):

```rust
// new
pub async fn get_by_id(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
) -> Result<Option<LoadedLesson>, EngineError>;

pub async fn record_sentiment_signal(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
    polarity: SignalPolarity,
) -> Result<LoadedLesson, EngineError>;
```

Where `record_sentiment_signal` becomes a CAS-loop:

```rust
loop {
    let (bytes, version) = storage.get_with_version(&key).await?;
    let modified = apply_signal(bytes, polarity)?;
    if storage.put_if_version(&key, modified, Some(&version)).await? {
        return Ok(...);
    }
    // CAS lost — re-read and retry
}
```

### Migration order (incremental, leaf-first per Day 14 D8)

**16b-step-1**: Add new APIs alongside old ones (delegating wrappers). Old signatures stay; new signatures call through.

```rust
// Old API stays for one cycle
pub fn get_lesson_by_id(id: &str) -> Result<Option<LoadedLesson>> {
    let ctx = Context::single_user_local();
    let storage = LocalFsStorage::new(paths::loop_home()?);
    futures::executor::block_on(get_by_id(&ctx, &storage, id))
        .map_err(anyhow::Error::from)
}
```

**16b-step-2**: Migrate the orchestrator (16a's caller in 16b-step-2; this is the FIRST caller of the new API — clean integration moment).

**16b-step-3**: Migrate `lessons::signals::record_sentiment_signal` internals to use `Storage::put_if_version` (CAS loop). Old `with_lock`-based code retires.

**16b-step-4**: Migrate `lessons::loader` tests off `ENV_LOCK` to `TestHarness { ctx, storage: MemoryStorage }`.

**16b-step-5**: Migrate `lessons::signals` tests similarly.

**16b-step-6**: Delete `ENV_LOCK` if no remaining callers.

Each step: `cargo test --all` green.

### Big-bang vs incremental

**Incremental** — strongly preferred. Day 14 D8 locked this; Day 15 audit confirmed the two-phase pattern works (M3 / M4 findings were small drift, not migration regressions).

Big-bang reasons against:
- 7+ tests touch `with_temp_loop_home` + ENV_LOCK; migrating all in one commit means one bug surface = full revert.
- The orchestrator (16a) is the natural FIRST caller of the new API. Big-bang would force the orchestrator to also migrate before it ships, conflating audit surfaces.

### Wrapper retirement timing

The delegating wrappers retire when:

1. All in-crate callers use the new `(ctx, storage)` shape.
2. No tests reference `ENV_LOCK`.
3. `paths::loop_home()` is unused outside the host wiring (where `LocalFsStorage::new(loop_home())` constructs the production storage).

Recommend retire in 16b's final commit; Day 17 audit verifies no regressions.

### Error type: `EngineError` introduction

Currently `lessons/loader.rs` and `lessons/signals.rs` use `anyhow::Result`. Day 14 L6 forward-fed this. 16b introduces:

```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EngineError {
    #[error("storage: {0}")]
    Storage(#[from] StorageError),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("yaml: {0}")]
    Yaml(#[source] Box<dyn std::error::Error + Send + Sync>),
}
```

Lives in `engine::error::EngineError`. New `lessons` APIs return `Result<_, EngineError>`. Delegating wrappers convert via `EngineError -> anyhow::Error` for backward compat during step-1.

### Tests: migration to `TestHarness`

Day 14 D7 locked `TestHarness { context, storage }`. Today no `TestHarness` exists (Day 14 D7 said "drop `with_temp_loop_home` ONCE all callers migrated" — not yet).

16b introduces `engine::test_support::TestHarness` (cfg-gated behind `test-fixtures` feature, OR `#[cfg(test)]`):

```rust
#[cfg(any(test, feature = "test-fixtures"))]
pub struct TestHarness {
    pub ctx: Context,
    pub storage: Arc<dyn Storage>,
    _tempdir: Option<tempfile::TempDir>,
}

impl TestHarness {
    pub fn memory() -> Self { /* MemoryStorage, no tempdir */ }
    pub fn fs() -> Self { /* LocalFsStorage on a fresh tempdir */ }
}
```

Tests rewrite from:

```rust
with_temp_loop_home(|tmp| {
    write_minimum_lesson(tmp, "active", "les-aaaaaaaa");
    let loaded = get_lesson_by_id("les-aaaaaaaa")?.expect(...);
    ...
});
```

to:

```rust
let h = TestHarness::fs();
write_minimum_lesson(&h, "active", "les-aaaaaaaa").await;
let loaded = lessons::get_by_id(&h.ctx, &*h.storage, "les-aaaaaaaa").await?.expect(...);
```

Tests now run in parallel (no global ENV mutation).

### Verification per step

- After 16b-step-1: all old tests pass (delegating wrappers preserve behavior).
- After 16b-step-2: orchestrator integration tests pass against `MockSentimentClassifier` + `MemoryStorage`.
- After 16b-step-3: old `with_lock`-based `record_sentiment_signal` is gone; `cargo grep -r "with_lock" src/engine/lessons` returns only the legacy module if still imported.
- After 16b-step-4 / step-5: `ENV_LOCK` usage drops to zero; `cargo test` runs faster (parallelization).
- After 16b-step-6: `ENV_LOCK` static + `with_temp_loop_home` helper deleted.

### Trade-offs

Incremental + leaf-first (chosen — Day 14 D8 precedent, audit-friendly) over: big-bang (audit risk, one-bug-full-revert), wrappers retained forever (debt accumulation).

### Audit smells (16b)

- Migration commit that touches both old and new APIs in one diff — should be two commits (S27)
- `anyhow::Error` leaking through new `(ctx, storage)` APIs — should be typed `EngineError`
- `with_temp_loop_home` + `TestHarness` both used in same test (transitional state outliving the cycle)

---

## Q7: `JsonlWatcher` → `EventSource` impl

**This question lands in 16a.**

### Current state

`src/host/claude_code/jsonl_watcher/runner.rs`:

```rust
pub async fn spawn_watcher(
    dir: PathBuf,
    events_tx: UnboundedSender<WatcherEvent>,
) -> Result<WatcherHandle> { ... }
```

Returns a handle; caller holds the receiver. WatcherEvents have fields:

```rust
WatcherEvent::UserTurn {
    session_id: String,
    event_uuid: String,
    parent_uuid: Option<String>,
    cwd: PathBuf,
    git_branch: Option<String>,
    timestamp: DateTime<Utc>,
    text: String,
    cc_version: String,
}
```

EngineEvent shape (Day 15 D1-locked):

```rust
EngineEvent::UserTurn {
    session_id: SessionId,
    event_uuid: String,
    parent_event_uuid: Option<String>,
    text: String,
    timestamp: DateTime<Utc>,
    cwd: Option<PathBuf>,
    host_version: Option<HostVersion>,
    project_tag: Option<ProjectTag>,
}
```

### Recommendation

Add a thin wrapper struct `JsonlWatcherSource` in `src/host/claude_code/jsonl_watcher/source.rs`:

```rust
// src/host/claude_code/jsonl_watcher/source.rs

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::engine::context::{Context, SessionId};
use crate::engine::events::{EngineEvent, EventSource, EventSourceError, HostVersion, ProjectTag};

use super::events::WatcherEvent;
use super::runner::spawn_watcher;

/// `EventSource` impl over the Claude Code JSONL watcher. Translates
/// `WatcherEvent` → `EngineEvent` per the Day 15 D1 mapping.
#[derive(Debug, Clone)]
pub struct JsonlWatcherSource {
    dir: PathBuf,
}

impl JsonlWatcherSource {
    pub fn new(dir: PathBuf) -> Self { Self { dir } }
}

#[async_trait]
impl EventSource for JsonlWatcherSource {
    async fn run(
        &self,
        _ctx: &Context,
        shutdown: CancellationToken,
    ) -> BoxStream<'static, Result<EngineEvent, EventSourceError>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<WatcherEvent>();
        let dir = self.dir.clone();

        // spawn_watcher returns a handle; drop it when shutdown fires.
        let handle = match spawn_watcher(dir, tx).await {
            Ok(h) => h,
            Err(e) => {
                // Fatal — couldn't attach FSEvents.
                let one = futures::stream::once(async move {
                    Err(EventSourceError::fatal(
                        std::io::Error::other(format!("watcher init: {e}")),
                    ))
                });
                return Box::pin(one);
            }
        };

        // Bridge: shutdown -> drop handle -> tx drops -> rx closes
        tokio::spawn(async move {
            shutdown.cancelled().await;
            drop(handle);
        });

        let receiver_stream = UnboundedReceiverStream::new(rx);
        let translated = receiver_stream.filter_map(|w_evt| async {
            translate(w_evt)
        });
        Box::pin(translated)
    }

    fn name(&self) -> &'static str {
        "claude_code.jsonl_watcher"
    }
}

fn translate(w: WatcherEvent) -> Option<Result<EngineEvent, EventSourceError>> {
    match w {
        WatcherEvent::UserTurn {
            session_id, event_uuid, parent_uuid, cwd, git_branch,
            timestamp, text, cc_version,
        } => Some(Ok(EngineEvent::UserTurn {
            session_id: SessionId::new(session_id),
            event_uuid,
            parent_event_uuid: parent_uuid,
            text,
            timestamp,
            cwd: Some(cwd),
            host_version: Some(HostVersion::new(cc_version)),
            project_tag: derive_project_tag(&git_branch, &cwd),
        })),
        WatcherEvent::UserInterrupt {
            session_id, event_uuid, parent_uuid, timestamp, ..
        } => Some(Ok(EngineEvent::UserInterrupt {
            session_id: SessionId::new(session_id),
            event_uuid,
            parent_event_uuid: parent_uuid,
            timestamp,
        })),
        WatcherEvent::SessionStarted { session_id, path, started_at } => Some(Ok(EngineEvent::SessionStarted {
            session_id: SessionId::new(session_id),
            path,
            started_at,
        })),
        WatcherEvent::SessionEnded { session_id } => Some(Ok(EngineEvent::SessionEnded {
            session_id: SessionId::new(session_id),
        })),
        WatcherEvent::ParseError { offset, error, .. } => Some(Err(EventSourceError::transient(
            std::io::Error::other(format!("parse error at offset {offset}: {error}")),
        ))),
    }
}

fn derive_project_tag(git_branch: &Option<String>, cwd: &PathBuf) -> Option<ProjectTag> {
    // OQ5: host adapter derives. Prefer git_branch; fall back to cwd basename.
    if let Some(branch) = git_branch.as_deref().filter(|s| !s.is_empty()) {
        return Some(ProjectTag::new(branch.to_string()));
    }
    cwd.file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .map(|s| ProjectTag::new(s.to_string()))
}
```

### Key decisions

1. **Reuse `spawn_watcher` internally.** Don't rewrite the watcher; the `EventSource` impl is a translation layer. Day 13's audit-validated cursor management, the A1-A5 fixes, and 127-test correctness all live in `runner.rs`. Keep it.
2. **`UnboundedReceiverStream` from `tokio-stream`** — already a transitive of `tokio_util` we have. Bridges mpsc to `Stream` trivially.
3. **Shutdown via dropping the `WatcherHandle`.** The handle owns the `notify::Watcher` and the runner task; dropping the handle stops FSEvents and lets the runner task drain and exit. We spawn a small task that waits for `shutdown.cancelled()` then drops the handle.
4. **Fatal vs transient errors.** `ParseError` → transient (single bad line, stream continues). Watcher init failure → fatal (one-shot error item then stream ends naturally via dropped sender). `SessionEnded` is a normal event, NOT an error.
5. **`SessionId::new(string)`** — wrap the bare `String` from `WatcherEvent` into a typed `SessionId`. Allocates once per event; `SessionId::new` takes `Into<Arc<str>>` so future cheap-clone in the orchestrator.
6. **`derive_project_tag`** lives here, per Day 15 OQ5: host adapter derives, engine treats as opaque.
7. **`_ctx: &Context`** is unused today; reserved for forward-feed (per-tenant directory routing in SaaS). Same prefix-underscore pattern Day 14 Storage methods use.

### Backward compat with `spawn_watcher`

`spawn_watcher` stays public. Existing Day 13 integration tests at `src/host/claude_code/jsonl_watcher/runner.rs:322+` keep working unchanged. 16a doesn't migrate them.

Day 17 audit OR a dedicated audit-sweep cycle decides whether `spawn_watcher` and `WatcherHandle` retire to `pub(crate)` after the orchestrator becomes the only consumer.

### Tests in 16a

1. **`translate_user_turn_maps_all_fields`** — unit test on the `translate` function; pin every field mapping.
2. **`translate_parse_error_is_transient`** — pin error-class mapping.
3. **`derive_project_tag_prefers_git_branch`** — pin OQ5 derivation.
4. **`derive_project_tag_falls_back_to_cwd_basename`** — pin fallback.
5. **`integration_engine_event_flows_through_source`** — spin up `JsonlWatcherSource`, write a JSONL line to the watched dir, consume from `BoxStream`, assert `EngineEvent::UserTurn` arrives. Reuses Day 13's integration test scaffolding.
6. **`shutdown_terminates_stream`** — start source, cancel `CancellationToken`, assert stream ends within a timeout.

### Trade-offs

Wrapper-over-existing-`spawn_watcher` (chosen — preserves 127-test validation, audit-A5 cursor logic, A1 tail-from-now) over: rewrite watcher into a `Stream` impl directly (re-derives audit-validated code), full deprecation of `spawn_watcher` (callers break — no benefit).

### Audit smells

- Translating inside a `match` that ends up in `Box::pin` allocations per event — `filter_map` is cheap (S21)
- Re-implementing `spawn_watcher`'s setup logic in the EventSource impl
- Translating `SessionEnded` to `EventSourceError::Transient` (it's a normal event)
- Lossy field translation (forgetting `parent_event_uuid` or `host_version`)
- `unwrap()` on `dir.file_name()` (must handle empty / root paths)

---

## Q8: Hazard auto-abstain + correction-window mining

**Both land in 16a.**

### Hazard auto-abstain

Per `sentiment-design-rules.md` (hazards section):

- Sarcasm suspected → auto-abstain
- Ambiguous referent → auto-abstain
- Self-directed (frustration without proximal AI/skill action) → auto-abstain
- Low register volatility → auto-abstain (rule 15)
- Faux-pas-class oversteps → auto-abstain

Translation to `Hazard` enum (per `engine::sentiment::types::Hazard`):

```rust
Hazard::Sarcasm => abstain
Hazard::AmbiguousReferent => abstain
// (Engine doesn't have `SelfDirected` or `LowRegister` variants yet —
//  16a OR 17 adds them per design rules. Defer to 16a learn-notes:
//  add SelfDirected to the Hazard enum if classifier returns it.)
Hazard::Hyperbole => NOT auto-abstain (signal still useful, gate may discount)
Hazard::LowConfidence => NOT auto-abstain (use Polarity threshold instead)
Hazard::PrivacyConcern => NOT auto-abstain (signal still useful)
Hazard::OutOfDistribution => abstain
```

### TS reference (`orchestrator.ts:60-87`)

```typescript
const ABSTAIN_HAZARDS = new Set([
    'sarcasm_suspected',
    'ambiguous_referent',
    'self_directed',
]);
// ...
const allHazards = [...item.hazards, ...raw.globalHazards];
if (allHazards.some((h) => ABSTAIN_HAZARDS.has(h))) {
    continue; // skip this signal
}
```

### Rust port

```rust
// In Orchestrator::derive_signals (called inside the second critical section per Q2).
fn is_auto_abstain_hazard(h: Hazard) -> bool {
    matches!(
        h,
        Hazard::Sarcasm | Hazard::AmbiguousReferent | Hazard::OutOfDistribution
        // Hazard::SelfDirected — if/when added to the enum
    )
}

let all_hazards = item.hazards.iter().chain(raw.global_hazards.iter()).copied();
if all_hazards.any(is_auto_abstain_hazard) {
    continue;
}
```

`Hazard` is `Copy` (Day 15 D10), so `.copied()` works on the iterator; no clone.

Note: 16a pre-research recommends **adding `Hazard::SelfDirected`** to the enum to faithfully port the TS abstain set. It's `#[non_exhaustive]` (Day 15 D10), so adding a variant is non-breaking.

### Threshold filtering (per sentiment-design-rules rule 5)

Before hazard check, the orchestrator filters by polarity-asymmetric thresholds:

```rust
const POSITIVE_MIN: f32 = 0.75;
const NEGATIVE_MIN: f32 = 0.85;

let threshold = match item.polarity {
    Polarity::Positive => POSITIVE_MIN,
    Polarity::Negative => NEGATIVE_MIN,
    Polarity::Neutral => continue,
};
if item.confidence.value() < threshold { continue; }
```

These constants live in `engine::sentiment::orchestrator` module (or a sibling `thresholds.rs`). Day 15 audit M5 flagged inline magic numbers in attribution.rs; the orchestrator does the same — pull these into `const` with rustdoc citing the design rule.

### Attribution cross-check (per TS audit-A2)

After the threshold and hazard checks, before emitting, cross-check with `attribute_signal`:

```rust
let attribution = attribute_signal(
    item.evidence.as_deref().unwrap_or(&utterance),
    &request.loaded_items,
    &request.recent_turns,
);
let Some(attr) = attribution else { continue };
if attr.item_id != item.item_id { continue; }
```

`attribute_signal` is the Day 15 D4 pure function. The closure-generic `_with_fallback` variant lets the orchestrator pass its classifier as the Pass 4 judge — for 16a, recommend NOT using `_with_fallback` initially; the classifier already ran for the per-item judgment, so re-running it as Pass 4 is wasteful. Pass 4 is the orchestrator's separate "judge top-K when 2-5 candidates" path, distinct from the per-item classify. 16a uses `attribute_signal` (no fallback); the closure-generic shape stays available for later.

### Correction-window mining

Per `sentiment-design-rules.md` rule 14: "frustration immediately after lesson-L-influenced turn → high prior on L." Per Day 15 OQ-D16-1: the orchestrator owns the recent-turn buffer; correction-window logic queries it.

Definition: when a `UserInterrupt` arrives within N seconds of an assistant turn that referenced item I, emit a strong negative signal on item I.

```rust
async fn process_user_interrupt(&self, ctx: &Context, evt: &UserInterruptEvent) -> Result<...> {
    let signals = {
        let entry = self.inner.sessions.get_mut(&ctx.session_id).unwrap();
        let mut state = entry.lock().expect("poisoned");
        let now = Instant::now();
        let recent = state.recent_turns.iter().rev();
        // Find the most recent assistant turn that referenced items
        let signals = recent
            .find(|t| t.role == TurnRole::Assistant && !t.referenced_items.is_empty())
            .filter(|t| /* turn-timestamp within correction_window of now */)
            .map(|t| build_negative_signals(&t.referenced_items, ctx))
            .unwrap_or_default();
        state.turn_count += 1;
        signals
    };
    Ok(signals)
}
```

The `correction_window` lives on `OrchestratorConfig` (default 30s — half the per-lesson cooldown so a real interrupt-then-frustration sequence isn't suppressed by rate limit; revisit in calibration).

### Rule 15 — frustration without proximal AI action

Per rule 15: "frustration without proximal AI/skill action does NOT decrement skill score." This maps to: if the most-recent assistant turn doesn't reference any loaded item, the orchestrator emits NO signal.

The check above (`!t.referenced_items.is_empty()`) is the implementation. If no assistant turn in the recent buffer referenced any item, the orchestrator abstains.

### Code sketch

```rust
// src/engine/sentiment/orchestrator.rs

fn derive_signals(
    raw: &RawClassification,
    request: &ClassificationRequest,
    rate_limit: &HashMap<LoadedItemId, Instant>,
    cooldown: Duration,
    now: Instant,
) -> Vec<SentimentSignal> {
    let mut signals = Vec::new();
    let mut seen = HashSet::new();
    for item in &raw.per_item {
        if seen.contains(&item.item_id) { continue; }
        let threshold = match item.polarity {
            Polarity::Positive => POSITIVE_MIN,
            Polarity::Negative => NEGATIVE_MIN,
            Polarity::Neutral => continue,
        };
        if item.confidence.value() < threshold { continue; }

        let all_hazards = item.hazards.iter().chain(raw.global_hazards.iter()).copied();
        if all_hazards.clone().any(is_auto_abstain_hazard) { continue; }

        // Attribution cross-check
        let utterance = item.evidence.as_deref().unwrap_or(&request.utterance);
        let attr = attribute_signal(utterance, &request.loaded_items, &request.recent_turns);
        let Some(attr) = attr else { continue };
        if attr.item_id != item.item_id { continue; }

        // Rate limit
        if let Some(&last) = rate_limit.get(&item.item_id) {
            if now.duration_since(last) < cooldown { continue; }
        }

        signals.push(SentimentSignal {
            item_id: item.item_id.clone(),
            polarity: item.polarity,
            calibrated_confidence: CalibratedConfidence::new(item.confidence.value()),
            attribution_method: attr.method,
            detected_hazards: all_hazards.collect(),
        });
        seen.insert(item.item_id.clone());
    }
    signals
}
```

### Trade-offs

Pure-function derive-signals inside short critical section (chosen — testable in isolation, no shared mutation) over: streaming-iterator (no perf win at our scale), per-pass methods on the orchestrator struct (state leakage).

### Audit smells

- Inline magic `0.75` / `0.85` instead of `POSITIVE_MIN` / `NEGATIVE_MIN` consts (S18; Day 15 M5 lineage)
- `HashSet<LoadedItemId>` dedup that uses `==` on `Arc<str>` — `LoadedItemId` already implements `Hash + Eq` correctly via `Arc<str>` (so this is fine; the smell is the OPPOSITE — wrapping in a different type and losing the Hash)
- `Vec<SentimentSignal>` with allocation when most calls produce 0 signals — pre-allocate with capacity 1 OR return `SmallVec`. Verdict: 1 signal/turn average; pre-allocate Vec::with_capacity(1) — no SmallVec dep
- `item.evidence.unwrap_or("")` then passing empty string to `attribute_signal` — bug (Pass 1 will match on empty string contains). Use `as_deref().unwrap_or(&request.utterance)`

---

## Sentiment-orchestrator-specific TS-with-Rust-syntax smells (S18–S30)

Extending Day 14's S1–S17 + Day 15's S1–S17 (separate numbering by cycle). Day 16 adds 13 orchestrator-flavored smells.

### S18. Inline literal thresholds without `const`

WRONG: `if item.confidence.value() < 0.75 { continue; }`
RIGHT: `const POSITIVE_MIN: f32 = 0.75; if item.confidence.value() < POSITIVE_MIN { ... }`
Rationale: design-rules-locked values; Day 15 M5 lineage.

### S19. `Box<dyn AttributionFallback>` instead of closure-generic

WRONG: `pub fn attribute_signal(..., fallback: Box<dyn AttributionFallback>) -> ...`
RIGHT: `pub fn attribute_signal_with_fallback<F: FnOnce(...)>(..., fallback: F)` (Day 15 D4 already locks this for attribution; same shape for any future fallback hooks in the orchestrator).

### S20. `Arc<Mutex<SessionState>>` cloned via `.clone()` before mutation

WRONG: `let state = entry.value().clone(); state.lock()...;` (clones the Arc; lock now contends on a stale Arc clone — actually this is correct for Arc, but the smell is the `.clone()`-then-lock pattern when `entry.value().lock()` is shorter and equally cheap)
RIGHT: `let mut state = entry.value().lock().expect("poisoned");`

### S21. Allocating-per-event in the stream pipeline

WRONG: `stream.map(|w| async move { Box::pin(translate(w)) })`
RIGHT: `stream.filter_map(|w| async move { translate(w) })` — translate returns `Option<Result<...>>`; filter_map yields only the Some values.

### S22. `tokio::sync::Mutex` when critical sections don't `.await`

WRONG: `tokio::sync::Mutex<SessionState>` when nothing inside the lock awaits.
RIGHT: `std::sync::Mutex<SessionState>` — faster, no async runtime needed for the lock itself.

### S23. Holding a `MutexGuard` across `.await`

WRONG:
```rust
let mut state = entry.lock().expect("poisoned");
let response = classifier.classify(ctx, &state.build_request()).await?;  // holding lock!
state.apply(response);
```
RIGHT:
```rust
let request = { entry.lock().expect("poisoned").build_request() }; // lock dropped
let response = classifier.classify(ctx, &request).await?;
{ entry.lock().expect("poisoned").apply(response); }
```
Note: `clippy::await_holding_lock` lint catches this. Enable it for the orchestrator module.

### S24. `governor::RateLimiter` for a single fixed-cooldown rule

WRONG: pulling in `governor` + `nonzero!` macros + per-key state stores for "≤1 per 60s."
RIGHT: hand-rolled `HashMap<K, Instant>` + cooldown check (Q4 above).

### S25. `Arc<RwLock<HashMap<SessionId, SessionState>>>` when `DashMap` fits

WRONG: `Arc<RwLock<HashMap<SessionId, ...>>>` — read-heavy assumed but writes are 1:1 with reads.
RIGHT: `Arc<DashMap<SessionId, Mutex<SessionState>>>` — sharded locks, per-key contention.

### S26. Orphan rate-limit entries after session end

WRONG: `Arc<DashMap<(SessionId, LoadedItemId), Instant>>` at engine top level — no clear GC.
RIGHT: rate-limit map lives INSIDE `SessionState`; dies with the session entry.

### S27. Mixing migration and feature work in one commit

WRONG: one commit moves `lessons/lock.rs` to `storage/lock.rs` AND changes `record_sentiment_signal` to use CAS.
RIGHT: commit 1 = move + delegating re-export; commit 2 = swap the implementation. Per Day 14 D8 / Day 15 incremental pattern.

### S28. Split read of `(bytes, version)` in `get_with_version`

WRONG: `let bytes = fs::read(path)?; let version = stat(path)?;` — race between read and stat.
RIGHT: hold the sidecar flock for the duration of read + stat (Q5).

### S29. `fd_lock::RwLock` held across `.await`

WRONG: `let _guard = lock.write()?; async_op().await;` — fd_lock is sync; the OS doesn't release the flock when the future suspends.
RIGHT: `tokio::task::spawn_blocking(move || { let _guard = lock.write()?; sync_op() }).await??`.

### S30. `Version` encoded as `String`

WRONG: `Version(String)` containing `"2026-05-13T12:34:56.789Z"` (mtime as ISO).
RIGHT: `Version(Box<[u8]>)` with opaque encoding — caller never inspects.

---

## Hard constraints check

- **NO AGPL/GPL/SSPL**: `dashmap` is MIT (verified). No other new deps in 16a. 16b adds no deps (`fd_lock` already direct).
- **File size ≤500 LOC**: `orchestrator.rs` estimated 400–500 LOC; if it exceeds, split as `orchestrator/{mod, state, signals, correction_window}.rs`.
- **`#[non_exhaustive]`**: `SessionState`, `SessionPhase`, `OrchestratorConfig`, new orchestrator-output types.
- **Sealed where engine-internal**: `Orchestrator` itself is NOT sealed (it's the concrete impl, not a trait). No new trait shipped in 16a.
- **Day 14 Context/Storage/EventSource MANDATORY**: orchestrator takes `&Context`; EventSource impl uses the trait; 16b storage migration uses `&dyn Storage`.

---

## Locked decisions for Day 16a learn-notes (proposed)

### D1. Split 16a / 16b

16a = orchestrator + EventSource impl + smoke wiring (no signal-write).
16b = `put_if_version` + `get_with_version` impls + lessons migration + signal-write hook in orchestrator.

### D2. Orchestrator state shape

`Arc<DashMap<SessionId, Mutex<SessionState>>>`. `SessionState` is a plain `#[non_exhaustive]` struct with `recent_turns`, `rate_limit`, `phase: SessionPhase`, `turn_count`. `SessionPhase` is `#[non_exhaustive] enum { Idle, AwaitingClassifier { ... } }`.

### D3. Per-session state keying

Keyed on `SessionId` only for 16a. Multi-tenant `(TenantId, SessionId)` deferred to SaaS-mode work. Per-lesson rate limit is `HashMap<LoadedItemId, Instant>` INSIDE `SessionState` — not a top-level map.

### D4. Rate limiting primitive

Hand-rolled `HashMap<LoadedItemId, Instant>` + cooldown check. NOT `governor`. Default cooldown 60s, configurable via `OrchestratorConfig`.

### D5. Lock discipline

`std::sync::Mutex` (not `tokio::sync::Mutex`) for `SessionState`. Critical sections never `.await`. Clippy lint `await_holding_lock` enabled for orchestrator module.

### D6. New dep

`dashmap = "6"` direct dep, MIT. Update `THIRD_PARTY_LICENSES.md`. No other new deps in 16a.

### D7. `JsonlWatcher` → `EventSource` impl

New `JsonlWatcherSource` struct in `src/host/claude_code/jsonl_watcher/source.rs`. Wraps existing `spawn_watcher`; bridges mpsc to `BoxStream` via `tokio_stream::wrappers::UnboundedReceiverStream`. Old `spawn_watcher` public API stays for backward compat.

### D8. Translation rules

- `WatcherEvent::UserTurn.parent_uuid` → `EngineEvent::UserTurn.parent_event_uuid`
- `cc_version` → `Some(HostVersion::new(cc_version))`
- `git_branch.or_else(cwd.file_name())` → `Some(ProjectTag::new(derived))` (host adapter derives per OQ5)
- `WatcherEvent::ParseError` → `EventSourceError::Transient`
- All other variants → typed `EngineEvent::*`

### D9. Hazard auto-abstain set

`Hazard::Sarcasm | Hazard::AmbiguousReferent | Hazard::OutOfDistribution` (and `Hazard::SelfDirected` if added — propose adding the variant to faithfully port TS).

### D10. Polarity-asymmetric thresholds

`POSITIVE_MIN: f32 = 0.75`, `NEGATIVE_MIN: f32 = 0.85` as named consts in orchestrator module. Cited to `sentiment-design-rules.md` rule 5.

### D11. Attribution cross-check

Orchestrator calls Day 15 `attribute_signal` (no fallback in 16a) and verifies `attribution.item_id == item.item_id`. If absent or mismatched, skip signal (audit-A2).

### D12. Correction-window mining

On `EngineEvent::UserInterrupt`, search recent_turns for last assistant turn that referenced items; if within `correction_window` (default 30s), emit negative signals on those items. Rule 15: skip when assistant didn't reference any item.

### D13. SignalWriter abstraction (16a-only)

16a defines `trait SignalWriter { async fn record(&self, ...) -> Result<()> }` as the orchestrator's output sink. 16a ships a `LoggingSignalWriter` (writes to `tracing` only) + `MockSignalWriter` (test-fixtures feature). 16b replaces with `StorageBackedSignalWriter` that calls `lessons::record_sentiment_signal` (CAS path).

### D14. Test strategy

- Pure-function unit tests inline `#[cfg(test)]` for `derive_signals`, hazard filter, correction window.
- Integration tests in `tests/orchestrator_*.rs` using `MockSentimentClassifier` + `MockSignalWriter` + `MemoryStorage`. Self-reference dev-dep (Day 15 M3 fix; should already land before 16a) lets tests see fixtures.
- Smoke test: `JsonlWatcherSource` → orchestrator → `MockSignalWriter` end-to-end with a synthesized JSONL.

### D15. File-size budget

`orchestrator.rs` target 400-500 LOC. If exceeded: split into `orchestrator/{mod, state, signals, correction_window}.rs`.

### D16. Audit clippy lints to enable for orchestrator module

- `clippy::await_holding_lock` (deny)
- `clippy::mut_mutex_lock` (warn)
- `clippy::significant_drop_in_scrutinee` (warn)

---

## Open questions to resolve in 16a learn phase

### OQ-D16a-1. Add `Hazard::SelfDirected` variant?

TS port faithfulness suggests yes; `Hazard` is `#[non_exhaustive]` so non-breaking. **Recommend YES** — add in 16a alongside the orchestrator's auto-abstain set.

### OQ-D16a-2. `SignalWriter` shape — trait or concrete?

A trait gives 16a a clean test seam (mock vs production); a concrete `LessonSignalWriter` defers the abstraction. **Recommend trait** — small surface, two impls (production + mock), clear seam for 16b replacement.

### OQ-D16a-3. Orchestrator output type

`Vec<SentimentSignal>`? `Result<OrchestratorOutput, OrchestratorError>` where `OrchestratorOutput { signals, abstained: bool, abstention_reason: Option<...> }`? The TS port has the latter (`SentimentSubagentOutput`). **Recommend** the structured-output type for auditability (Day 17 calibration will need the abstention_reason).

### OQ-D16a-4. `OrchestratorConfig` — separate type or part of an `EngineConfig` parent?

Day 14 didn't introduce a global config struct; each module owns its own. **Recommend** module-local `OrchestratorConfig` for 16a. Day 17+ may roll up into `EngineConfig` if a third config object appears.

### OQ-D16a-5. `recent_turns` capacity — fixed or configurable?

Design rules say "4-6 recent turns truncated to 800 tokens each." Capacity is configurable; **recommend** default 6 with `OrchestratorConfig.recent_turn_capacity: usize`.

### OQ-D16a-6. Turn-text truncation in orchestrator vs classifier?

Design rule 19: "Send 4-6 recent turns truncated to 800 tokens each." Truncation belongs to the classifier's request-building (so it's classifier-specific — different model windows). **Recommend** orchestrator stores full text in `recent_turns`; classifier truncates when building its prompt.

### OQ-D16a-7. `clippy::await_holding_lock` deny or warn?

`deny` for the orchestrator module specifically; `warn` crate-wide (other modules may have legitimate awaits-under-lock via `tokio::sync::Mutex`). **Recommend** module-scoped `#![deny(clippy::await_holding_lock)]` at the top of `orchestrator.rs`.

### OQ-D16a-8. `JsonlWatcherSource::run` shutdown task: spawn vs select

The sketch above spawns a small task that waits on `shutdown.cancelled()` to drop the handle. Alternative: `select!` between the stream and the shutdown token. **Recommend** spawn pattern — simpler, doesn't propagate the shutdown into the stream impl.

---

## Open questions to resolve in 16b learn phase (later)

### OQ-D16b-1. `Version` encoding — recommend `mtime_ns + len` = 24 bytes

See Q5. Spell it out in learn-notes; document the APFS-ms-resolution caveat.

### OQ-D16b-2. Move `lessons/lock.rs` to `storage/lock.rs`?

Recommend yes — storage's `put_if_version` consumer needs the same lock primitive; co-locating avoids the layer-violation. Re-export from `lessons::lock` for one cycle, retire in 16b's final commit.

### OQ-D16b-3. `EngineError` enum — module-level or crate-level?

**Recommend** crate-level (`engine::error::EngineError`); shared across `lessons`, `storage`, and (eventually) `orchestrator`.

### OQ-D16b-4. CAS-loop retry policy

Bounded vs unbounded? Recommend **bounded at 5 retries** with `EngineError::CasContended` on exhaustion. Single-user mode rarely contends; 5 retries handles transient cross-process races.

### OQ-D16b-5. `TestHarness` location

`engine::test_support` (crate-public) vs `tests/common/mod.rs` (test-binary-private)? **Recommend** the crate-public option behind `test-fixtures` feature — integration tests under `tests/*.rs` can import.

---

## Scope concerns for 16a-in-one-cycle

Estimated 16a build sizes:

| Component | LOC |
|---|---|
| `orchestrator.rs` | 400–500 |
| `JsonlWatcherSource` (`source.rs`) | 150 |
| `SignalWriter` trait + impls | 80 |
| Hazard set additions (`types.rs` patch) | 10 |
| `Cargo.toml` + license update | 20 |
| New tests (unit + integration) | 250 |
| **Total 16a** | **~900–1000 LOC** |

Concerns:

1. **`orchestrator.rs` size**. At 500 LOC, near the 500 LOC hard cap. If it exceeds, split into submodule (per D15).
2. **Smoke integration test flakiness**. End-to-end `JsonlWatcherSource → orchestrator → MockSignalWriter` test depends on FSEvents timing. Same risk Day 13 integration tests carry; mitigate with same timeout-and-drain pattern.
3. **`Hazard::SelfDirected` addition** breaks no existing code (variant added to `#[non_exhaustive]` enum) but the audit must verify the TS-side classifier prompt actually emits this hazard name (otherwise we add a variant nothing emits).
4. **Day 15 audit findings unfixed**. Day 15 audit M1 (`AsRef<str>` missing), M2 (stale doc), M3 (self-ref dev-dep), M4 (`TurnRole` not re-exported), M5 (magic number `0.8`). Recommend applying ALL Day 15 audit M-findings BEFORE 16a build starts — they're small, mechanical, and Day 16 builds on these types.
5. **`std::sync::Mutex` vs `parking_lot::Mutex`**. Recommend std for 16a (no new dep); revisit if perf testing in Day 17+ shows contention.

**Verdict:** 16a fits in one cycle if Day 15 audit findings are applied first, scope holds at orchestrator + EventSource, and the smoke test is timeout-gated.

---

## Scope concerns for 16b (next cycle, surfaced now)

1. **Cross-process flock-vs-CAS semantics**. The TS MCP server's flock pattern MUST be verified compatible with the Rust CAS pattern (Q5 sub-question). 16b pre-research expands this with a survey of the TS lock.ts current behavior + a paired integration test.
2. **`TestHarness` introduction touches ~7 tests**. Migration order: loader tests first (read-only), signals tests second (write path), then ENV_LOCK retirement.
3. **`anyhow → EngineError`** migration scope. New API uses `EngineError`; delegating wrappers convert `EngineError -> anyhow::Error` for one cycle.
4. **Day 14 stubs replaced**. The two test functions `put_if_version_returns_backend_error_in_phase_3b` (line 331 of `filesystem.rs`) and any equivalent for `get_with_version` retire when the implementations land.

---

## Sources / crate versions cited

- `regex` 1.11.1 — already direct dep. MIT/Apache.
- `dashmap` 6.x (Oct 2024) — proposed new direct dep. MIT.
- `tokio-stream` 0.1.x — already transitive of `tokio_util`. MIT.
- `governor` 0.6.x — surveyed; not selected. MIT/Apache.
- `tower` 0.5.x — surveyed for `tower::limit`; wrong shape. MIT.
- `ractor` 0.13.x — surveyed; not selected. MIT.
- `parking_lot` 0.12.x — surveyed; not selected for 16a (std::sync::Mutex sufficient). MIT/Apache.
- `async-trait` 0.1.x — already direct dep. MIT/Apache.
- `fd-lock` 4.x — already direct dep. MIT/Apache.
- `tokio_util` 0.7.x — already direct dep. MIT.
- `futures` 0.3.x — already direct dep. MIT/Apache.

No AGPL/GPL/SSPL dependencies recommended. `dashmap` MIT verified at crates.io as of 2026-05-13.

---

## Related

- [[feedback-rust-idiomatic-refactor]] — the hard rule.
- `docs/research/day-15-pre-research.md` Q3 (classifier trait), Q4 (attribution).
- `docs/research/day-15-learn-notes.md` D1-D15 locked types.
- `docs/research/day-15-post-research.md` L1-L8 forward-feeds + OQ-D16-1 through OQ-D16-7.
- `docs/research/day-15-audit-report.md` — M1-M5 must-fix-before-16a.
- `docs/research/day-14-pre-research.md` Q4 (EventSource), Q5 (migration strategy), Q8 (testing).
- `docs/research/day-14-learn-notes.md` D8 two-phase migration, D7 test strategy.
- `docs/research/sentiment-design-rules.md` rules 5, 8, 13–16, hazard list.
- `loop-archive-2026-05-13/core-ts/src/sentiment/orchestrator.ts` — TS reference (the *what*, not the *how*).
- `src/engine/sentiment/{types,classifier,attribution,pretrigger,mod}.rs` — Day 15 build outputs.
- `src/engine/lessons/{loader,signals,lock}.rs` — Day 11/12 build outputs (16b migration targets).
- `src/engine/storage/{filesystem,memory,version}.rs` — Day 14 build outputs (16b implements stubs).
- `src/host/claude_code/jsonl_watcher/{runner,events,parser,cursor}.rs` — Day 13 build outputs (16a wraps).
