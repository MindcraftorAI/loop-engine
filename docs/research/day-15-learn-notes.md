# Day 15 Learn Notes — Locked Decisions for Build Phase

**Date:** 2026-05-13
**Cycle phase:** Learn (workflow cycle phase 2)
**Cycle:** Day 15 (sentiment pretrigger + SentimentClassifier trait + attribution)
**Source pre-research:** `docs/research/day-15-pre-research.md` (744 lines)

These decisions are LOCKED. Build phase consumes them as input; they do not get revisited mid-build absent a fundamental discovery. Per [[feedback-rust-idiomatic-refactor]] the design target is idiomatic Rust (regex/serde/sqlx/tower patterns), NOT TS transliteration.

---

## Locked decisions (verbatim from pre-research D1-D15, condensed)

### D1. `EngineEvent::UserTurn` shape (closes Day 14 L1)
- Add 3 flat fields: `parent_event_uuid: Option<String>`, `host_version: Option<HostVersion>`, `project_tag: Option<ProjectTag>`
- `HostVersion(Arc<str>)` and `ProjectTag(Arc<str>)` newtypes in `engine::events`
- NO `HostExtras` sub-struct, NO host-specific variants
- Host adapter populates these fields (Claude Code maps `cc_version` → `HostVersion`, derives `ProjectTag` from `cwd`+`git_branch`)

### D2. Pretrigger
- `regex = "1"` promoted to direct dep
- One `LazyLock<Regex>` for the pre-compiled pattern
- Wrap in `Pretrigger` struct with `Default::default()` constructor + `Pretrigger::with_pattern(...)` for test injection
- Pretrigger is sync, pure function

### D3. `SentimentClassifier` trait
- Sealed (engine-internal impls only) via `sealed::Sealed`
- `async_trait` macro for async fn signatures
- Object-safe: held as `Arc<dyn SentimentClassifier>`
- Methods:
  - `async fn classify(&self, &Context, &ClassificationRequest) -> Result<RawClassification, ClassifierError>`
  - `fn name(&self) -> &'static str`
- Mirrors Day 14's Storage + EventSource trait shape

### D4. Attribution algorithm
- PURE FUNCTION: `attribute_signal(utterance, &[LoadedItem], &[RecentTurn]) -> Option<Attribution>`
- NO state machine, NO struct, NO typestate
- Variant: `attribute_signal_with_fallback(...)` accepts a `FnOnce` for Pass 4 judge invocation
- Stateless — orchestrator (Day 16) holds any session-scoped context separately

### D5. Module layout
```
src/engine/sentiment/
├── mod.rs          (re-exports + prelude)
├── types.rs        (Polarity, Hazard, AttributionMethod, LoadedItemKind, confidence newtypes, etc.)
├── pretrigger.rs
├── classifier.rs   (trait + MockSentimentClassifier behind `test-fixtures` feature)
└── attribution.rs
```
Orchestrator (Day 16) and solicitor (Day 17) land as sibling files (`orchestrator.rs`, `solicitor.rs`).

### D6. Test strategy
- `MockSentimentClassifier` ships in `engine/sentiment/classifier.rs` behind `test-fixtures` Cargo feature
- Adversarial fixtures under `tests/fixtures/sentiment/` loaded via `include_str!`
- Pure-function inline `#[cfg(test)] mod tests` for pretrigger + attribution

### D7. Lessons migration → **DEFERRED to Day 16**
Day 15 ships PURE-LOGIC sentiment code with ZERO changes to `engine::lessons::*`. Cross-process flock-vs-CAS semantic audit moves to Day 16 pre-research.

### D8. Naming (drop "subagent")
- `SentimentSubagentInput` → `ClassificationRequest`
- `SentimentSubagentOutput` → not introduced today (Day 16 orchestrator builds its own output shape)
- `SentimentClassifierClient` → `SentimentClassifier`

### D9. Confidence newtypes (three distinct types)
- `AttributionConfidence` — attribution algorithm output
- `ClassifierConfidence` — raw classifier output
- `CalibratedConfidence` — orchestrator-calibrated value used for promotion thresholds
- All wrap `f32`, all clamp/validate at construction, all `Copy + Clone + Debug + PartialEq + PartialOrd`
- Polarity-threshold logic in Day 16 orchestrator uses `CalibratedConfidence` exclusively

### D10. Enum shapes
- `Polarity` — closed (3 variants: `Positive`, `Negative`, `Neutral`). NO `#[non_exhaustive]`.
- `Hazard`, `AttributionMethod`, `LoadedItemKind` — `#[non_exhaustive]`
- All derive `Copy + Clone + Debug + PartialEq + Eq + Hash`

### D11. `LoadedItemId`
Newtype `Arc<str>` matching `SessionId` pattern. Replaces TS `string` id everywhere in the sentiment module.

### D12. Dependencies added
- `regex = "1"` — promoted to direct dep (was transitive via `tracing-subscriber`). MIT/Apache.
- NO other new deps.

### D13. Feature flags
- Add `[features] test-fixtures = []` to `Cargo.toml`
- Self-reference via `loop-daemon = { path = ".", features = ["test-fixtures"] }` in `[dev-dependencies]` so integration tests can see the mock
- Production builds don't compile the mock

### D14. File-size budget
- All Day 15 files target <300 LOC each (under the 500 LOC hard limit by safety margin)
- If `types.rs` approaches 250 LOC: split first by concern (`confidence.rs`, etc.)

### D15. License audit
- `regex` 1.x is MIT/Apache-2.0 — already transitively present
- Promote to direct-dep listing in `Cargo.toml`
- `THIRD_PARTY_LICENSES.md`: no changes needed (already covered by the MIT/Apache umbrella)

---

## Open-question decisions (accepting all pre-research recommendations)

### OQ1. Ship `attribute_signal_with_fallback` Day 15 or Day 16? → SHIP Day 15
Signature only; no caller. Day 16 wires it into the orchestrator. Locks the closure-generic shape early.

### OQ2. Mock API shape → BUILDER CHAIN
`MockSentimentClassifier::new().with_response(r1).with_response(r2)`. Revisit if Day 16/17 needs response-by-call-shape sequencing.

### OQ3. Pretrigger per-locale → `Pretrigger::default()` (no locale in name)
KISS — no multilingual in Day 15-17 plan. Add `default_en` / `default_es` etc. when needed.

### OQ4. `HostVersion::is_in_tested_range` → BARE TYPE today, tripwire impl Day 17
Day 17 (solicitor work) adds the tripwire as the natural moment.

### OQ5. `ProjectTag` derivation policy → HOST adapter, NOT engine
Write the convention into the rustdoc on `ProjectTag` so the rule is on the type, not in floating prose:
> "`ProjectTag` is derived by the host adapter from host-specific signals (e.g. Claude Code: `git_branch`-or-`cwd_basename` fallback). The engine treats it as opaque."

### OQ6. `ClassificationRequest` → OWNED
Bounded size (4-6 turns + ≤20 items × small structs). Ships across `.await` trivially. Aligns with the `Arc<str>`-cheap-clone philosophy from Day 14.

### OQ7. `RawClassification::abstain()` → NAMED CONSTRUCTOR
Explicit-abstain is more readable than `RawClassification::default()`. Empty `per_item` + empty `global_hazards`.

### OQ8. Adversarial fixtures count → ~30 for Day 15 (10 positive / 10 negative / 10 edge)
Edge cases: smart quotes, mixed case, surrounding punctuation. Verifies the TS audit-A1 fixes carry forward. Full 50-case set lands by Day 17 audit (when sentiment work fully stabilizes).

### OQ9. Cargo.lock policy → **ALREADY DONE** in `5e55f93` (Day 14 L6 forward-feed)
Committed before Day 15 pre-research returned. ✓

---

## Build phase scope (sub-phases)

### Phase 3a — `EngineEvent::UserTurn` field additions + adapter update (~50 LOC)
1. Add `HostVersion` + `ProjectTag` newtypes to `src/engine/events.rs`
2. Add 3 fields to `EngineEvent::UserTurn`
3. Update `host::claude_code::jsonl_watcher::parser` to populate them (parent_event_uuid from existing parent_uuid, host_version from cc_version, project_tag from cwd+git_branch derivation)
4. Update `WatcherEvent::UserTurn` to match (it's the source the parser populates)

### Phase 3b — `engine::sentiment` skeleton + types (~200 LOC)
1. `src/engine/sentiment/mod.rs` — module declarations + re-exports
2. `src/engine/sentiment/types.rs` — Polarity, Hazard, AttributionMethod, LoadedItemKind, three confidence newtypes, LoadedItemId, LoadedItem, RecentTurn, ClassificationRequest, RawClassification + `abstain()`, Attribution, ClassifierError
3. Update `src/engine/mod.rs` to declare `sentiment`

### Phase 3c — pretrigger (~230 LOC)
1. `src/engine/sentiment/pretrigger.rs` — `LazyLock<Regex>` + `Pretrigger` struct + `Default` impl + `with_pattern` injection + `fires(&self, &str) -> bool` method
2. Inline tests for ~30 adversarial fixtures (positive/negative/edge)

### Phase 3d — classifier trait + mock (~250 LOC)
1. `src/engine/sentiment/classifier.rs` — sealed `SentimentClassifier` async trait + `MockSentimentClassifier` behind `#[cfg(feature = "test-fixtures")]`
2. Feature flag wiring in `Cargo.toml`

### Phase 3e — attribution (~350 LOC)
1. `src/engine/sentiment/attribution.rs` — `attribute_signal` pure function + `attribute_signal_with_fallback` variant (closure-generic Pass 4 hook)
2. Inline tests for attribution priors / abstain skip

### Phase 3f — wiring + license + verification
1. `Cargo.toml`: promote `regex = "1"` to direct dep; add `[features] test-fixtures = []`
2. `THIRD_PARTY_LICENSES.md`: verify regex MIT/Apache attestation
3. `cargo check` + `cargo test --all` + `cargo test --all --features test-fixtures` + `cargo clippy --all-targets`

Commit cadence: one logical commit per sub-phase OR batch related sub-phases. Suggested: 3a alone (touches host code), 3b+3c together (types + pretrigger are coherent), 3d alone (classifier trait), 3e alone (attribution), 3f alone (wiring close).

---

## Audit checklist for Day 15 audit phase

The audit agent will receive this + the 17 sentiment-specific smells (S1-S17 in pre-research) + the Day 14 smell list + the locked-decisions table above.

**Must verify:**
- [ ] No `crate::host` references inside `src/engine/sentiment/`
- [ ] All Day 15 files <300 LOC each, none >500 LOC
- [ ] `SentimentClassifier` is sealed (no external impl possible)
- [ ] All new public enums marked `#[non_exhaustive]` per D10 (except `Polarity` which is intentionally closed)
- [ ] Three confidence newtypes are distinct types, NOT type aliases
- [ ] `attribute_signal` is a pure function (no `&self`, no state)
- [ ] All 146 prior tests still pass; new tests added bring count to ≥176 (≥30 adversarial fixtures + ≥1 per pretrigger structural case + classifier-trait roundtrip + attribution structural cases)
- [ ] `cargo test --no-default-features` still works (test-fixtures feature truly opt-in)
- [ ] `cargo test --features test-fixtures` exercises the MockSentimentClassifier
- [ ] License check: `regex` MIT/Apache-2.0, no AGPL/GPL/SSPL introduced

**Must check for sentiment-specific TS-with-Rust-syntax smells** (full list in pre-research S1-S17):
- S1. Stringly-typed Polarity / methods / kinds → all enums per D10
- S2. Regex match positions as raw `usize` pairs → typed Span if needed
- S3. `f32` confidences without bounds → use the three newtypes from D9
- S4. `Option<Option<T>>`
- S5. `Box<dyn SentimentClassifier>` taken by ephemeral helpers → take `&dyn`
- S6. Hazards as `Vec<String>` → `Vec<Hazard>` (enum)
- S7. `Vec<Box<dyn AttributionPass>>` extensible registry — not warranted today
- S8. Walls of `if let Some(x) = ... { return ... } else { return None }` → use `?`
- S9. `HashMap<String, T>` for known-finite sets → enum-keyed map or `[T; N]`
- S10. `async fn` on pure-CPU methods → pretrigger + attribution stay sync
- S11. `Arc<Mutex<SessionState>>` premature
- S12. Manually-typed `Pin<Box<dyn Future + Send + 'static>>` → use BoxFuture
- S13. `tokio::spawn` inside attribution / pretrigger — they're pure CPU; never spawn from inside
- S14. `String` for `LoadedItem::id` → `LoadedItemId` newtype (D11)
- S15. `Polarity::from_str("positive")` inside the engine — engine never parses polarity strings
- S16. `Vec<RecentTurn>` taken by value where borrow would suffice → `&[RecentTurn]`
- S17. Premature genericism `<R: Read>` for regex pattern source — concrete `&str` is fine

**Must verify Day 14 carryover compliance:**
- [ ] `EngineEvent::UserTurn` field additions are non-breaking (the type is `#[non_exhaustive]`)
- [ ] Host adapter populates new fields without breaking existing integration tests
- [ ] No accidental code paths take `EngineEvent` by value where `&EngineEvent` would do

---

## What this learn-notes does NOT decide

- Day 16 orchestrator structure (separate Day 16 pre-research)
- Day 17 solicitor structure (separate Day 17 pre-research)
- Anthropic Haiku adapter shape (post-17, deferred for user discussion per [[feedback-execute-the-plan]])
- Lessons migration to `Storage::put_if_version` (Day 16; D7 deferral)
- `LocalFsStorage::put_if_version` implementation (Day 16; same trigger)

---

Related: [[feedback-workflow-cycle]], [[feedback-rust-idiomatic-refactor]], [[feedback-execute-the-plan]], `docs/research/day-15-pre-research.md`, `docs/research/sentiment-design-rules.md`
