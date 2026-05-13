# Day 16a Audit Report

**Cycle:** Day 16a (orchestrator + JsonlWatcher EventSource impl + SignalWriter)
**Audit window:** commits `6cde320..8fcb029`
**Phase:** 5 (audit — backward-looking)
**Date:** 2026-05-13

**Build status at audit time:** `cargo test --all-targets` → 221 unit + 3 integration tests pass (200 → 221 = +21 new unit, +0 new integration). `cargo clippy --all-targets` clean. `cargo clippy --all-targets --features test-fixtures -- -D warnings` clean. No new transitive AGPL/GPL/SSPL deps.

---

## CRITICAL findings

### C1. `expect("session must exist after critical section 1")` is a real concurrent-panic vector

`src/engine/sentiment/orchestrator.rs:256-257` — between critical section 1 (line 208, inserts session) and critical section 2 (line 253, re-reads session), the orchestrator awaits the classifier (line 234). A concurrent `EngineEvent::SessionEnded` for the same `SessionId` will hit `process_event` → `self.inner.sessions.remove(session_id)` (line 187) on a DIFFERENT task while critical section 1's clone of the orchestrator is awaiting. When the classifier resumes, the `.get(...)` returns `None` and `.expect(...)` panics, taking down the tokio worker.

Why this is plausible (not theoretical): the orchestrator is documented `Clone` (line 134-136: "designed to be passed by value into spawned tasks"). The intended deployment pattern is N spawned tasks each calling `process_event`. The host emits `SessionEnded` when the JSONL file is removed, which can race with a still-in-flight classifier call for the same session's last UserTurn.

Recommended fix: replace the `.expect` with `.ok_or(_)` semantics that turn the missing-session case into an abstain. Either:

```rust
let Some(entry) = self.inner.sessions.get(&ctx.session_id) else {
    return OrchestratorOutput {
        signals: vec![],
        abstentions: vec![(None, AbstainReason::ClassifierAbstained /* TODO: SessionGone variant */)],
    };
};
```

Or — preferred — re-acquire via `.entry(...).or_default()` (preserving the post-end-then-restart case where a new session reuses the ID rarely-but-possibly). Add a dedicated `AbstainReason::SessionGone` variant for clarity.

### C2. `handle_user_interrupt` signal-emit path is dead code in 16a (and its only test exercises the trivially-true gate)

The function has a 38-LOC "build negative signals" branch (lines 331-355) gated by `let Some(turn) = proximal` (line 309, requires an Assistant-role recent turn with `referenced_items`) AND `within_window` (line 319, requires `state.last_assistant_turn_at = Some(_)`). Both predicates require an assistant turn to have been pushed into `recent_turns` — but **`push_turn` is only ever called from `handle_user_turn` with `TurnRole::User`** (line 213-217). There is no code path in 16a that pushes an Assistant turn or sets `last_assistant_turn_at`.

Consequence: every `UserInterrupt` event in production will take the early-return at line 312-315 (`NoProximalReference`). The 38 LOC of negative-signal-emit logic is structurally unreachable, and the only integration test (`orchestrator_user_interrupt_no_proximal_assistant_abstains` at line 847-860) exercises exactly the dead-code-avoiding path. There is zero test coverage for the lines that actually emit signals.

The audit prompt flagged this directly: "handle_user_interrupt: correction_window check uses last_assistant_turn_at — but does it ever get SET? ... Verify test exercises only the 'no proximal' path or document the gap." The gap is undocumented in either the code or the learn-notes. Two options:

1. **Document explicitly** (preferred for 16a since assistant-turn ingestion is correctly deferred). Add a module-level comment in `orchestrator.rs` AND in `handle_user_interrupt`'s docstring that says: "Until 16b+ wires assistant-turn observation (currently only the JSONL UserTurn path is hooked), the post-proximal branch is structurally unreachable; the AbstainReason::NoProximalReference path is the only live behavior." Add a `#[cfg(test)]`-only test that injects a `RecentTurn` with `TurnRole::Assistant` directly into `SessionState` and exercises the emit path so it doesn't drift.
2. **Cut the dead code** — keep only the proximal-not-found path; restore the emit path in 16b when assistant-turn observation actually lands. This is more disciplined under the "don't ship dead code" principle.

Either way, do NOT leave the build with a 38-LOC un-testable branch and no acknowledgement.

---

## MAJOR findings

### M1. `orchestrator.rs` is 861 LOC (522 prod, 339 test) — blows past D15's 400-500 LOC budget

`wc -l src/engine/sentiment/orchestrator.rs` → 861 total. Production code (lines before `#[cfg(test)] mod tests`) is 522 LOC. D15 explicitly said: "`orchestrator.rs` target 400-500 LOC (under 500 hard limit). Split into `orchestrator/{mod, state, signals, correction_window}.rs` if exceeded."

The build IS over the hard 500 limit on prod code alone. The split should happen now per the D15 plan. Suggested split:

- `orchestrator/mod.rs` — public surface: `Orchestrator`, `OrchestratorConfig`, `process_event` dispatcher
- `orchestrator/state.rs` — `SessionState`, `SessionPhase`, `push_turn` helper
- `orchestrator/derive.rs` — `derive_signals`, `is_auto_abstain_hazard`, threshold consts
- `orchestrator/handlers.rs` — `handle_user_turn`, `handle_user_interrupt`

The Display-wrapper helpers (`ClassifierErrorDisplay`, `SignalWriteErrorDisplay`) can live in `mod.rs`.

Why it matters: the file is monolithic enough that the audit prompt's S22/S23/critical-section-discipline checks took two passes to verify. Future readers will need the same effort. The audit prompt also called this out as a Phase 5 verification item.

### M2. No integration smoke test for the wired orchestrator path

Learn-notes D14: "Integration in `tests/orchestrator_*.rs` using `MockSentimentClassifier` + `MockSignalWriter` + `MemoryStorage`. Smoke: `JsonlWatcherSource → orchestrator → MockSignalWriter` end-to-end with synthesized JSONL." Audit checklist line 159-160 reiterates: "Integration tests in `tests/orchestrator_smoke.rs` using fixtures + smoke."

`ls tests/` shows three files — `byte_fixture.rs`, `concurrent_signal_writes.rs`, `ts_lesson_roundtrip.rs`. There is NO `tests/orchestrator_*.rs`. The 13 orchestrator tests in `orchestrator.rs` are all inline `#[cfg(test)] mod tests` — they exercise pure-function rules (10 of them) and three integration paths that never produce a single emitted signal.

Why it matters: with no `JsonlWatcherSource → translate → process_event → MockSignalWriter` end-to-end test, a regression that breaks the translation, the dispatch, or the writer plumbing will land green. The smoke test is the primary value-add of 16a (the "wiring" cycle); shipping without it leaves the wiring unverified.

Recommended fix: add `tests/orchestrator_smoke.rs` that:
1. Constructs a `MemoryStorage`-free orchestrator with `MockSentimentClassifier` configured via `.with_response(canned)` and a `MockSignalWriter`.
2. Synthesizes a `WatcherEvent::UserTurn` via the test fixture path, runs it through `translate`, then through `orch.process_event(...)`.
3. Asserts the `MockSignalWriter.captured()` contains the expected `SentimentSignal`.

This also closes the "production code can emit a signal" coverage gap behind C2's documented dead branch.

### M3. Signal-emit path is completely untested at the integration level (no positive-confidence path)

All three `#[tokio::test]` `process_event` integration tests use the default `MockSentimentClassifier` (no `.with_response(...)`), so the classifier always returns `RawClassification::abstain()`. The three tests verify:
- session creation/teardown
- abstain-on-empty propagates
- user-interrupt-with-no-proximal abstains

None of them inject a canned `RawClassification` and verify that the orchestrator (a) derives a `SentimentSignal`, (b) calls `writer.record(...)`, (c) records `signals.len() == 1`, AND (d) updates `state.rate_limit`. This is the same Day-15-audit-m3 risk: silent-abstain masks orchestrator bugs.

Why it matters: a substantial bug in the threshold-gate, the rate-limit insertion, the attribution cross-check, or the writer plumbing would not be caught by `cargo test`. The pure-function `derive_signals` tests cover the rules but not their wiring through `process_event`.

Recommended fix: add at minimum two `process_event`-level tests:
1. `orchestrator_emits_positive_signal_when_classifier_confident` — inject a canned `RawClassification` with a positive-conf item, populate `loaded_items` in the request setup (requires either threading them through `EngineEvent::UserTurn` or extending the mock-orchestrator setup), assert one `SentimentSignal` captured by `MockSignalWriter` with the right polarity/method/confidence.
2. `orchestrator_rate_limits_repeat_signal_within_cooldown` — fire two consecutive UserTurns each producing the same positive signal, assert the second is rate-limited via `AbstainReason::RateLimited`.

This requires solving the "loaded_items is hard-coded empty in process_event" issue — see M4.

### M4. `ClassificationRequest::loaded_items` is hard-coded `Vec::new()` — the orchestrator can never attribute in production

`src/engine/sentiment/orchestrator.rs:228` builds the classifier request with `loaded_items: Vec::new()`. The comment says "manifest assembly lives elsewhere and is wired in 16b+ alongside lessons migration." But `derive_signals` (line 457) calls `attribute_signal(utterance, &request.loaded_items, &request.recent_turns)` — with an empty items slice, attribution **always returns `None`** (no candidate items to match against), which trips `AbstainReason::AttributionAbstained` at line 459.

Consequence: even if a classifier returns a high-confidence per-item classification, the orchestrator will abstain via attribution-mismatch on every signal in 16a. The orchestrator's signal-emit path is end-to-end unreachable in production code, mirroring C2.

This is a downstream effect of "no manifest assembly in 16a" being correct but un-flagged: the orchestrator was wired without ever exercising the success path. Combined with M3 (no positive-path test) it means 16a ships an orchestrator that **never emits a signal in any conceivable scenario.**

Recommended fix:
- For tests: the test orchestrator setup should inject a manifest-provider seam (a `Fn(&ctx) -> Vec<LoadedItem>` closure, or accept `loaded_items` in `OrchestratorConfig` for now). This closes M3.
- For 16b: the manifest-provider seam becomes the real lessons-layer call.
- Either way, document in module docs that **16a's `process_event` always produces an empty signal vector in production until 16b lands the manifest provider.** Failing to document this leaves a maintainer assuming 16a is "feature-complete enough to demo" when in fact it isn't.

### M5. D16's three module-scoped lints — only two are applied

Learn-notes D16:
```
#![deny(clippy::await_holding_lock)]
#![warn(clippy::mut_mutex_lock)]
#![warn(clippy::significant_drop_in_scrutinee)]
```

`src/engine/sentiment/orchestrator.rs:19-20` ships only:
```
#![deny(clippy::await_holding_lock)]
#![warn(clippy::significant_drop_in_scrutinee)]
```

`clippy::mut_mutex_lock` is missing. The lint catches `mutex.lock().unwrap().push(...)` when you held `&mut mutex` and could have used `.get_mut()`. Low-impact in 16a (the orchestrator passes mutexes by `&` only), but the audit checklist (line 173) and learn-notes both explicitly require all three lints. Inconsistency now will compound — the next learn-notes that adds a lint won't catch the missing one.

Recommended fix: add `#![warn(clippy::mut_mutex_lock)]` at line 21. One line. Trivial.

### M6. `SessionState` and `SessionPhase` are `pub` and re-exported — internal types leaking

`src/engine/sentiment/orchestrator.rs:94` (`pub struct SessionState`), `:119` (`pub enum SessionPhase`), and `mod.rs:24` (`pub use orchestrator::{Orchestrator, OrchestratorConfig, SessionPhase, SessionState}`).

These are internal state shapes that the orchestrator's own dashmap owns. External consumers (Day 17 solicitor, integration tests) only need `Orchestrator`, `OrchestratorConfig`, and `OrchestratorOutput`. Day 15 audit M4 caught the inverse gap (`TurnRole` not re-exported); this audit catches the inverse: SessionState/SessionPhase are exposed but should not be.

Day 15 audit's reasoning applies in reverse: the "module IS the public surface" rule means re-exports should only include the customer-facing API. `SessionState::default()` being publicly constructible lets external code build state that the orchestrator can't reason about (e.g. with mismatched recent_turns capacity).

Recommended fix: change both `pub struct SessionState` and `pub enum SessionPhase` to `pub(crate)`. Remove from `mod.rs:24`'s `pub use` block. Test code inside `orchestrator.rs` (#[cfg(test)] block) still has visibility.

### M7. `OrchestratorOutput` field shape diverges from learn-notes OQ-D16a-3

Learn-notes OQ-D16a-3 locked: `OrchestratorOutput { signals: Vec<SentimentSignal>, abstained: bool, abstention_reason: Option<AbstainReason> }`.

`src/engine/sentiment/signals.rs:81-87` ships: `OrchestratorOutput { signals: Vec<SentimentSignal>, abstentions: Vec<(Option<LoadedItemId>, AbstainReason)> }`.

The shipped shape is RICHER — it supports per-item abstention reasons and multiple abstentions per event (correction-window fan-out), which is the correct model. But it's a divergence from the locked decision text that should have been called out either in the build commit message or in a post-research note. The audit prompt specifically flagged "OQ-D16a-3: OrchestratorOutput is structured (signals + abstentions)" as a verification item — and the structured form ships, but with different field names than locked.

Why it matters: locked decisions are the source of truth between cycles. Silent improvements drift the spec. The improvement here is good, but the workflow expectation (D-spec → build → audit reconciles) wants the divergence acknowledged.

Recommended fix: add a post-research note (`day-16a-post-research.md` or appended to learn-notes) that supersedes OQ-D16a-3 with the new shape and the rationale (correction-window fan-out needs multiple reasons). Going forward, the new shape is the locked spec.

---

## MINOR findings

### m1. `text.clone()` called three times in `handle_user_turn`

`src/engine/sentiment/orchestrator.rs:215, 220, 227` — the same `&String text` is cloned three times in immediate sequence. Cheap (each clone is one heap allocation), but obvious cleanup:

```rust
let text_owned: String = text.clone();
// ... use text_owned by clone() for the request/phase
```

Or restructure so push_turn and phase-set borrow text and the request takes ownership. Defer if Day 16b refactor splits the function.

### m2. `derive_signals` returns `(Vec<_>, Vec<_>)` tuple — `result.0` accessor is awkward

`src/engine/sentiment/orchestrator.rs:260-264`:

```rust
let result =
    derive_signals(&raw, &request, &state.rate_limit, &self.inner.config, now, event_uuid);
for sig in &result.0 {
    state.rate_limit.insert(sig.item_id.clone(), now);
}
```

Three readability misses: anonymous tuple positionality, `.0` accessor, no destructure. The pure-function tests use `let (sigs, abst) = derive_signals(...)` and that's clearer. A named-tuple-struct or returning `OrchestratorOutput` directly would fix both this and m1 site.

Recommended fix: change `derive_signals` to return `OrchestratorOutput` directly. The function name lies about what it returns ("signals" → returns abstentions too); having it return the orchestrator's output type centralizes the shape.

### m3. `proximal` borrow scope creates fragile read-then-mut-borrow on `state`

`src/engine/sentiment/orchestrator.rs:303-353` — `proximal` is `Option<&RecentTurn>` borrowed from `state.recent_turns`. The branch then proceeds to mutate `state.rate_limit` (line 351) and read `turn.referenced_items.len()` (line 332). The compiler accepts this because the borrow ends after `turn.referenced_items.clone()` on line 334, but any future code edit between lines 332-334 that uses `turn` again will fight the borrow checker.

Recommended fix: defensively snapshot `turn.referenced_items.clone()` at the top of the matching branch (after the `proximal` find), drop the borrow, then proceed. Or extract referenced_items into a local before line 332. Pre-emptive — Rust catches the regression, but the code expresses tighter intent.

### m4. AbstainReason::PretriggerNotFired is unreachable in 16a (pretrigger not wired)

`src/engine/sentiment/signals.rs:54-55` defines the variant; the orchestrator (line 234) calls the classifier unconditionally without a pretrigger fast-path. Pre-research (line 1452) and learn-notes do not require pretrigger wiring for 16a, so this is arguably correct deferral. But the variant exists with no production caller, which is exactly the dead-code carryover smell flagged by Day 14 audit m8.

Recommended fix: either wire pretrigger before the classifier call (it's a one-liner: `if !self.pretrigger.fires(text) { return abstain(PretriggerNotFired) }`) or remove the variant until pretrigger lands. Variant exists with no caller is a smell-cousin of removed-dead-code.

### m5. `LoggingSignalWriter` writes `polarity = ?signal.polarity` (Debug formatter) — inconsistent with the Display wrappers elsewhere

`src/engine/sentiment/signals.rs:147-158` uses `?` (Debug) for `polarity`, `method`, and `hazards` — but builds `ClassifierErrorDisplay` / `SignalWriteErrorDisplay` wrappers elsewhere (orchestrator.rs:506-517) so error fields are Display-formatted for grep-ability. The Debug-formatted enum names are still grep-friendly (`Positive` / `Negative` / `Neutral`), so this is acceptable, just inconsistent.

Recommended fix: accept current shape. If a future log analysis tool wants `polarity="positive"` instead of `polarity=Positive`, add a `Display` impl on `Polarity`.

### m6. `derive_signals` allocates a `HashSet<LoadedItemId>` for dedup at every call

`src/engine/sentiment/orchestrator.rs:411`: `let mut seen: HashSet<LoadedItemId> = HashSet::new();`. The function is called once per UserTurn — bounded, ≤ ~20 items by manifest size. The HashSet allocation costs more than a `Vec::contains` lookup at this size.

Recommended fix: switch to `Vec<&LoadedItemId>` for dedup, or use `with_capacity(raw.per_item.len())` if HashSet is preferred. Low-priority perf nit; minor allocations on a hot-ish path.

### m7. `is_auto_abstain_hazard` is duplicated logic relative to `AbstainReason::HazardSet`

The set `{Sarcasm, AmbiguousReferent, OutOfDistribution, SelfDirected}` is hard-coded in `is_auto_abstain_hazard` (line 493-501). It's the same set referenced by D9. A future maintainer adding a new auto-abstain hazard has to remember to touch both the function AND the design-rules doc.

Recommended fix: move the set to an associated `Hazard::is_auto_abstain(&self) -> bool` method on the enum, so the policy lives next to the variant definition. Or `const AUTO_ABSTAIN_HAZARDS: &[Hazard] = &[...]` at module scope. Low priority.

### m8. JsonlWatcherSource `filter_map(|w_evt| async move { Some(translate(w_evt)) })` — allocation per event

`src/host/claude_code/jsonl_watcher/source.rs:72-73`: every event goes through an async future from filter_map, even though `translate` is synchronous and never filters (returns `Some(_)` unconditionally). The async-block overhead is one stack-frame per event.

Pre-research smell S21 ("Allocating-per-event in the stream pipeline") — the audit prompt's S21 specifically called out this filter_map chain. The simpler `receiver_stream.map(translate)` would skip the async overhead entirely.

Recommended fix:
```rust
let translated = receiver_stream.map(translate);
```

This is `futures::StreamExt::map` (already in scope) which is a synchronous combinator — no allocation per event. The `filter_map` was over-applied here; the `Option<_>` filtering was a red herring (translate never returns None). One-line cleanup.

### m9. `Orchestrator::session_count()` is `#[doc(hidden)] pub` — should be `pub(crate)`

`src/engine/sentiment/orchestrator.rs:378-381`. `#[doc(hidden)]` hides it from rustdoc but the symbol is still part of the public API surface. Any external consumer can call it. The audit prompt explicitly flagged: "session_count() public — should it be #[doc(hidden)] or `pub(crate)`?"

Why it matters: external callers that come to depend on `session_count()` for behavior pin the orchestrator's session-set semantics. The internal dashmap shape becomes a contract.

Recommended fix: change to `pub(crate) fn session_count(&self) -> usize`. Tests in the same crate still see it. Drop `#[doc(hidden)]` since `pub(crate)` already excludes from rustdoc.

### m10. `MockSignalWriter::with_record_error` consumes `self` then locks the Mutex — Day 15 m4 pattern repeated

`src/engine/sentiment/signals.rs:196-202` — same chained-by-move-with-mutex pattern Day 15 audit m4 flagged on `MockSentimentClassifier`. Lock is uncontended at builder-chain time. Acceptable for consistency with the existing pattern, but worth noting that it's a knowing repeat of m4.

### m11. `SignalWriteError::Backend(#[source] Box<dyn std::error::Error + Send + Sync>)` — same Box-of-trait-object pattern as Day 14 audit m7

Pre-research smell list didn't include this for 16a specifically, but Day 14 m7 noted: prefer a named source variant if the upstream error space is closed. Today the only `Backend` source is `MemoryStorage` / future `Storage::put_if_version` errors — closed enough that a `#[from] StorageError` would type more cleanly. Defer to 16b when the real storage backend lands.

### m12. `MockSignalWriter` has `error: Mutex<Option<SignalWriteError>>` — `SignalWriteError` is not `Clone`, comment says "one-shot"

`src/engine/sentiment/signals.rs:178-181, 196-202, 215-230`. The mock takes one error, uses it on the next call, then re-captures normally. Test `mock_writer_returns_one_shot_error_then_resumes` documents this. Defensible (`SignalWriteError::Backend` wraps `Box<dyn Error>` which generally isn't Clone), but the name `with_record_error` implies repeated-error injection, not one-shot. Tests would surprise a reader who expected the error to persist.

Recommended fix: rename to `with_one_shot_record_error` (verbose but accurate). Or add `with_persistent_record_error(...)` that uses a `dyn Fn() -> SignalWriteError` factory.

### m13. `is_empty()` and `empty()` on `OrchestratorOutput` — minor surface bloat

`src/engine/sentiment/signals.rs:89-97` adds `OrchestratorOutput::empty()` (`Self::default()`) and `is_empty()` (`signals.is_empty() && abstentions.is_empty()`). Both convenience-only. `OrchestratorOutput::default()` is already callable via the derive. No internal caller uses `is_empty()`.

Recommended fix: remove both; keep the derive. If a caller needs `empty`, `Default::default()` is two more characters. Drop the bloat.

### m14. `tests/orchestrator_smoke.rs` mentioned in learn-notes audit checklist but the file is not present

(Already covered as M2 — restating in MINOR for the checklist-coverage view: 4 of the 12 learn-notes audit-checklist items are not met. M2/M5/M6 cover three of those gaps.)

### m15. `loop-daemon` self-reference dev-dep is correctly present (closes Day 15 M3)

`Cargo.toml:106-109` adds the self-reference that Day 15 M3 flagged. Verified compiles, integration tests can see `test-fixtures`-gated symbols. **Day 15 M3 closed.** (Positive-finding — listed for completeness.)

---

## Verified clean

### Locked-decision compliance

- **D2** — `Arc<OrchestratorInner { ... sessions: DashMap<SessionId, Mutex<SessionState>> ... }>` shape correct. Internal Arc lets `Orchestrator: Clone` be cheap (one Arc clone, not a dashmap clone). ✓
- **D3** — Key is `SessionId` only; per-lesson rate limit lives inside `SessionState` as `HashMap<LoadedItemId, Instant>`. No `(TenantId, SessionId)` keying. ✓
- **D4** — Hand-rolled `HashMap<LoadedItemId, Instant>` + `now.duration_since(last) < cooldown` check. No `governor` dep introduced. Default 60s cooldown via `OrchestratorConfig::default()`. ✓
- **D5** — `std::sync::Mutex<SessionState>` confirmed via `grep -rn "tokio::sync::Mutex" src/engine/sentiment/`: zero hits. Critical sections (lines 207-231, 252-268, 296-356, 370-375) contain no `.await`. ✓
- **D6** — `dashmap = "6"` direct dep with MIT attestation (`Cargo.toml:86-89`). MIT umbrella in `THIRD_PARTY_LICENSES.md`. ✓
- **D7** — `JsonlWatcherSource` impl of `EventSource`, wraps existing `spawn_watcher`, bridges via `UnboundedReceiverStream`. Old `spawn_watcher` is still pub-re-exported in `jsonl_watcher/mod.rs:20`. ✓
- **D8** — Translation: `parent_uuid → parent_event_uuid` ✓, `cc_version → HostVersion` via `HostVersion::new` ✓, `git_branch.or_else(cwd.file_name())` via `derive_project_tag` ✓ (4 tests cover the precedence), `ParseError → Transient` ✓.
- **D9** — `is_auto_abstain_hazard(Sarcasm | AmbiguousReferent | OutOfDistribution | SelfDirected)`. Set exactly matches D9. ✓
- **D10** — `POSITIVE_MIN: f32 = 0.75` and `NEGATIVE_MIN: f32 = 0.85` as named consts (orchestrator.rs:48, 51). Cited to sentiment-design-rules rule 5. ✓
- **D11** — `attribute_signal` (no `_with_fallback` in 16a) called at line 457. Item-id mismatch handled (line 462-465). ✓
- **D13** — `SignalWriter` is an async trait; `LoggingSignalWriter` writes via `tracing::info` (signals.rs:141-161); `MockSignalWriter` test-fixture-gated (signals.rs:175-237). ✓
- **OQ-D16a-1** — `Hazard::SelfDirected` variant added (`types.rs:51`). Non-breaking (Hazard already `#[non_exhaustive]`). Auto-abstain set includes it. ✓
- **OQ-D16a-2** — `SignalWriter` is a trait with `Logging + Mock` impls. ✓
- **OQ-D16a-4** — `OrchestratorConfig` is module-local. No global `EngineConfig` introduced. ✓
- **OQ-D16a-5** — `recent_turn_capacity: 6` default (line 76). ✓
- **OQ-D16a-6** — Orchestrator stores full `text` in `RecentTurn.text`; no truncation. ✓
- **OQ-D16a-7** — `#![deny(clippy::await_holding_lock)]` is module-scoped (orchestrator.rs:19). Crate-wide stays default. ✓
- **OQ-D16a-8** — `tokio::spawn(async move { shutdown.cancelled().await; drop(handle); })` shutdown bridge (source.rs:66-69). Simple, no `select!`. ✓

### TS-with-Rust-syntax smell sweep (Day 16a-specific S18-S30)

| Smell | Status | Notes |
|---|---|---|
| S18. Inline literal thresholds | ✓ clean | `POSITIVE_MIN`, `NEGATIVE_MIN` named consts; cooldown defaults in `OrchestratorConfig::default()` as named `Duration::from_secs(60)`. |
| S19. `Box<dyn AttributionFallback>` | ✓ N/A | No fallback closure in 16a; deferred to 16b+ per D11. |
| S20. `Arc<Mutex<SessionState>>` clone-before-mutate | ✓ clean | Uses `DashMap::entry(...).or_default()` shard-locking; no `Arc<Mutex<_>>` per-entry. |
| S21. Allocating-per-event in stream pipeline | partial | `filter_map(|w| async move { Some(translate(w)) })` allocates per event — see m8. Simple `.map(translate)` is the idiomatic fix. |
| S22. `tokio::sync::Mutex` when no await | ✓ clean | `std::sync::Mutex` only. ✓ |
| S23. MutexGuard across `.await` | ✓ clean | Two `{ ... }`-scoped critical sections (orchestrator.rs:207-231, 252-268). Classifier `.await` on line 234 happens between blocks. `clippy::await_holding_lock = deny` is the live enforcement. |
| S24. `governor::RateLimiter` | ✓ clean | Hand-rolled hashmap cooldown check. |
| S25. `Arc<RwLock<HashMap<SessionId, SessionState>>>` | ✓ clean | DashMap throughout. ✓ |
| S26. Orphan rate-limit entries after session end | ✓ clean | `SessionEnded` calls `self.inner.sessions.remove(...)` (line 187), dropping the entire `SessionState` and with it the rate_limit HashMap. ✓ |
| S27. Mixing migration + feature in one commit | ✓ clean | `6cde320` is pre-research-only; `8fcb029` is the build commit. No migration mixed in. ✓ |
| S28. Split get_with_version read | ✓ N/A | 16b concern. |
| S29. fd_lock::RwLock across .await | ✓ N/A | 16b concern. |
| S30. Version as String | ✓ N/A | 16b concern. |

### Day 14/15 universal smell carryover

- `anyhow::Error` in engine: `grep -rn "anyhow" src/engine/sentiment/` returns 0 hits. ✓
- `crate::host::*` from engine: only doc comment at `engine/mod.rs:9`. ✓
- Trivial `new()` constructors: `JsonlWatcherSource::new` (non-trivial: `Into<PathBuf>`), `Orchestrator::new` (non-trivial: builds `Arc`), `LoadedItemId`/`HostVersion`/`ProjectTag::new` (non-trivial: `Into<Arc<str>>`). All non-trivial. ✓

### Translation correctness (JsonlWatcherSource)

- `WatcherEvent::UserTurn.parent_uuid` → `EngineEvent::UserTurn.parent_event_uuid`: line 100. ✓
- `cc_version` → `Some(HostVersion::new(cc_version))`: line 104. ✓
- `derive_project_tag`: prefers `git_branch` (non-empty), falls back to `cwd.file_name()`, returns None for empty (4 tests cover, lines 242-268). ✓
- `WatcherEvent::ParseError` → `EventSourceError::transient`: line 137. ✓
- Fatal vs Transient: init failure = `EventSourceError::fatal` (line 55); parse error = transient. ✓

### Code quality

- Module boundary: `grep -rn "crate::host" src/engine/sentiment/` → 0 hits. ✓
- License: no new AGPL/GPL/SSPL. `dashmap = 6` MIT; `tokio-stream = 0.1` MIT. ✓
- Test counts: 221 unit + 3 integration tests pass (was 197 + 3 at Day 15 audit — net +24 unit). Audit prompt expected 221+3; verified. ✓
- `cargo clippy --all-targets --features test-fixtures -- -D warnings` clean. ✓

### Concurrency-safety spot-checks

- The DashMap `remove(session_id)` (line 187) is atomic per-shard. Concurrent `entry(...)` for the same session_id while remove is in-flight serializes on the shard lock — no torn-state risk. ✓
- The `SessionEnded` race that DOES produce a panic via `.expect` is captured as **C1** above, not here.

---

## Summary

| Severity | Count | Examples |
|---|---|---|
| CRITICAL | 2 | C1 `.expect` panic on SessionEnded-vs-classifier-await race; C2 `handle_user_interrupt` signal-emit branch is structurally unreachable AND untested |
| MAJOR | 7 | M1 orchestrator.rs 522 prod LOC (over 500 hard limit); M2 no `tests/orchestrator_smoke.rs`; M3 zero signal-emit integration coverage; M4 `loaded_items` hard-coded empty → always abstains; M5 missing `clippy::mut_mutex_lock` lint; M6 `SessionState`/`SessionPhase` over-public; M7 `OrchestratorOutput` shape diverges from OQ-D16a-3 |
| MINOR | 15 | m1 `text.clone()` triple; m2 tuple-positionality on derive_signals return; m3 fragile `proximal` borrow scope; m4 `PretriggerNotFired` variant unreachable; m5 Debug-format polarity in tracing; m6 HashSet for dedup of ≤20 items; m7 `is_auto_abstain_hazard` policy not on enum; m8 stream `filter_map` allocation (S21 partial); m9 `session_count` should be `pub(crate)`; m10/m12 mock pattern repeats; m11 `Box<dyn Error>` source; m13 surface bloat on OrchestratorOutput; m14 checklist coverage gap; m15 Day 15 M3 self-reference closed (positive) |
| Verified clean | many | D2/D5/D7/D8/D9/D10/D11/D13 + OQ-D16a-1/2/4/5/6/7/8 met; all relevant TS-syntax smells S18-S27 either clean or addressed; module boundary intact; 221+3 tests pass; clippy clean both feature states; license discipline holds |

---

## TL;DR

Day 16a wires the orchestrator, the EventSource, and the SignalWriter trait cleanly at the *interface* level — D2/D5/D7-D11/D13 + OQ-D16a-1-8 are all met or improved on, the TS-syntax smells S18-S27 are clean or only minorly degraded (S21 is one-line away), the engine→host boundary holds, and 221 unit + 3 integration tests stay green under both feature states.

**The biggest concern is the compound of C2 + M3 + M4: the orchestrator ships with its signal-emit path structurally unreachable in production AND fully untested at the `process_event` integration level.** `loaded_items` is hard-coded empty so attribution always abstains; `last_assistant_turn_at` is never set so the correction-window emit branch is dead; and all three `#[tokio::test]` `process_event` tests use a default mock that always abstains. A maintainer running `cargo test --all-targets` will see 224 passing tests and conclude the orchestrator works — but no test verifies a single `SentimentSignal` was ever derived, attributed, threshold-gated, rate-limited, OR written via `SignalWriter`. The pure-function `derive_signals` tests cover the rules in isolation but nothing checks they're wired correctly into `process_event` or that the writer is actually called. Adding even one positive-path integration test (M3's recommended fix) would close most of that exposure — and would have caught M4 immediately. Pair with the C1 panic-on-race and you have an orchestrator that's well-designed at the seams but neither correct under concurrency nor verified end-to-end.

The secondary biggest concern is **M1: `orchestrator.rs` is 522 prod LOC, past the 500 hard limit** that D15 set. The audit prompt explicitly asked whether to trigger the planned split into `orchestrator/{mod,state,signals,correction_window}.rs`. The answer is yes — the build is already past the threshold, and the file's size is what made it possible for C2's dead branch and M4's empty `loaded_items` to slip in unflagged. A split now (before 16b's `StorageBackedSignalWriter` adds another 100+ LOC) keeps each module reviewable.
