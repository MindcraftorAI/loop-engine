# Day 15 Audit Report

**Cycle:** Day 15 (sentiment pretrigger + SentimentClassifier trait + attribution)
**Audit window:** commits `704277a..0f135af` (3 commits: pre-research + learn-notes, sentiment build, Cargo.lock update)
**Phase:** 5 (audit — backward-looking)
**Date:** 2026-05-13

**Build status at audit time:** `cargo build --all-targets` clean (no warnings); `cargo test` 197 unit + 3 integration tests pass; `cargo test --no-default-features` passes; `cargo test --features test-fixtures` passes; `cargo clippy --all-targets` clean; `cargo clippy --all-targets --features test-fixtures` clean.

---

## CRITICAL findings

None. The build is structurally sound and matches the locked D1–D15 + OQ1–OQ9 decisions in every load-bearing respect. The remaining issues are MAJOR/MINOR.

---

## MAJOR findings

### M1. New `Arc<str>` newtypes don't impl `AsRef<str>` — divergence from Day 14 `impl_id_newtype!` pattern

`src/engine/events.rs:103-110` (`HostVersion`), `src/engine/events.rs:129-136` (`ProjectTag`), `src/engine/sentiment/types.rs:137-150` (`LoadedItemId`) each ship `new()`, `as_str()`, and `Display` — but no `AsRef<str>` impl.

Day 14 locked the pattern in `src/engine/context.rs:48-69` via `impl_id_newtype!`, which ships **`new` + `as_str` + `AsRef<str>` + `Display`**. `TenantId / UserId / SessionId / TeamId` all use it. The audit prompt explicitly called out "Display + AsRef-style ergonomics present" as a Phase 3a verification item.

Why it matters: every site that today calls `id.as_str()` is forced to remember which getter to use; `AsRef<str>` enables generic `fn foo<S: AsRef<str>>(s: S)` callers to accept any of these newtypes interchangeably. This will surface the moment Day 16 starts threading these IDs through generic helpers (logging, key construction, etc.).

Recommended fix: route all three through `impl_id_newtype!` (move the macro to a shared location — e.g. a `engine::newtype` private module — or keep it `pub(crate)` in `context.rs` and `use crate::engine::context::impl_id_newtype;`). Alternatively, hand-add `impl AsRef<str>` to each of the three types. The macro route is preferred — guarantees future newtypes don't drift further.

### M2. `events.rs` module doc-comment is stale post-Day-15

`src/engine/events.rs:1-28`: the doc says `**Day 14 status (audit-closed):** trait + types defined. No impl ships yet` and `Why deferred: EngineEvent::UserTurn currently carries the host-agnostic minimum (session_id, event_uuid, text, timestamp, cwd)` and `Day 15 plan: pre-research nails down EngineEvent::UserTurn's final shape`. None of those statements are true any more — D1 was locked and the three fields landed in this audit window.

Why it matters: future readers (including audit agents) will be misled into thinking the field decision is still open. This is exactly the kind of stale doc Day 14 audit m9 flagged on the same file.

Recommended fix: rewrite the module doc to say "Day 15 D1 closed: `EngineEvent::UserTurn` carries `parent_event_uuid`, `host_version`, `project_tag` alongside the original five fields. First `EventSource` impl lands in Day 16 alongside the orchestrator." Drop the stale "Why deferred" paragraph entirely.

### M3. D13 self-reference dev-dep not added

Learn-notes D13: "Self-reference via `loop-daemon = { path = ".", features = ["test-fixtures"] }` in `[dev-dependencies]` so integration tests can see the mock."

`Cargo.toml:92-95` ships only:

```
[dev-dependencies]
tokio = { version = "1", features = ["test-util"] }
tempfile = "3"
dirs = "6"
```

No `loop-daemon` self-reference. Confirmed via `grep -n "loop-daemon\|test-fixtures" Cargo.toml`: only the package-name and bin-name hits + the `[features]` entry; no self-reference under `[dev-dependencies]`.

Why it matters: today's tests for the mock live inside `src/engine/sentiment/classifier.rs` under `#[cfg(test)] mod tests`, which sees the mock via the in-crate `#[cfg(test)]` arm of the cfg gate. Day 16 will want to test orchestrator behavior using `MockSentimentClassifier` from integration tests under `tests/*.rs`, and those CANNOT see the `test-fixtures`-feature-gated symbol without the self-reference (the test crate is a separate compilation unit). The feature flag declaration is half-complete without it.

Recommended fix: add to `Cargo.toml`:

```toml
[dev-dependencies]
loop-daemon = { path = ".", features = ["test-fixtures"] }
```

Optionally add a smoke integration test (e.g. `tests/mock_classifier_visible.rs`) that imports `MockSentimentClassifier` to pin the contract — flips to a compile error when the self-reference is dropped.

### M4. `TurnRole` is `pub` but not re-exported from `sentiment::mod.rs`

`src/engine/sentiment/types.rs:189-194` defines `pub enum TurnRole { User, Assistant }`. It is a required field on the public `RecentTurn::role`. The mod.rs re-export list (`src/engine/sentiment/mod.rs:23-27`) covers every other public type from `types.rs` — `AttributionConfidence, AttributionMethod, CalibratedConfidence, ClassificationRequest, ClassifierConfidence, Hazard, ItemClassification, LoadedItem, LoadedItemId, LoadedItemKind, Polarity, RawClassification, RecentTurn` — but NOT `TurnRole`.

Why it matters: any external caller that needs to construct a `RecentTurn` (today: the engine's own tests; tomorrow: the Day 16 orchestrator, Day 17 solicitor, integration tests) must `use crate::engine::sentiment::types::TurnRole` while every other type comes through the prelude `crate::engine::sentiment::*`. This is exactly the "single source of truth" / "module IS the surface" rule from pre-research Q5 audit smells.

Recommended fix: add `TurnRole` to the `pub use types::{...}` block in `src/engine/sentiment/mod.rs:23-27`.

### M5. attribution.rs Pass 4 confidence threshold `0.8` is an unnamed magic number

`src/engine/sentiment/attribution.rs:79`: `if conf.value() >= 0.8 { ... }` with the inline comment "Threshold: the judge must be ≥0.8 confident or we abstain."

Why it matters: this is the Pass 4 abstain threshold, a design-rules-locked value (per sentiment-design-rules.md). Inlined as a literal it can drift between attribution.rs and orchestrator/calibration code that may want to read or change it. The audit prompt flagged "the value documented? Should it be a named const?" specifically.

Recommended fix:

```rust
/// Pass 4 (Salience) abstain threshold: the classifier-judge fallback
/// must report ≥ this confidence or attribution abstains. Locked by
/// design rules (see docs/research/sentiment-design-rules.md).
const PASS4_MIN_CONFIDENCE: f32 = 0.8;
```

Then `if conf.value() >= PASS4_MIN_CONFIDENCE`. Optionally promote the type to `AttributionConfidence` for consistency, though the comparison is currently bare `f32`.

---

## MINOR findings

### m1. `first_word_lowercase` allocates a `String` per Pass 2 call

`src/engine/sentiment/attribution.rs:190-195`: `first_word_lowercase` returns `Option<String>`. Called from `pass2_pronoun_anaphor` for every utterance, allocates a fresh `String` for what's almost always a short ASCII pronoun ("that", "it", "this").

Why it matters: minor perf hit on a hot path. Pre-research smell list Q4 explicitly called out `.collect::<Vec<_>>()`-style materialization. The pronoun set is ASCII-only — `eq_ignore_ascii_case` would avoid the allocation entirely.

Recommended fix: use a `&str` slice + `eq_ignore_ascii_case` against the small pronoun set, or accept the allocation and document it. Not load-bearing for Day 15.

### m2. `recently_referenced_items` clones full `LoadedItem`s including keyword `Vec<String>`s

`src/engine/sentiment/attribution.rs:175-188`: returns `Vec<LoadedItem>` via `.cloned().collect()`. Each clone walks `label: String` and `keywords: Vec<String>`.

Why it matters: only called from the `_with_fallback` path which is gated on the candidate-count test, so impact is small. The closure boundary requires owned `LoadedItem` to be a true `FnOnce(&[LoadedItem])` — but `&[&LoadedItem]` (a slice of borrows) would work too without changing the closure shape much.

Recommended fix: return `Vec<&'a LoadedItem>` and adjust the closure signature to `FnOnce(&[&LoadedItem]) -> ...`. Or accept the clones — bounded ≤20 items. Defer to Day 16 when call-site is concrete.

### m3. `MockSentimentClassifier::default()` returns empty queue → silent abstain

`src/engine/sentiment/classifier.rs:90-95` derives `Default`. `classify()` with an empty queue returns `Ok(RawClassification::abstain())` (line 142).

Pre-research Q6 audit smells: "`MockSentimentClassifier::default()` returning empty queue — should require explicit setup so silent-abstain doesn't mask orchestrator bugs."

The build chose to keep `Default` and document the abstain-on-empty behavior in tests (`mock_returns_abstain_when_queue_empty` at line 172). Defensible — explicit-abstain is the well-tested behavior. But the pre-research smell warning still applies: when Day 16 orchestrator tests use `MockSentimentClassifier::default()` without configuring responses, the test passes even if the orchestrator silently abstains.

Recommended fix (optional, low priority): rename `Default` derive away → require `with_response(...)` or `with_error(...)` for construction; add `expect_no_calls()` method for the test-intent "this mock should never be called." Day 16 may revisit if real orchestrator tests get flaky.

### m4. `MockSentimentClassifier` builder methods take `self` by value, then lock the Mutex

`src/engine/sentiment/classifier.rs:100-115`: `with_response(self, ...) -> Self` and `with_error(self, ...) -> Self` consume `self`, lock the wrapped Mutex, push, return `self`. Logically correct but slightly silly — during the builder chain `self` is the only owner, so the Mutex is uncontended; the Mutex exists only because the runtime impl in `classify` needs `&self` interior mutability.

Why it matters: a tiny ergonomic quirk; not a real smell. Documented because the alternative (`&mut self` builder methods returning `&mut Self`) is equally idiomatic and would skip the futile mutex locks.

Recommended fix: accept current shape (chained-builder pattern is locked by OQ2 and works fine). Or switch to `&mut self` if Day 16 finds the chained-by-move pattern awkward at call sites.

### m5. types.rs at 299 LOC (256 prod, 43 test) — at the D14 soft-split threshold

`wc -l src/engine/sentiment/types.rs` → 299. Production code (excluding `#[cfg(test)] mod tests`) is 256 LOC.

D14 said: "If `types.rs` approaches 250 LOC, split first by concern (e.g. `confidence.rs`)." 256 prod-LOC is past that threshold. Pre-research Q5 also noted "Cap ~250 LOC; revisit if it grows."

Why it matters: future additions (more Hazard variants, calibration types, polarity-confidence combos) will push past 300 quickly. Splitting now is cheap; splitting later means cascading import changes.

Recommended fix: defer for now — the file is coherent and Day 16 / Day 17 will reveal which concept-cluster wants its own file (likely `confidence.rs` per the D14 hint). Track in Day 15 post-research as a forward-feed.

### m6. attribution.rs at 374 LOC total — over D14 target <300

Production code is 200 LOC (tests are 174 LOC). Total 374 exceeds the D14 soft target (<300) but is comfortably under the hard limit (<500).

Why it matters: D14 D14 target is a soft cap. Day 15 cycle ships with the inline tests intact, which is correct per D6 (inline tests for pure-function modules). The file would split poorly — five passes + helpers + tests are tightly coupled.

Recommended fix: none. Document as accepted; consider extracting test cases to a sibling `attribution_tests.rs` only if Day 17 audit pushes total back over 500.

### m7. pretrigger.rs at 313 LOC total — over D14 target <300 (130 prod, 183 test)

Same pattern as m6 — production is small (130 LOC); inline adversarial fixture tests are the bulk. Acceptable.

### m8. `Polarity` derives `PartialEq, Eq, Hash` but `ItemClassification` only derives `PartialEq` (no `Eq`/`Hash`)

`src/engine/sentiment/types.rs:218-227`: `#[derive(Debug, Clone, PartialEq)]`. Missing `Eq` is correct because `ClassifierConfidence` wraps `f32` (no `Eq`), but it does mean `ItemClassification` and `RawClassification` can't be put into a `HashSet` or used as a `HashMap` key.

Why it matters: not a smell — the float prevents it by language rules. Documented for completeness because the audit checklist asks for enum-derive review.

Recommended fix: none. Accepted.

### m9. `PRONOUNS` HashSet vs a `static [&str; 6]` array or `phf::Set`

`src/engine/sentiment/attribution.rs:119-127`: `LazyLock<HashSet<&'static str>>` with six entries. A 6-element `[&'static str; 6]` slice + `.contains(&first_word.as_str())` is O(n) but trivially faster on six entries than `HashSet` (which has per-lookup hash cost).

Pre-research Q4 mentioned "static `STOPWORDS: phf::Set<&'static str>` (compile-time) or `LazyLock<HashSet<&'static str>>` (lazy)". The build picked the lazy HashSet — fine for six entries; pre-research smelled HashSet over enum/array for known-finite sets (S9).

Why it matters: this is exactly S9 (`HashMap<String, ...>` for known-finite sets) at small scale. The set is known and finite (six pronouns); a const array would be marginally faster and idiomatic.

Recommended fix: `const PRONOUNS: &[&str] = &["that", "it", "this", "those", "these", "they"];` then `PRONOUNS.iter().any(|p| first_word.eq_ignore_ascii_case(p))`. Pairs with m1 to drop the allocation entirely. Low priority.

### m10. `TurnRole` carries `#[non_exhaustive]` without being in D10's list

`src/engine/sentiment/types.rs:189-194`: `#[non_exhaustive] pub enum TurnRole { User, Assistant }`.

D10 explicitly lists `Hazard, AttributionMethod, LoadedItemKind` as `#[non_exhaustive]`. `TurnRole` is not in that list; it's also not in the closed-by-design list (which is just `Polarity`).

Defensible: `TurnRole` could grow a `System` variant later (system prompts, tool messages, etc.) — `#[non_exhaustive]` is forward-compatible insurance.

Recommended fix: none. Document in Day 15 post-research as an intentional addition consistent with the D10 spirit.

### m11. `Pretrigger::with_pattern` returns `Result<Self, regex::Error>` — exposing `regex::Error` at engine boundary

`src/engine/sentiment/pretrigger.rs:110`: gated test method exposes the upstream `regex::Error` type. The audit prompt's S15 (parsing belongs at the boundary) doesn't quite apply because this is the test-fixtures injection point.

Why it matters: pre-research smell #2 (Day 14 carryover): engine public boundary uses named error enums. The current type is gated test-only, so the leak is bounded. Day 14 audit m7 accepted `Box<dyn Error>` similarly when scoped to a named wrapper variant.

Recommended fix: accept current shape. If Day 17 adds a non-test config-driven pattern loader, wrap in `PretriggerError::InvalidPattern(#[source] regex::Error)`.

---

## Verified clean

### Locked-decision compliance

- **D1** — `EngineEvent::UserTurn` adds three fields with correct types: `parent_event_uuid: Option<String>`, `host_version: Option<HostVersion>`, `project_tag: Option<ProjectTag>`. `events.rs:50-69`. ✓ Newtypes are `Arc<str>`-backed. No `HostExtras` sub-struct, no host-specific variants. ✓
- **D2** — `regex = "1"` direct dep (Cargo.toml:84 with MIT/Apache attestation). `LazyLock<Regex>` at `pretrigger.rs:75-77`. `Pretrigger::default()` is the public constructor (`pretrigger.rs:98-104`); the name does NOT include `_en` (OQ3 ✓). `with_pattern` is gated `#[cfg(any(test, feature = "test-fixtures"))]` ✓.
- **D3** — `SentimentClassifier` sealed via `sealed::Sealed` (`classifier.rs:58, 72-74`). `async_trait` macro applied (`classifier.rs:57`). Object-safe: test `classifier_trait_is_object_safe` at line 226-232 holds `Arc<dyn SentimentClassifier>`. Signature exactly matches D3: `async fn classify(&self, &Context, &ClassificationRequest) -> Result<RawClassification, ClassifierError>` + `fn name(&self) -> &'static str`. ✓
- **D4** — `attribute_signal` is a free function, not a method (`attribution.rs:45`). `attribute_signal_with_fallback` takes `F: FnOnce`, not `Box<dyn FnMut>` (line 70). No state machine, no struct. ✓
- **D5** — Module layout matches spec exactly: `sentiment/{mod, types, pretrigger, classifier, attribution}.rs`. ✓
- **D6** — `MockSentimentClassifier` gated behind `#[cfg(any(test, feature = "test-fixtures"))]` (`classifier.rs:90, 97, 123, 126`). Inline `#[cfg(test)] mod tests` for pretrigger + attribution pure-function tests. ✓
- **D7** — Day 15 ships zero changes to `engine::lessons::*`. Confirmed via `git diff 704277a..0f135af --name-only` — no `lessons/` paths. ✓
- **D8** — "subagent" naming dropped: `SentimentClassifier` not `SentimentClassifierClient`; `ClassificationRequest` not `SentimentSubagentInput`. ✓
- **D9** — Three confidence newtypes are DISTINCT types via the macro `impl_confidence_newtype!` (types.rs:91-126). Test `confidences_are_distinct_types` (line 270-279) documents the compile-time guarantee. NOT type aliases. ✓
- **D10** — `Polarity` has NO `#[non_exhaustive]` (closed at 3 variants ✓); `Hazard`, `AttributionMethod`, `LoadedItemKind` DO have it. ✓
- **D11** — `LoadedItemId(Arc<str>)` newtype matching `SessionId` pattern. ✓ (modulo M1 — `AsRef<str>` missing).
- **D12** — Only `regex = "1"` added. ✓
- **D13** — `[features] test-fixtures = []` declared in Cargo.toml:86-90. ✓ (modulo M3 — self-reference dev-dep missing).
- **D14** — file size budget: production code per file ≤256 LOC (types.rs). Inline-tests inflate totals but stay under hard 500 LOC cap. m5/m6/m7 track soft-cap drift.
- **D15** — `regex` MIT/Apache attestation present in Cargo.toml:82-83. `THIRD_PARTY_LICENSES.md` covers it under the "all other direct deps are MIT/Apache" umbrella. ✓
- **OQ1** — `attribute_signal_with_fallback` shipped Day 15. ✓
- **OQ2** — Builder chain (`MockSentimentClassifier::default().with_response(...).with_response(...)`). ✓
- **OQ3** — `Pretrigger::default()` not `default_en()`. ✓
- **OQ4** — `HostVersion` is a bare type without `is_in_tested_range()`. ✓ Day 17 work.
- **OQ5** — `ProjectTag` rustdoc (`events.rs:118-125`) documents: "derivation lives in the host adapter, not the engine." ✓
- **OQ6** — `ClassificationRequest` is owned (`String` + `Vec<...>`). ✓
- **OQ7** — `RawClassification::abstain()` named constructor present (`types.rs:243-245`). ✓
- **OQ8** — Pretrigger inline tests: 10 positive (lines 141-189), 10 negative (lines 193-241), 10 edge (lines 245-312). ✓ Count matches.
- **OQ9** — Cargo.lock already committed pre-Day-15. ✓

### Phase 3a verification

- `WatcherEvent::UserTurn` (host) still carries `parent_uuid: Option<String>`, `git_branch: Option<String>`, `cc_version: String`. Confirmed via `grep -n "parent_uuid\|cc_version\|git_branch" src/host/claude_code/jsonl_watcher/events.rs`. ✓ Not renamed; translation deferred to Day 16 per learn-notes.
- `EngineEvent::UserTurn` has new optional fields. ✓
- `EngineEvent::UserInterrupt` has new `parent_event_uuid: Option<String>` field. ✓
- `HostVersion` and `ProjectTag` are `Arc<str>`-backed newtypes with `new` + `as_str` + `Display`. (Missing `AsRef<str>` — see M1.)

### TS-with-Rust-syntax smell sweep (Day 15 sentiment-specific S1–S17)

| Smell | Status | Notes |
|---|---|---|
| S1. Stringly Polarity/methods/kinds | ✓ clean | All enums per D10 (types.rs:19, 34, 60, 79). |
| S2. Regex match positions as raw `usize` pairs | ✓ N/A | No span types introduced — attribution uses `String::contains` boolean output only. |
| S3. `f32` confidences without bounds | ✓ clean | Three newtypes, all clamp at construction. |
| S4. `Option<Option<T>>` | ✓ clean | `grep "Option<Option"` returns 0 hits in Day 15 files. |
| S5. `Box<dyn SentimentClassifier>` by ephemeral helpers | ✓ clean | No `Box<dyn SentimentClassifier>` anywhere. |
| S6. Hazards as `Vec<String>` | ✓ clean | `Vec<Hazard>` everywhere. |
| S7. `Vec<Box<dyn AttributionPass>>` | ✓ clean | Closed-set, fixed-function dispatch. |
| S8. Walls of `if let Some else { return None }` | ✓ clean | `.or_else` chain (attribution.rs:50-52). Inner `if let` blocks use `?` where applicable. |
| S9. `HashMap<String, ...>` for finite sets | partial | No `HashMap` hits, but PRONOUNS is `HashSet<&str>` over 6 items — see m9. |
| S10. `async fn` on pure-CPU methods | ✓ clean | Pretrigger + attribution are sync. Only `SentimentClassifier::classify` is async. |
| S11. `Arc<Mutex<SessionState>>` premature | ✓ clean | No session state in Day 15. |
| S12. Manually-typed `Pin<Box<dyn Future + Send + 'static>>` | ✓ clean | `async_trait` macro used. |
| S13. `tokio::spawn` in attribution/pretrigger | ✓ clean | `grep "tokio::spawn" src/engine/sentiment/*.rs` returns 0 hits. |
| S14. `String` for `LoadedItem::id` | ✓ clean | `LoadedItemId(Arc<str>)` everywhere. |
| S15. `Polarity::from_str` in engine | ✓ clean | No parsing in `engine::sentiment`. |
| S16. `Vec<RecentTurn>` by value where borrow would do | ✓ clean | Helpers take `&[RecentTurn]`, `&[LoadedItem]`. |
| S17. Premature genericism `<R: Read>` | ✓ clean | `with_pattern(pattern: &str)` — concrete. |

### Day 14 generic smell carryover

- `anyhow::Error` in new engine public function returns: **NOT FOUND** in any Day-15-touched file. ✓
- `crate::host::*` from `crate::engine::*`: only the boundary-doc comment at `engine/mod.rs:9`; no code reference. ✓
- `pub fn new()` trivial constructors (Day 14 M7): all three new `new()` methods on Arc-str newtypes are non-trivial (perform `Into<Arc<str>>` conversion); the confidence newtypes' `new()` performs `clamp(0.0, 1.0)`. None equivalent to `Default::default()`. ✓

### Code quality

- File size (≤500 LOC hard limit): all files in scope are ≤374 (attribution.rs). ✓
- Module boundary: zero `crate::host` code references inside `src/engine/sentiment/`. ✓ (confirmed via `grep -rn 'crate::host' src/engine/sentiment/`)
- Public surface: every new `pub` symbol is used externally or scoped under `pub mod sealed`. The `mod sealed` is correctly `pub(crate)` (line 72).
- Re-exports in `sentiment::mod.rs`: covers Pretrigger, SentimentClassifier, ClassifierError, attribute_signal, attribute_signal_with_fallback, Attribution, all types from types.rs — except **`TurnRole`** (M4).
- Test isolation: new tests use only in-memory state + owned strings. No env vars, no filesystem, no `with_temp_loop_home`/`ENV_LOCK`. ✓
- Test count: 197 unit + 3 integration = 200 total (audit checklist expected ≥176). ✓
- `cargo test --no-default-features` works. ✓
- `cargo test --features test-fixtures` exercises the mock (classifier.rs tests use `#[cfg(test)]` which is OR-ed with the feature gate). ✓
- `cargo clippy --all-targets` clean both with and without the feature. ✓

### Spot-checks the audit prompt called out

- `Pretrigger::with_pattern` gated `#[cfg(any(test, feature = "test-fixtures"))]` — line 109. ✓
- `MockSentimentClassifier` gated `#[cfg(any(test, feature = "test-fixtures"))]` — lines 90, 97, 123, 126. ✓
- `Attribution` struct lives in `attribution.rs` (line 34), NOT in `types.rs`. mod.rs re-exports it from attribution. ✓
- `AttributionMethod` has 4 variants — `DirectMention, PronounResolved, Recency, Salience`. NO `Abstained` variant. ✓ Abstain is `Option::None`.
- `RawClassification::abstain()` documented entry point (types.rs:243-245); `Default` derive is the implementation, but `abstain()` is the documented preferred call. ✓
- Pretrigger character class for apostrophe tolerance is `['‘’]` — ASCII U+0027 + U+2018 left + U+2019 right (smart-quote-tolerant). Pre-research sketch had only two (U+0027 + U+2019); build is slightly more permissive. ✓ Defensible.

### License audit

- `regex = "1"` MIT/Apache attestation in Cargo.toml comment (lines 82-83). ✓
- `regex` is dual MIT/Apache-2.0 (standard rust-lang crate); already transitively pulled by `tracing-subscriber`. Promotion is metadata-only. ✓
- THIRD_PARTY_LICENSES.md covers `regex` via "All other direct dependencies are MIT or Apache-2.0; see Cargo.toml comments." ✓
- No AGPL/GPL/SSPL deps introduced. (Cargo.lock diff shows only regex 1.x family pulled to top-level.) ✓

---

## Summary

| Severity | Count | Examples |
|---|---|---|
| CRITICAL | 0 | — |
| MAJOR | 5 | M1 missing `AsRef<str>` on three new newtypes; M2 stale `events.rs` module doc; M3 missing self-reference dev-dep (D13 half-done); M4 `TurnRole` not re-exported from `mod.rs`; M5 unnamed `0.8` Pass 4 threshold |
| MINOR | 11 | m1 first_word_lowercase allocation; m2 recently_referenced_items full-clone; m3 Default-mock silent abstain; m4 builder-by-move ergonomic quirk; m5/m6/m7 LOC drift over D14 soft target; m8 ItemClassification no Eq (correct); m9 PRONOUNS HashSet over 6 items; m10 TurnRole non_exhaustive not in D10 (intentional); m11 regex::Error at test-fixtures boundary |
| Verified clean | many | all D1-D15 + OQ1-OQ9 either fully met or with the deltas captured above; all 17 sentiment smells (S1-S17) clean or accepted; boundary rule respected; 200 tests pass; clippy clean both feature states |

---

## TL;DR

The Day 15 build cleanly executes the 15 locked decisions and 9 open-question choices from the learn-notes. All 17 sentiment-specific TS-with-Rust-syntax smells (S1-S17) are clean or accepted-with-justification. 197 unit + 3 integration tests pass, clippy is clean both with and without `--features test-fixtures`, and the module boundary (engine never references host) is intact. No CRITICAL findings.

**The single biggest concern is M1** — the three new `Arc<str>` newtypes (`HostVersion`, `ProjectTag`, `LoadedItemId`) ship with `new` + `as_str` + `Display` but omit `AsRef<str>`, diverging from the Day 14 `impl_id_newtype!` macro pattern that every Day-14 newtype follows. This breaks the consistency the macro was introduced to enforce and will surface friction the moment Day 16 starts routing IDs through generic helpers. M3 and M4 are close seconds — M3 leaves D13 only half-implemented (no `loop-daemon` self-reference under `[dev-dependencies]`, so the `MockSentimentClassifier` can't yet be consumed from integration tests under `tests/*.rs`); M4 (`TurnRole` missing from `mod.rs` re-exports) forces external callers to reach into the `types` submodule for one specific type while the rest come through the prelude.
