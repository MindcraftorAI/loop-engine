# Day 14 Learn Notes: Locked Decisions for Build Phase

**Date:** 2026-05-13
**Cycle phase:** Learn (workflow cycle phase 2)
**Cycle:** Day 14 (single-crate module restructure + Context/Storage/EventSource)
**Source pre-research:** `docs/research/day-14-pre-research.md` (1567 lines)

These decisions are LOCKED. Build phase consumes them as input; they do not get revisited mid-build absent a fundamental discovery. Per [[feedback-rust-idiomatic-refactor]], the design target was idiomatic Rust (tower/hyper/object_store/opendal patterns), not TS transliteration.

---

## Locked decisions (from pre-research, no contention)

### D1. Module organization
- `src/engine/` (host-agnostic, "to-be-extracted-as-loop-engine") + `src/host/claude_code/` (the JSONL watcher and future Haiku/Auto Memory adapters) as **plain modules** — NOT Cargo features, NOT a workspace.
- Boundary enforced by lint + CI grep, not type system (cheaper).
- Edition stays `2021`; 2024 bump is a separate audit.
- Final layout per pre-research Q1.

### D2. Context shape
- `struct Context { tenant_id: TenantId, user_id: UserId, session_id: SessionId, team_id: Option<TeamId> }`
- `#[non_exhaustive]` so adding fields later is non-breaking.
- IDs are `Arc<str>` newtype wrappers (`TenantId`, `UserId`, `SessionId`, `TeamId`) — cheap to clone, type-safe.
- Pass as `&Context` everywhere; never thread-local, never env.
- `Context::single_user_local()` is the day-one default (collapses to current behavior).

### D3. Storage trait
- Object-safe `dyn Storage` (not generic `<S: Storage>`).
- `async_trait` macro for the async fn signatures.
- Fixed `StorageError` enum (not associated `type Error;`).
- Custom `StorageKey` newtype (not `&str`, not `PathBuf`).
- Two impls ship in Day 14: `LocalFsStorage` (production, replaces existing filesystem ops) and `MemoryStorage` (test fixture).

### D4. Sealed trait
- `Storage: sealed::Sealed` — external impls of the engine's storage surface forbidden.
- Backends added inside the crate only. The pattern is a private `sealed` module with a `Sealed` trait that nothing outside the crate can impl.

### D5. EventSource trait
- Factory pattern returning `BoxStream<Result<EngineEvent, EventSourceError>>`.
- `CancellationToken` for shutdown (already in our deps tree via tokio-util).
- `JsonlWatcher` becomes the first impl, living in `host::claude_code::jsonl_watcher`.
- The Day 13 watcher (`src/watcher/`) gets moved to `src/host/claude_code/jsonl_watcher/` and refactored to implement the trait.

### D6. Public surface
- `lib.rs` curates a small prelude of engine essentials (Context, Scope, Storage, StorageError, EngineEvent, EventSource).
- Full module paths still work.
- `host::*` is unstable — break freely.
- Engine items get a `cargo-public-api` snapshot.

### D7. Test strategy
- `TestHarness` with per-test tempdir backing `LocalFsStorage` for integration tests.
- `MemoryStorage` for pure-logic tests (no filesystem needed).
- Drop `with_temp_loop_home` + `ENV_LOCK` once all callers migrated.

### D8. Migration phasing — two-phase, leaf-first
- **Phase 1 (this Day 14):** abstractions land first with delegating wrappers preserving the old API for callers not yet migrated. Existing tests keep passing throughout.
- **Phase 2 (Day 14 continued OR Day 15 alongside sentiment):** module-by-module migration of callers, leaf-first: `yaml/buffer/pid` (already stateless or trivially refactorable) → `lifecycle` → `lessons` → `watcher` (already moves to `host/` in Phase 1, but its consumers — lessons-write-signal — migrate later).

### D9. Cargo edition
- Stay on `edition = "2021"` for Day 14.
- Edition 2024 bump is a separate audit; deferred.

### D10. Dependencies added
- `async-trait` (MIT/Apache) — for `Storage` trait async methods
- `bytes` (MIT) — already a transitive of `tokio`/`reqwest`; promote to direct dep for `StorageKey` / payload types
- `futures` (MIT/Apache) — for `BoxStream` in EventSource
- `dashmap` (MIT) — deferred to build phase; only if `MemoryStorage` benchmarks meaningfully prefer it over `Mutex<HashMap>`

---

## Open-question decisions (accepting all pre-research recommendations)

### OQ1. `team_id` in Context from day one? → YES
Carry as `Option<TeamId>`. Cost is zero today; prevents "oh-we-need-it-everywhere" later. Adding fields to `#[non_exhaustive]` is non-breaking but consumer code still has to grow with it.

### OQ2. `agent_id` separate or part of session_id? → COLLAPSED into SessionId
TS-side `MemoryScope::agent_shared`/`agent_private` exists, but at the Context layer the agent is part of the same session. Break out later if needed; YAGNI today.

### OQ3. `MemoryStorage` ships in Day 14? → YES, ships
The multi-tenant routing tests it enables are exactly the safety net we need to land `Context` confidently. ~150 LOC well-spent.

### OQ4. `cargo-public-api` snapshot CI policy → OPT-IN for Days 14-16, GATING from Day 17
Log diff on mismatch but don't fail CI for Days 14-16 (engine surface still settling). Promote to gating when sentiment work stabilizes at Day 17.

### OQ5. Naming: `loop_daemon::engine` vs eventual `loop_engine` → KEEP `loop_daemon::engine`
The crate name is `loop-daemon` (per Cargo.toml). The rename to `loop-engine` is a separate decision when extraction-to-crates.io happens. Module path is `loop_daemon::engine::*`.

### OQ6. `async-trait` macro or hand-rolled `Pin<Box<dyn Future>>` → MACRO
`async_trait = "0.1"` is the convention. Reject only if build phase finds it doesn't compose with `BoxStream` cleanly — confirm at build kickoff.

### OQ7. Storage CAS: `put_if_version` cloud-primitive or `lock`/`unlock` primitives → `put_if_version`
Ship the cloud-shaped primitive; implement it via flock+sidecar internally in `LocalFsStorage`. Day 12's logic moves wholesale, no behavior change. The cloud-shape is forward-compatible with S3/Postgres backends; the lock/unlock shape isn't.

---

## Build phase scope (what gets built in Day 14)

**In scope (Phase 1 abstractions):**

1. Restructure `src/` into `engine/` + `host/claude_code/` per pre-research Q1 layout. Move existing modules without API shape changes:
   - `src/lessons/` → `src/engine/lessons/`
   - `src/yaml/` → `src/engine/yaml/`
   - `src/lifecycle.rs` → `src/engine/lifecycle.rs`
   - `src/buffer.rs` → `src/engine/buffer.rs`
   - `src/pid.rs` → `src/engine/pid.rs`
   - `src/paths.rs` → `src/engine/paths.rs` (becomes `pub(crate)`)
   - `src/watcher/` → `src/host/claude_code/jsonl_watcher/` (becomes EventSource impl)
2. Update `src/lib.rs` to declare `engine` + `host` and curate a small prelude.
3. Create new modules:
   - `src/engine/context.rs` — `Context`, `TenantId`, `UserId`, `SessionId`, `TeamId`, `Scope`, `Context::single_user_local()`
   - `src/engine/storage/mod.rs` — `Storage` trait (sealed, async), `StorageKey`
   - `src/engine/storage/filesystem.rs` — `LocalFsStorage`
   - `src/engine/storage/memory.rs` — `MemoryStorage`
   - `src/engine/storage/error.rs` — `StorageError` enum
   - `src/engine/events.rs` — `EventSource` trait, `EngineEvent`, `EventSourceError`
4. Refactor `JsonlWatcher` to implement `EventSource` (returns `BoxStream<Result<EngineEvent, EventSourceError>>` instead of its current ad-hoc mpsc shape).
5. Add deps to `Cargo.toml`: `async-trait`, `bytes`, `futures`. (`dashmap` deferred.)
6. License audit (none of the new deps are AGPL/GPL/SSPL; verify SPDX).
7. Update `THIRD_PARTY_LICENSES.md` to declare the new deps.
8. CI lint: add a check that `grep -r 'crate::host' src/engine` returns nothing (boundary enforcement).
9. All 127 existing tests MUST continue to pass. The migration is incremental; old API surface stays via delegating wrappers.
10. Apply Day 13 audit fixes (A1/A2/A3/A4/A5) **in the new** `src/host/claude_code/jsonl_watcher/` location. (Task #44 closes here.)

**Out of scope for Day 14 (Phase 2 territory):**
- Migrating `lessons/loader.rs`, `lessons/signals.rs`, `lifecycle.rs` etc. to take `&Context` parameters. Delegating wrappers keep them working with the old shape until Phase 2.
- Sentiment module work (Day 15).
- Anthropic Haiku client (later).
- `cargo-public-api` snapshot setup (opt-in mode = just add the tooling; gating from Day 17).

---

## Migration order (leaf-first, per D8)

Within Day 14's Phase 1 scope, the file-moves can happen in this order to keep `cargo test` green at each step:

1. `engine/yaml/` (no state, no callers care about Context) — moves cleanly
2. `engine/buffer.rs`, `engine/pid.rs` (no state) — moves cleanly
3. `engine/paths.rs` (becomes pub(crate); legacy public API stays as a wrapper for one cycle)
4. `engine/context.rs` (new file) — defines all the types but no callers yet
5. `engine/storage/*` (new files) — defines trait + impls but no callers yet
6. `engine/events.rs` (new file) — defines EventSource trait but no impls yet
7. `engine/lessons/` (move; existing public functions keep their non-Context signature for now via wrappers)
8. `engine/lifecycle.rs` (move; same wrapper pattern)
9. `host/claude_code/jsonl_watcher/` (move from `src/watcher/`; ALSO refactor to impl `EventSource` AND apply Day 13 audit fixes A1-A5)
10. `lib.rs` curated prelude
11. CI lint addition

After each step: `cargo test --all` green.

---

## Audit checklist for the Day 14 audit phase

The audit agent will receive this checklist and the 17 TS-with-Rust-syntax smells from pre-research (Q1-Q8 audit-smells sections + the final smells section).

**Must verify:**
- [ ] 127 tests still pass (no regressions during the move)
- [ ] No `crate::host` references inside `src/engine/`
- [ ] All new public engine items are documented (rustdoc) and have `#[non_exhaustive]` where appropriate
- [ ] Day 13 audit fixes A1-A5 applied in the new jsonl_watcher location:
  - [ ] A1: pre-existing files don't replay full content (tail-from-now respected)
  - [ ] A2: process_cursor advances by actually-read bytes (not requested bytes)
  - [ ] A3: MAX_APPEND_READ doesn't cause permanent stall (loop over chunks until file caught up OR cap)
  - [ ] A4: `THIRD_PARTY_LICENSES.md` declares `notify` (CC0-1.0)
  - [ ] A5: SessionStarted fires for pre-existing files seen on watcher startup
- [ ] License check: `async-trait`, `bytes`, `futures` are MIT/Apache. No AGPL/GPL/SSPL.
- [ ] File size limit: no new file >500 LOC. Storage module probably approaches this — split early if needed.
- [ ] `Storage::sealed` actually prevents external impls (a test that tries to impl Storage outside the crate, expected to fail compile, recorded via `trybuild`)

**Must check for TS-with-Rust-syntax smells** (full list in pre-research lines 1475-1556):
- `Arc<RwLock<HashMap<String, Context>>>` as a context registry
- `Box<dyn Error>` at engine boundaries
- `anyhow::Error` in engine public function signatures
- `Option<Option<T>>`
- `Vec<Box<dyn Trait>>` where a closed-set enum is more accurate
- `pub fn new(...)` constructors that just assign fields with no construction logic
- `async fn` that doesn't `await`
- `String` where `&str` would work
- Stringly-typed scope/status fields
- `Mutex<()>` as "lock without value"
- Unnecessary `Arc<Mutex<T>>` (audit each: could be `&mut T`? mpsc? `Arc<T>` because read-only?)
- `if let Some(x) = ... { } else { return Err(...) }` (use `?`)
- Manual byte iteration for UTF-8 (use `str::chars` / `bstr`)
- Visitor-pattern transliterations (closures + iterators)
- `tokio::runtime::Handle` field (engine receives work; doesn't own its executor)

---

## What this learn-notes does NOT decide

- Specific function-level Context refactor for `lessons::loader` — that's Phase 2 / Day 15.
- The exact wrapper-shedding cadence — Day 14 lands Phase 1, Phase 2 cadence is decided at Day 14's post-research.
- Sentiment module structure — covered in Day 15 pre-research.

---

Related: [[feedback-workflow-cycle]], [[feedback-rust-idiomatic-refactor]], [[project-2026-05-13-restructure-plan]], `docs/research/day-14-pre-research.md`
