# Day 15 Pre-Research: Sentiment Pretrigger + Classifier Trait + Attribution

**Date:** 2026-05-13
**Cycle phase:** Pre-research (workflow cycle phase 1)
**Cycle:** Day 15 — sentiment pretrigger, classifier trait, attribution port
**Toolchain assumed:** Rust 1.85 (MSRV), Cargo 1.95.0, edition = "2021" (Day 14 D9 locked; 2024 bump is a separate audit).
**Inputs:** TS at `/Users/slee/projects/loop-archive-2026-05-13/core-ts/src/sentiment/{types,orchestrator,attribution,index}.ts`; design rules `docs/research/sentiment-design-rules.md`; engineering report `sentiment-engineering-2026-05-12.md`; linguistics report `sentiment-linguistics-2026-05-12.md`; Day 14 abstractions (`engine::{Context, Storage, EventSource}`); Day 14 post-research L1/L2/L3 forward-feeds.

---

## Executive summary

Day 15's three deliverables sit at the seam between Day 14's host-agnostic engine abstractions and Day 16's orchestrator. The idiomatic-Rust target across the board: **value-typed pure functions where state is per-call, sealed async traits where the engine owns the contract, `dyn` dispatch for swappable backends, `Option`+`?` for abstain semantics** — the same tower/object_store shape Day 14 adopted.

Recommendations:

1. **`EngineEvent::UserTurn`** adds three flat fields: `parent_event_uuid: Option<String>`, `host_version: Option<HostVersion>`, `project_tag: Option<ProjectTag>` (latter two are `Arc<str>` newtypes). Engine-agnostic naming — no `cc_version`, no `git_branch`. Not a `HostExtras` sub-struct (indirection without value); not host-specific variants (violates Day 14 invariant).
2. **Pretrigger** uses `regex = "1"` (promote transitive to direct), compiled via `LazyLock<Regex>` (stable since 1.80; our 1.85 MSRV is fine). Not `aho-corasick` — our patterns need `\b` and contraction grouping. Wrap in a `Pretrigger` struct for future locale extension and test injection.
3. **`SentimentClassifier`** is a sealed async trait via `async_trait`, object-safe (`Arc<dyn SentimentClassifier>`). Matches Day 14 Storage/EventSource. Method: `classify(&self, &Context, &ClassificationRequest) -> Result<RawClassification, ClassifierError>`.
4. **Attribution** is a pure function `attribute_signal(utterance, &[LoadedItem], &[RecentTurn]) -> Option<Attribution>` — no state machine, no struct, no typestate. Abstain encoded as `None`. Five-pass logic is a flat early-return pipeline.
5. **Module layout:** `src/engine/sentiment/{mod, types, pretrigger, classifier, attribution}.rs` — flat, under 300 LOC each. Day 16 adds `orchestrator.rs`; Day 17 adds `solicitor.rs`.
6. **Tests** ship `MockSentimentClassifier` behind a `test-fixtures` Cargo feature (precedent: Day 14's `MemoryStorage`). Pretrigger + attribution unit-tested inline. Adversarial fixtures land as YAML under `tests/fixtures/sentiment/`, loaded via `include_str!`.
7. **Lessons migration to `Storage::put_if_version`** — **DEFER to Day 16**. Day 15 writes no signals; emission is orchestrator work. Migrating today couples ~300 LOC of fs+test surgery into the sentiment cycle and risks scope overrun.

This keeps Day 15 in one cycle, gives Day 16 stable contracts to build on, and avoids transliterating the TS `classifySentiment` async function (which belongs to the Day 16 orchestrator, not any Day 15 deliverable).

---

## Q1: `EngineEvent::UserTurn` shape finalization

### Survey

The four-field `UserTurn` shape currently in `src/engine/events.rs` (lines 50-56) covers what the pure pretrigger needs (`text`), but the sentiment orchestrator (Day 16) and the auto-memory adapter (later) will need three additional inputs that the Claude Code `WatcherEvent::UserTurn` already carries (lines 21-43 of `src/host/claude_code/jsonl_watcher/events.rs`):

| Field | Used by | Necessity |
|---|---|---|
| `parent_uuid` | Correction-window mining (rule 14, sentiment-design-rules.md): "frustration immediately after lesson-L-influenced turn" needs to identify the previous turn. Also Day 17 solicitor: did the user respond to a previous solicitation? | High — without it, Day 16 orchestrator must reverse-lookup via `session_id` + `event_uuid` ordering, which the engine doesn't currently track. |
| `cc_version` | "Daemon version tripwire" — if the Claude Code version moves outside the known-tested range, we want to log a warning. This is Day 17+ work but the data must flow through. | Medium — could be host-only if engine never reads it, but Day 13 learn-notes (line 42 of `events.rs`) called it out as "Day-N+ tripwire." Carrying it is forward-feed work. |
| `git_branch` | Project routing for sentiment signals (per-project weighting), per-project sentiment thresholds, "this lesson is dead in project X but live in project Y" in Phase C. | Low for Day 15-17, but cheap to thread now while we're touching the type. |

Three options for shape:

**(a) Flatten host-agnostic fields onto `EngineEvent::UserTurn`.** Engine sees them as ordinary fields, ignores those it doesn't need today, reads those it does. Existing `cwd: Option<PathBuf>` is precedent — `cwd` is already in this shape (host populates, engine reads). No new types, no indirection.

**(b) Opaque `host_extras: HostExtras` sub-struct.** `HostExtras` is `#[non_exhaustive]`; engine consumers ignore it; host-specific consumers downcast or call sub-struct accessors. Matches the `hyper::Request::extensions()` typed-extension-map pattern. Pro: forward-compat is structural (add fields without touching `UserTurn`). Con: indirection cost is ergonomic (every read becomes `evt.host_extras.parent_uuid.as_deref()` vs `evt.parent_uuid.as_deref()`), and these three fields aren't really "host extras" — they're host-agnostic concepts that any future host can populate.

**(c) Host-specific event variants** (`EngineEvent::ClaudeCodeUserTurn` etc.). Rejected on the user's "engine is host-agnostic" invariant from Day 14. The whole reason `host::*` exists is so the engine doesn't pattern-match on host identity.

Survey of comparable Rust event/request types:

- **`hyper::Request`**: strongly typed core fields + `extensions: Extensions` typed-map for open-ended add-ons. Justifies (b) when extensions are truly open-ended — not our case (three known fields).
- **`rocket::Request`**: host-known fields flat; user-extensible via `local_cache`. Confirms: don't sub-struct unless you need indirection.
- **`tower::Service<Request, Response>`**: caller-defined Request shape. We own the type, so the precedent's main contribution is "don't bake host knowledge into the engine surface."
- **`lapin` / `rdkafka` events**: per-source variants with typed payloads — never "extras" sub-struct. Confirms (c) is real but only when the host *is* part of the contract.

### Recommendation

**Option (a): flatten the three fields onto `EngineEvent::UserTurn`** with two of them wrapped in lightweight newtypes to prevent stringly-typed confusion:

- `parent_event_uuid: Option<String>` — plain `Option<String>`. Matches existing `event_uuid: String` style. Not a domain concept worth a newtype (it's just an event-graph linkage).
- `host_version: Option<HostVersion>` where `HostVersion(Arc<str>)`. Newtype because comparison against a known-good range is a typed operation (`HostVersion::is_in_tested_range()`), not a string compare.
- `project_tag: Option<ProjectTag>` where `ProjectTag(Arc<str>)`. Newtype because routing semantics (e.g. per-project sentiment threshold lookups) need a typed key, not raw strings.

All three are `Option` because not every future host can populate them (a hypothetical HTTP-API event source might have no `parent_event_uuid`). The engine reads what's present, abstains/defaults gracefully when absent.

Critically: we are **not** adding fields that don't have a known consumer. `cc_version` becomes `host_version` (generalization), `git_branch` becomes `project_tag` (generalization). The renamings move us from "ClaudeCode-specific snake_case fields on a host-agnostic type" to "host-agnostic concepts the engine actually reasons about."

### Code sketch

```rust
// src/engine/events.rs (proposed Day 15 shape)

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum EngineEvent {
    UserTurn {
        session_id: SessionId,
        event_uuid: String,
        parent_event_uuid: Option<String>,   // NEW — correction-window mining
        text: String,
        timestamp: DateTime<Utc>,
        cwd: Option<PathBuf>,
        host_version: Option<HostVersion>,   // NEW — Day 17 tripwire input
        project_tag: Option<ProjectTag>,     // NEW — Phase C routing input
    },
    UserInterrupt {
        session_id: SessionId,
        event_uuid: String,
        parent_event_uuid: Option<String>,   // same forward-feed
        timestamp: DateTime<Utc>,
    },
    SessionStarted { session_id: SessionId, started_at: DateTime<Utc> },
    SessionEnded { session_id: SessionId },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)] pub struct HostVersion(Arc<str>);
#[derive(Debug, Clone, PartialEq, Eq, Hash)] pub struct ProjectTag(Arc<str>);
// Both newtypes ship with new() and as_str(); tripwire impl on HostVersion
// is Day 17 (see OQ4).
```

Adapter translation: `WatcherEvent::UserTurn { parent_uuid, cc_version, git_branch, cwd, ... }` → `EngineEvent::UserTurn { parent_event_uuid: parent_uuid, host_version: Some(HostVersion::new(cc_version)), project_tag: git_branch.map(ProjectTag::new), cwd: Some(cwd), ... }`.

### Trade-offs

Flat fields (chosen — zero indirection; matches existing `cwd` precedent; engine-agnostic naming forces concept-naming) over: `host_extras` sub-struct (indirection per read; these aren't really "extras" — they're concepts every host has); host-specific variants (engine pattern-matches on host — breaks Day 14 invariant).

### Audit smells

- **Carrying raw `cc_version: String` through the engine.** Should be `host_version: Option<HostVersion>`. Naming reveals whether the engine understands what it's holding.
- **Three flat `Option<String>` fields** when the right shape is two typed newtypes + one plain `Option<String>`. The newtype-vs-string decision is per-field; `parent_event_uuid` has no domain semantics so stays a plain `String`.
- **Default-constructed `EngineEvent::UserTurn { ..Default::default() }`** anywhere. `EngineEvent` is `#[non_exhaustive]`; can't be `Default`-derived from outside. Construct via builder if the call site is verbose.
- **`Box<HostVersion>`** or `Arc<HostVersion>`. The newtype already wraps `Arc<str>`; double-wrapping is TS-style "just to be safe."
- **`parent_uuid: Option<&str>`** in a function signature where the borrow doesn't live across an await point. Take `Option<&str>` when borrowing is OK, `Option<String>` when ownership is required; don't mix `Option<&String>` (never idiomatic).

---

## Q2: Pretrigger — regex / DFA / aho-corasick

### Survey

The TS pretrigger is one big alternation regex (~30 alternations covering thanks/perfect/wrong/broken/contractions/interrupt-markers — see `loop-archive-2026-05-13/core-ts/src/sentiment/types.ts:87`). Word-boundary anchored, case-insensitive, smart-quote-tolerant. Compiled once via JS regex literal.

Three Rust crate options:

| Crate | Version | Strength | Weakness |
|---|---|---|---|
| **`regex`** | 1.11.1 (Oct 2024) | Linear-time, no backtracking, full Unicode word-boundary support, `Regex::is_match` is the universal Rust regex API. Used by `ripgrep`, `comrak`, `clippy`. | Compile cost is non-trivial; need lazy init. |
| **`aho-corasick`** | 1.1.3 | Fast multi-pattern literal search; supports `case-insensitive`; very fast for hundreds-of-needles workloads. Used by `regex` internally for prefiltering, by `ripgrep` for fixed-strings mode. | No `\b` word-boundary, no contraction patterns (`do\s*n['']?t`). Our patterns need both. |
| **`regex-automata`** | 0.4.x (transitive of `regex`) | Lower-level; lets you build a `DFA` or `lazy::DFA` directly; preserves the pattern from `regex` parsing. | More API surface than we need; `regex` already gives us this via `Regex::new`. |

Use-case in the wild:

- **ripgrep**: uses `regex` for the user-supplied pattern, `aho-corasick` only when the pattern is detected as pure literals. Pattern-quality detection is opaque to users.
- **comrak**: uses `regex` for inline-link and footnote parsing — alternation over a few dozen literal substrings, very similar shape to ours. Lazy-initialized with `OnceLock`.
- **clippy**: uses `regex` for some lints; pre-`LazyLock` era so `lazy_static!` everywhere. Modern Rust would use `LazyLock`.
- **`tracing-subscriber`**: already pulls `regex` (1.11) as a direct dep for env-filter parsing. We already pay the binary-size cost.

The performance comparison is irrelevant at our scale (one regex match per user turn, ~one turn per second peak, ~0.02 user turns/sec sustained). What matters is **clarity** (regex literal reads like the TS), **maintainability** (lexicon adds are a one-line append), and **correctness** (word boundaries on contractions). All three favor `regex`.

### Recommendation

Use `regex = "1"` (1.11.x). Promote to direct dep in `Cargo.toml` (it's already transitive of `tracing-subscriber`). Compile once into a `LazyLock<Regex>`.

`LazyLock` (stable since 1.80; our MSRV is 1.85, so safe) over `OnceLock` because we avoid the boilerplate `static.get_or_init(|| { ... })` per access — `LazyLock` initializes on first deref. Also over `lazy_static!` because that crate is in maintenance mode and `LazyLock` is the std-blessed replacement (per the `lazy_static` README, 2023).

Wrap inside a `Pretrigger` struct rather than expose the raw `LazyLock<Regex>` at module level. The struct gives us:
1. A natural place to add multilingual triggers later (`Pretrigger::for_locale(Locale::En)`).
2. The ability to inject a different regex in tests (`Pretrigger::with_pattern(r"foo")`).
3. An API surface (`fires_on(&str) -> bool`) that documents what the pretrigger guarantees without forcing callers to know it's a regex.

The default `Pretrigger` uses the static `LazyLock<Regex>`; the test/custom constructor compiles a fresh regex.

### Code sketch

```rust
// src/engine/sentiment/pretrigger.rs (excerpt)

use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone)]
pub struct Pretrigger { regex: PretriggerRegex }

#[derive(Debug, Clone)]
enum PretriggerRegex { Default, Custom(std::sync::Arc<Regex>) }

impl Pretrigger {
    pub fn default_en() -> Self { Self { regex: PretriggerRegex::Default } }

    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn with_pattern(pattern: &str) -> Result<Self, regex::Error> {
        let r = Regex::new(pattern)?;
        Ok(Self { regex: PretriggerRegex::Custom(std::sync::Arc::new(r)) })
    }

    pub fn fires_on(&self, text: &str) -> bool {
        match &self.regex {
            PretriggerRegex::Default => DEFAULT_REGEX.is_match(text),
            PretriggerRegex::Custom(r) => r.is_match(text),
        }
    }
}

// Pattern: (?i) for case-insensitive (replaces TS /i flag); \b on both
// ends; smart-quote ['’]? on every apostrophe site for the contraction
// patterns (does\s*n['’]?t etc.). Built from the audit-A1-merged TS regex.
static DEFAULT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(thanks?|thank\s+you|perfect|great|exactly|nailed|works?|amazing|awesome|love\s+(it|that)|cool|nice|no|wrong|incorrect|broken|useless|sucks|dumb|hate\s+(it|that)|does\s*n['’]?t|did\s*n['’]?t|do\s*n['’]?t|is\s*n['’]?t|was\s*n['’]?t|are\s*n['’]?t|were\s*n['’]?t|wo\s*n['’]?t|ca\s*n['’]?t|could\s*n['’]?t|should\s*n['’]?t|stop|nope|wtf|ugh|meh|huh\??|what\??|instead|hmm)\b")
        .expect("pretrigger regex is compile-time correct")
});
```

Notes: `(?i)` over `to_lowercase()` (allocation per call); raw string literal `r"..."` so `\b` is the regex word-boundary not the backspace char; `expect()` is fine — failure here is a developer bug not a runtime condition.

### Trade-offs

`regex` + `LazyLock` + `Pretrigger` struct (chosen) over: raw `static LAZY: LazyLock<Regex>` at module scope (no test injection); `aho-corasick` two-stage hybrid (two-stage, perf gain irrelevant); `OnceLock` + init closure (boilerplate); `lazy_static!` (maintenance mode); per-classifier owned regex (re-allocates).

### Audit smells

- **`Regex::new(...).unwrap()` at module top level outside `LazyLock`** — initialization at first use, not at module load.
- **String constant for the pattern with raw `\b` instead of `\\b`** — `\b` in a non-raw Rust string is the backspace control character, not the regex word-boundary. Always use raw string literals (`r"..."`) for regex patterns.
- **`Regex::is_match(&text.to_lowercase())`** to fake case-insensitivity. Always use `(?i)` in the pattern instead — `to_lowercase()` allocates per call.
- **Borrowing the static directly across an `await`.** `LazyLock<Regex>` derefs to `&Regex` which is `'static`, so this is fine in practice, but the `Pretrigger` struct shape sidesteps the question.
- **Module-level `pub static DEFAULT_REGEX`** — keep it private; expose only `Pretrigger`.

---

## Q3: `SentimentClassifier` trait shape

### Survey

The TS trait is `interface SentimentClassifierClient { classify(input): Promise<RawClassification> }` — single async method, takes a curated input, returns a structured result. The implementations are (today): a real Anthropic client (`HaikuSentimentClient`) and a mock for tests. Tomorrow: a local Ollama client (per design-rules open-question 3).

Three Rust trait shapes to compare:

**Shape A — async object-safe via `async_trait`:**
```rust
#[async_trait]
pub trait SentimentClassifier: Send + Sync + sealed::Sealed {
    async fn classify(
        &self,
        ctx: &Context,
        request: &ClassificationRequest,
    ) -> Result<RawClassification, ClassifierError>;
}
```
Direct port of TS interface. Same shape as `engine::Storage` (Day 14 D3). Lets us hold `Arc<dyn SentimentClassifier>` in the orchestrator and swap impls via DI. Cost: `Box<dyn Future>` per call (negligible at one call per ~12 turns).

**Shape B — generic `<C: SentimentClassifier>` on the orchestrator:**
```rust
pub trait SentimentClassifier: Send + Sync {
    fn classify(&self, ctx: &Context, request: &ClassificationRequest)
        -> impl Future<Output = Result<RawClassification, ClassifierError>> + Send;
}
```
Native `async fn in trait` (stable since 1.75) with the manual `+ Send` bound on the returned future. Zero dispatch cost. Cost: generics propagate through the orchestrator's type (`Orchestrator<C: SentimentClassifier>`), which then propagates through the daemon's wiring; if we want two backends behind a config flag, we need `enum BackendChoice { Anthropic(Orchestrator<HaikuClient>), Local(Orchestrator<OllamaClient>) }` — duplicates the orchestrator per backend. Day 14 D3 hit this exact tradeoff and chose `dyn` for the same reason.

**Shape C — closure type:**
```rust
pub type Classifier = Arc<
    dyn Fn(&Context, &ClassificationRequest) -> BoxFuture<'static, Result<...>>
        + Send + Sync,
>;
```
Plain function-pointer-ish; no trait at all. Pro: maximally simple. Con: no associated types for variants of the client (config, retry policy, model selection); name disappears from type signatures so error messages get cryptic; can't add `name() -> &'static str` for diagnostics.

Survey of "swap-at-runtime backend behind a trait" patterns:

- **`tokenizers`**: concrete struct with internal enum variants — dispatch at data layer, no trait. Works when backends are finite & internal.
- **`tower::Service`**: native async fn in trait, generic dispatch. Tower's middleware-stack composition is overkill here.
- **`bb8` / `deadpool`** connection pools: `async_trait` + sealed-ish, swappable backend. Direct precedent.
- **Day 14 `engine::Storage`**: `async_trait` + sealed + `dyn`. Engine-internal convention already set.
- **`serde::{Serializer, Deserializer}`**: monomorphized per-adapter — wrong shape (we have one classifier per daemon, not one per call).

### Recommendation

**Shape A: async object-safe via `async_trait`, sealed.** Matches Day 14's `Storage` precedent verbatim. Specific decisions:

1. **`async_trait` macro, not native async-fn-in-trait.** Why: object safety + `Send`-bounded futures is the use case `async_trait` was made for. Native AFIT works but requires explicit `+ Send` per method and is awkward with `dyn` (the new `dyn AFIT` machinery is stable as of 1.75 but has rough edges around return-position-impl-trait dispatch tables). Day 14 D10 already locked `async_trait = "0.1"`; consistency with Storage/EventSource is high-value.

2. **Sealed via `sealed::Sealed` in `engine::sentiment::classifier::sealed`.** External crates can't implement `SentimentClassifier`. The shipped impls are (Day 15): the test `MockSentimentClassifier` (in this module, behind `#[cfg(any(test, feature = "test-fixtures"))]`). (Day 16+): the real `HaikuSentimentClassifier` (will live in `host::claude_code::haiku_client` or a similar adapter location). External "I want my own sentiment backend" is not supported — the design rules' confidence-calibration and abstention rules are integral to the engine, not customizable.

3. **Input type: `ClassificationRequest`**, not `&EngineEvent::UserTurn` directly. The classifier needs the *windowed* input (last 4-6 turns + loaded items + session_id), not a single event. Building the request is the orchestrator's job (Day 16); the classifier consumes the curated form.

4. **Output type: `RawClassification` enum-mode**, not the raw TS shape. TS has `RawClassification { perItem: [{itemId, polarity, confidence, evidence, hazards}], globalHazards }`. Polarity in TS is `'positive'|'negative'|'neutral'`. In Rust: make `Polarity` an enum, make per-item a `Vec<ItemClassification>`, give `ItemClassification` a typed `Hazards` bitflag or `Vec<Hazard>` enum, etc. Detailed in Q5/Q8.

5. **Take `&Context` even though the classifier doesn't read it today.** Forward-feed: per-tenant classifier overrides, per-user calibration tables, per-team classifier selection are all `Context`-driven. Adding it now is non-breaking; adding it later requires touching every caller.

6. **Error type: dedicated `ClassifierError` enum**, not `anyhow::Error`. Engine public surface uses named error enums (Day 14 audit smell list, line 1080 of pre-research).

### Code sketch

```rust
// src/engine/sentiment/classifier.rs (excerpt)

#[async_trait]
pub trait SentimentClassifier: Send + Sync + std::fmt::Debug + sealed::Sealed {
    async fn classify(
        &self,
        ctx: &Context,
        request: &ClassificationRequest,
    ) -> Result<RawClassification, ClassifierError>;

    fn name(&self) -> &'static str;
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClassifierError {
    #[error("classifier transport error: {0}")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("classifier returned unparseable output: {0}")]
    InvalidOutput(String),
    #[error("classifier rate-limited; retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u32 },
    #[error("classifier timeout after {elapsed_ms}ms")]
    Timeout { elapsed_ms: u64 },
}

mod sealed { pub trait Sealed {} }

// Day 15 ships the mock; Day 16 ships the production classifier in
// `host::claude_code::haiku_client`.
#[cfg(any(test, feature = "test-fixtures"))]
pub mod mock {
    // MockSentimentClassifier — builder-chain API (with_response / with_error),
    // Mutex<VecDeque> of canned responses, AtomicUsize call_count.
    // Returns RawClassification::abstain() when the queue is empty.
    // Implements Sealed + SentimentClassifier; name() = "mock".
}
```

### Trade-offs

`async_trait` + sealed + `dyn` (chosen — consistent with Day 14 Storage/EventSource; per-call boxing negligible vs LLM latency) over: native AFIT + generic `<C>` (generics propagate through orchestrator; doubles types per backend; AFIT-with-sealed has rough compile errors); closure type alias (no `name()` or other methods); enum sum type of concrete clients (no third-party extensibility).

### Audit smells

- **`Box<dyn SentimentClassifier>` everywhere** when `Arc<dyn SentimentClassifier>` is the cheaper-clone shape for a singleton.
- **`Rc<dyn ...>`** anywhere in engine code (orchestrator may cross threads; need `Arc`).
- **Returning `BoxFuture<'static, ...>` manually** when `async_trait` does the macro for us.
- **`async fn name(&self) -> &'static str`** — `name()` is sync; making it async via the macro is unnecessary work.
- **`Result<RawClassification, anyhow::Error>`** — engine public boundary uses named error enums.
- **`fn classify(&self, input: SentimentSubagentInput)`** — TS-style naming (`SentimentSubagentInput`, `SentimentSubagentOutput`). Rust naming should drop the "subagent" prefix; the type already lives under `engine::sentiment` so the namespace establishes the purpose.

---

## Q4: Attribution algorithm port

### Survey

TS algorithm is `attributeSignal(utterance, loadedItems, recentTurns, classifierFallback?) -> AttributionResult | null`: five passes, early-return on first match, abstain on no-match. **Pure function** — no state, no side effects. Passes: (1) direct mention keyword/id → 0.95; (2) pronoun anaphor on prior assistant turn → 0.80; (3) recency single-candidate → 0.65; (4) classifier-judged top-K when 2-5 candidates → classifier output; (5) abstain.

Sub-questions:

- **State-machine encoding:** none. Plain early-return is right. No shared state between passes (each independent). No caller sequencing (one call in, one result out). Typestate / actor / `AttributionState` enum would all be transliterations.
- **State location:** none for attribution itself. Per-session state (turn buffer, rate-limit timestamps, attribution history for audit-A2 abstain skip) belongs to the orchestrator (Day 16).
- **Concurrency:** single `EventSource` stream → single sequential consumer per session. `attribute_signal` has no shared state. Multi-session multiplexing is Day 16 (likely `Arc<DashMap<SessionId, Mutex<SessionState>>>` or per-session-task mpsc).
- **Return shape:** TS `AttributionResult | null` with stringly-typed method union. Rust: `Option<Attribution>`; `Abstained` is `None`, not a sixth enum variant.

### Recommendation

A pure function `attribute_signal(...) -> Option<Attribution>` with:

- `Attribution` = `{ item_id: LoadedItemId, method: AttributionMethod, confidence: AttributionConfidence }`
- `AttributionMethod` = closed enum `{ DirectMention, PronounResolved, Recency, Salience }`. **No `Abstained` variant** — abstain is `Option::None`.
- `AttributionConfidence` = newtype around `f32` clamped 0..=1.
- `LoadedItemId` = newtype around `Arc<str>` (matches `SessionId` pattern from Day 14).

### Code sketch

```rust
// src/engine/sentiment/attribution.rs (excerpt)

#[derive(Debug, Clone, PartialEq)]
pub struct Attribution {
    pub item_id: LoadedItemId,
    pub method: AttributionMethod,
    pub confidence: AttributionConfidence,
}

/// Five-pass with abstain default. Pure function.
pub fn attribute_signal(
    utterance: &str,
    loaded_items: &[LoadedItem],
    recent_turns: &[RecentTurn],
) -> Option<Attribution> {
    pass1_direct_mention(utterance, loaded_items)
        .or_else(|| pass2_pronoun_anaphor(utterance, loaded_items, recent_turns))
        .or_else(|| pass3_single_recent(loaded_items, recent_turns))
    // Pass 4 only fires through the _with_fallback variant.
    // Pass 5 = abstain = the None at the end of the chain.
}

/// Variant with Pass 4 (classifier-judged top-K). The orchestrator
/// passes its classifier-derived judging closure here; this keeps
/// attribution decoupled from `SentimentClassifier`.
pub fn attribute_signal_with_fallback<F>(
    utterance: &str,
    loaded_items: &[LoadedItem],
    recent_turns: &[RecentTurn],
    fallback: F,
) -> Option<Attribution>
where
    F: FnOnce(&[LoadedItem]) -> Option<(LoadedItemId, AttributionConfidence)>,
{
    if let Some(a) = attribute_signal(utterance, loaded_items, recent_turns) {
        return Some(a);
    }
    let recent = recent_referenced(loaded_items, recent_turns);
    if (2..=5).contains(&recent.len()) {
        if let Some((item_id, conf)) = fallback(&recent) {
            if conf.value() >= 0.8 {
                return Some(Attribution {
                    item_id, method: AttributionMethod::Salience, confidence: conf,
                });
            }
        }
    }
    None
}
```

Notes:

- **Pass 4 is a separate function with a generic closure parameter.** Rust's `Option<FnOnce>` is awkward (can't easily call an optional `FnOnce`); two functions is cleaner. `F: FnOnce(...)` is monomorphized — zero allocation per call.
- **`.or_else` chain over `if let` early-return**: both idiomatic. Use `.or_else` (lazy) — `.or` would evaluate all passes.
- **`to_lowercase()` allocation** in Pass 1 mirrors the TS. Future perf pass can use `eq_ignore_ascii_case` per substring; not Day 15.

### Trade-offs

Pure-function pipeline (chosen) vs alternatives: `Option<&mut dyn FnMut>` fallback (awkward call-site), state-machine enum (no inter-pass state to track), typestate `Attributor<P1Done>` (no invariant to guarantee — over-engineering), `Attributor` trait (YAGNI — design rules lock the algorithm).

### Audit smells

- **`AttributionMethod::Abstained` variant** — should be `Option::None`, not a sixth enum value. TS uses a stringly-typed union including `'abstained'`; Rust convention is to use the type system (Option/Result) for present/absent.
- **`Result<Attribution, AttributionError>`** — there's no error case; abstain isn't an error. Use `Option<Attribution>`.
- **`Vec<Attribution>` return type "in case we want multiple"** — design rules lock single-best attribution.
- **State struct field initialization (`AttributionState::new()`)** — pure function doesn't need state. Smell of "I'm porting an OO design."
- **Generic `<F: FnOnce(...)>` everywhere** when the function only takes a fallback once. `FnOnce` is right; resist the urge to use `Fn` "in case."
- **`stopwords: HashSet<String>` field** on an `Attributor` struct. The TS has a module-level `STOPWORDS` set; Rust convention is `static STOPWORDS: phf::Set<&'static str>` (compile-time) or `LazyLock<HashSet<&'static str>>` (lazy). Don't make it owned per attributor.
- **`.collect::<Vec<_>>()` on iterators that don't need to materialize**: e.g. `recent.iter().filter(...).collect::<Vec<_>>().len()` should be `.count()`. (TS has implicit array materialization; Rust doesn't.)

---

## Q5: Module organization within `engine::sentiment`

### Survey

Day 14 D1 settled engine-vs-host boundary. Engine modules are either single `.rs` files or directories with `mod.rs` + sub-files (e.g. `storage/{mod, error, filesystem, key, memory, version}.rs`). Convention: directory when there's internal substructure.

### Recommendation

Day 15 layout:

```
src/engine/sentiment/
├── mod.rs            re-exports + module docs (no impl)
├── types.rs          LoadedItem, RecentTurn, Polarity, Hazard,
│                     AttributionMethod, AttributionConfidence, LoadedItemId,
│                     ClassificationRequest, RawClassification
├── pretrigger.rs     Pretrigger struct + LazyLock<Regex>
├── classifier.rs     SentimentClassifier trait + ClassifierError + sealed
│                     + #[cfg(test/test-fixtures)] mock module
└── attribution.rs    attribute_signal + _with_fallback + Attribution
```

Day 16 adds `orchestrator.rs`; Day 17 adds `solicitor.rs`.

Decisions:

- **`types.rs` is one shared file**, not split per-concept (storage split because it has 5+ unrelated concepts; sentiment types are tightly coupled). Cap ~250 LOC; revisit if it grows.
- **No `engine::sentiment::types::*` re-export** — module IS the surface; path is `engine::sentiment::Polarity`.
- **`#[non_exhaustive]`** on `Hazard`, `AttributionMethod`, `Attribution`, `ClassifierError`, `LoadedItemKind`. NOT on `Polarity` (fixed three variants).

### Trade-offs

Flat 4-5 files (chosen) vs sub-modules `{types, engine}` (over-engineered), one mega `sentiment.rs` (exceeds 500 LOC cap), per-pass attribution split (5 tiny files of 10 LOC each).

### Audit smells

- **`pub use super::types::*` in `mod.rs`** — use named items.
- **Same type defined in two files** — single source of truth in `types.rs`.
- **`attribution.rs` importing from `classifier.rs`** — fallback closure is generic `FnOnce`, not a trait import; no module cycle needed.
- **`types/mod.rs` with `pub mod loaded_item; pub mod polarity;`** — over-decomposition.

---

## Q6: Test strategy for sentiment

### Survey

Day 14 D7 locked: pure-logic tests use `MemoryStorage`; integration tests use `LocalFsStorage::new_at(&tempdir)`. `MemoryStorage` ships *in the engine* as a `pub` test fixture next to `LocalFsStorage`. Same precedent applies here.

Three test surfaces:

- **Pretrigger:** pure unit, table-driven (~30-50 cases from TS A1 fixtures).
- **Attribution:** pure-function unit, five-pass coverage matrix (~20 cases per pass + abstain).
- **Classifier trait:** mock impl for orchestrator/solicitor tests (Day 16/17).

### Recommendation

- **`MockSentimentClassifier`** lives in `src/engine/sentiment/classifier.rs` under `pub mod mock`, gated `#[cfg(any(test, feature = "test-fixtures"))]`. Add `test-fixtures = []` feature in `Cargo.toml`. (Same pattern as `tokio`'s `test-util` feature.) Gating behind a feature so the production binary doesn't ship the fake; `cargo test` enables the feature for the crate's own tests.
- **Adversarial fixtures** as YAML under `tests/fixtures/sentiment/`:
  - `pretrigger_positive.yaml` / `pretrigger_negative.yaml`
  - `attribution_direct.yaml` / `_pronoun.yaml` / `_recency.yaml` / `_abstain.yaml`
- **Fixture loader** in `tests/common/mod.rs`: `fn fixtures<T: DeserializeOwned>(name: &str) -> Vec<T>` via `include_str!` + `serde_yml::from_str`. No runtime I/O.
- **Pure-fn tests** stay inline `#[cfg(test)] mod tests` per Day 14 convention.

### Trade-offs

`test-fixtures` feature (chosen) vs unconditional `pub mod mock` (production binary ships the fake; auditor-flaggable) vs `#[cfg(test)]` only (tests/*.rs can't see it).

### Audit smells

- **`pub mod mock`** not gated — fake classifier ships in production.
- **`MockSentimentClassifier::default()`** returning empty queue — should require explicit setup so silent-abstain doesn't mask orchestrator bugs.
- **Test fixtures via `build.rs`** — opaque on failure; prefer `include_str!`.
- **`serial_test` crate** anywhere in `engine::sentiment` — abstractions should support parallel tests.

---

## Q7: Lessons migration to `Storage::put_if_version`

### Survey

Per Day 14 L2, the natural trigger for `LocalFsStorage::put_if_version` (stubbed Day 14) is "orchestrator writes signals." Today `engine::lessons::signals::record_sentiment_signal` uses direct `fs::read` → parse → write-tmp → `fs::rename` plus `with_lock(path, ...)` flock + ENV_LOCK in tests.

Migration cost: implement `put_if_version` + `get_with_version` on `LocalFsStorage`, refactor `record_sentiment_signal` (and the loader it calls) to take `&Context, &dyn Storage`, migrate 7+ tests from `with_temp_loop_home`/`ENV_LOCK` to a `TestHarness`, plus cross-process flock-vs-CAS semantic audit (TS MCP server still writes via flock — interleaving with Rust CAS can race). Estimate: 250-500 LOC across 4-5 files + a dedicated pre-research.

### Recommendation

**DEFER lessons migration to Day 16.** Reasoning:

1. Day 15 writes no signals — three locked deliverables are pure-logic. Migrating I/O today is scope creep.
2. Day 16's orchestrator IS the new caller that will write signals; natural co-migration moment.
3. Cross-process flock-vs-CAS semantics needs its own pre-research; probably Day 16 sub-target.
4. Day 14 stubs return `StorageError::Backend(...)` and are not called anywhere; they only become callable when Day 16 builds the orchestrator.

**For Day 15:** add a doc-comment to `record_sentiment_signal` noting "Day 16 migration target." No code change.
**For Day 16 pre-research:** spawn a research agent on flock+CAS interaction and the lock module's future.

### Trade-offs

Defer to Day 16 (chosen) vs migrate Day 15.5 split-commit (doubles Day 15 audit count) vs migrate as part of Day 15 (5 concerns in one cycle — audit risk).

### Audit smells

- **Calling `engine::paths::loop_home()` in Day 15 code** — engine should use Context+Storage; sentinel for "haven't actually adopted Day 14."
- **`use crate::engine::paths::ENV_LOCK`** in Day 15 tests — ENV_LOCK is retiring.
- **Day 15 build touches `lessons/signals.rs`** — that's Day 16 scope.

---

## Q8: TS-with-Rust-syntax smells specific to sentiment

Building on the 17-smell list from Day 14 pre-research, this section adds **sentiment-specific** smells the audit agent will check Day 15 build code for.

### S1. `String` polarities, methods, kinds

The TS uses `polarity: 'positive' | 'negative' | 'neutral'` as a string union. Rust:

- **WRONG:** `pub polarity: String` — stringly-typed; no compile-time guarantee of valid values; allocation per polarity assignment.
- **RIGHT:** `pub polarity: Polarity` where `Polarity` is `#[derive(Clone, Copy, PartialEq, Eq, Debug)] enum Polarity { Positive, Negative, Neutral }`. `Copy` is free (it's an enum tag); exhaustive matching at the use sites.

Same smell shape applies to `attribution_method: String` → `AttributionMethod` enum, `hazards: Vec<String>` → `Vec<Hazard>` enum, `item_kind: String` → `LoadedItemKind` enum.

### S2. Regex match positions as raw `usize` pairs

WRONG `pub start: usize, pub end: usize`. RIGHT `pub span: std::ops::Range<usize>` (or typed `Span(Range<usize>)` newtype if span operations have invariants).

### S3. `f32` confidences without bounds

WRONG `pub confidence: f32` (downstream `if confidence > 0.85` may see 1.5). RIGHT newtype `Confidence(f32)` with validated `::new()`. Three distinct types (`AttributionConfidence` / `ClassifierConfidence` / `CalibratedConfidence`) prevent cross-stage confusion.

### S4. `Option<Option<T>>` for nullable-and-might-not-exist

WRONG `Option<Option<String>>`. RIGHT `Option<RecentTurn>` where `RecentTurn::text: String` (empty string is valid).

### S5. `Box<dyn SentimentClassifier>` taken by ephemeral helpers

Orchestrator field is `Arc<dyn SentimentClassifier>` (right). Helper functions taking ownership of a `Box<dyn ...>` is wrong shape — take `&dyn SentimentClassifier` borrow; caller keeps the `Arc`.

### S6. Hazards as `Vec<String>` instead of `Vec<Hazard>` / bitflags

WRONG: `Vec<String>` — no compile-time validation, allocates, `iter().any(...)` set-membership. RIGHT: `Vec<Hazard>` enum (Day 15 KISS). Future: `bitflags::bitflags!` for hot-path set ops (revisit if needed).

### S7. `Vec<Box<dyn AttributionPass>>` extensible registry

TS-style strategy pattern. Design rules lock five passes; no extensibility needed. Use enum or fixed-function dispatch.

### S8. Walls of `if let Some(x) = ... { return ... } else { ... return None }`

WRONG: nested conditional walls. RIGHT: `.or_else` chain or flat early-return-per-pass. `.or` (eager) vs `.or_else` (lazy) — use `.or_else` for cost-bearing pass functions.

### S9. `HashMap<String, ...>` for known-finite sets

WRONG: `HashMap<String, &str>` mapping polarity-string to label. RIGHT: exhaustive `match polarity { ... }`. HashMap only for open-ended keys.

### S10. `async fn` on pure-CPU methods

Pretrigger + attribution = sync. Classifier = async (LLM call). Smell: marking attribution `async` for "consistency" with the classifier. Rust async is opt-in.

### S11. `Arc<Mutex<SessionState>>` premature

Day 15 has no session state. Any `Arc<Mutex<...>>` in Day 15 build code is reaching for Day 16 work. Reject — Day 16 orchestrator picks its concurrency model on actual requirements.

### S12. Manually-typed `Pin<Box<dyn Future + Send + 'static>>`

`async_trait` macro handles the boxing. Smell: re-implementing what the macro already does.

### S13. `tokio::spawn` inside attribution / pretrigger

Pure CPU; no async scheduling needed. Smell: any `spawn` in Day 15 build code outside the (Day 16) orchestrator.

### S14. `String` for `LoadedItem::id`

Should be `LoadedItemId(Arc<str>)` matching `SessionId` pattern. IDs are passed around dozens of times during attribution; `String` clones are O(n) vs `Arc<str>` O(1).

### S15. `Polarity::from_str("positive")` inside the engine

Parsing string polarity belongs at the engine BOUNDARY (Anthropic JSON deserialization in the haiku client adapter), not inside engine logic. Engine works with `Polarity` enum; adapter does the parse.

### S16. `Vec<RecentTurn>` taken by value when borrow would suffice

Helper functions take `&[RecentTurn]`, not `Vec<RecentTurn>`. TS clones arrays freely; Rust borrows slices.

### S17. Premature genericism `<R: Read>` for regex pattern source

If pretrigger ever loads patterns from a file, take `&str`, not `<R: Read>`. The only impl will ever be `&str`; premature genericism is a Rust beginner habit.

---

## Locked decisions for Day 15 learn-notes

These are clear-best answers, ready to lock as build inputs:

### D1. `EngineEvent::UserTurn` shape
Add three flat fields: `parent_event_uuid: Option<String>`, `host_version: Option<HostVersion>`, `project_tag: Option<ProjectTag>`. Newtypes for the latter two (`Arc<str>` inside). No `HostExtras` sub-struct; no host-specific variants. (See Q1.)

### D2. Pretrigger
Use `regex = "1"`, promoted to direct dep. Compile once via `LazyLock<Regex>`. Wrap in a `Pretrigger` struct with `default_en()` and `with_pattern(...)` constructors. (See Q2.)

### D3. `SentimentClassifier` trait
Sealed async trait via `async_trait` macro. Object-safe (`Arc<dyn SentimentClassifier>`). Methods: `classify(&self, &Context, &ClassificationRequest) -> Result<RawClassification, ClassifierError>` + `name(&self) -> &'static str`. (See Q3.)

### D4. Attribution
Pure function `attribute_signal(utterance, &[LoadedItem], &[RecentTurn]) -> Option<Attribution>` + variant `_with_fallback` accepting a `FnOnce`. No state machine, no struct. (See Q4.)

### D5. Module layout
`src/engine/sentiment/{mod, types, pretrigger, classifier, attribution}.rs`. Flat. Orchestrator (Day 16) and solicitor (Day 17) land as additional sibling files. (See Q5.)

### D6. Test strategy
- `MockSentimentClassifier` ships behind `test-fixtures` Cargo feature in `engine/sentiment/classifier.rs`.
- Adversarial YAML fixtures under `tests/fixtures/sentiment/`, loaded via `include_str!`.
- Inline `#[cfg(test)] mod tests` for pure-function tests on pretrigger + attribution. (See Q6.)

### D7. Lessons migration
**DEFERRED to Day 16.** Day 15 ships pure-logic code with zero changes to `engine::lessons::*`. Cross-process flock-vs-CAS semantics is a Day 16 pre-research sub-target. (See Q7.)

### D8. Naming
Drop "subagent" from type names. `SentimentSubagentInput` → `ClassificationRequest`. `SentimentSubagentOutput` → not introduced today; orchestrator builds its own output shape in Day 16. `SentimentClassifierClient` → `SentimentClassifier`.

### D9. Confidence newtypes
Three distinct types: `AttributionConfidence`, `ClassifierConfidence`, `CalibratedConfidence`. All wrap `f32` and clamp/validate on construction. Polarity-threshold logic in the orchestrator (Day 16) uses `CalibratedConfidence` exclusively.

### D10. Polarity, Hazard, AttributionMethod, LoadedItemKind
All enums, `Copy + Clone + Debug + PartialEq + Eq + Hash`. `Polarity` is closed (3 variants, no `#[non_exhaustive]`). `Hazard`, `AttributionMethod`, `LoadedItemKind` are `#[non_exhaustive]`.

### D11. `LoadedItemId`
Newtype `Arc<str>` matching `SessionId` pattern. Replaces TS `string` id everywhere in the sentiment module.

### D12. Dependencies added
- `regex = "1"` (already transitive; promote to direct dep; MIT/Apache).
- (No others.) `async-trait`, `bytes`, `futures`, `thiserror`, `chrono` already in deps.

### D13. Feature flags
Add `test-fixtures = []` to `[features]` in `Cargo.toml`. Add to crate's own dev-dependencies via the self-reference trick or enable in `[dev-dependencies]` setup so integration tests can see the mock.

### D14. File-size budget
All Day 15 files target <300 LOC each. If `types.rs` approaches 250 LOC, split first by concern (e.g. `confidence.rs`).

### D15. License audit
`regex` 1.x is dual MIT/Apache-2.0. Already in `THIRD_PARTY_LICENSES.md` via transitive; promote to direct-dep listing.

---

## Open questions to resolve in learn phase

These are decisions that benefit from one more round of discussion before the build phase locks them:

### OQ1. Ship `attribute_signal_with_fallback` Day 15 or Day 16?
Pass 4 needs a "judge top-K" capability the current `SentimentClassifier::classify` shape doesn't expose. Two options: (a) ship `_with_fallback` public API Day 15 with no caller — orchestrator wires Day 16; (b) hold the fallback variant for Day 16 when the orchestrator's needs are concrete. **Recommend (a)** — the signature is small, the closure-generic shape is locked, Day 16 just adds the caller.

### OQ2. `MockSentimentClassifier` API — builder / setter / scripted?
Builder chain (`Mock::new().with_response(r1).with_response(r2)`), mutable setter, or up-front script (`Mock::from_script(&[r1, r2])`). **Recommend builder chain** for simplicity; revisit if Day 16/17 tests need richer sequencing (e.g. response-by-call-shape rather than by index).

### OQ3. `Pretrigger` per-locale today, or defer?
`Pretrigger::default_en()` bakes locale into constructor naming; `Pretrigger::default()` keeps it implicit and adds multilingual constructors when needed. **Recommend `Pretrigger::default()` for Day 15** — KISS; no multilingual in Day 15-17 plan.

### OQ4. `HostVersion::is_in_tested_range` impl today or Day 17?
Three options: ship returning `true` (placeholder), don't define the method (bare type), or punt to a `tripwire` sub-module in Day 17. **Recommend** bare type for Day 15; Day 17 adds the tripwire impl as part of solicitor work.

### OQ5. `ProjectTag` derivation — host vs engine?
Principled answer: host derives `project_tag` from `cwd` + `git_branch`. Engine is host-agnostic; it can't know which derivation policy is right. Day 15 doesn't consume `project_tag`, so academic — but **decide in learn phase whether to write the convention down** or punt to Phase C.

### OQ6. `ClassificationRequest` — owned or borrowed?
Owned (`Vec<RecentTurn>` etc., self-contained, ships across `await` trivially) vs borrowed (`&'a [RecentTurn]`, zero-copy, lifetime propagation). **Recommend owned** — bounded size (4-6 turns + ≤20 items × small structs), aligns with `Arc<str>` cheap-clone philosophy from Day 14.

### OQ7. `RawClassification::abstain()` constructor
Should there be a named "nothing to report" constructor? Empty `per_item` + empty `global_hazards`. **Recommend yes** — explicit-abstain is more readable than `RawClassification::default()`.

### OQ8. Adversarial-fixture coverage for Day 15 audit
Full 50-case set is design-rules-mandated but not all needs to land Day 15. **Recommend** ~30 fixtures for Day 15 (10 positive, 10 negative, 10 edge — smart quotes, mixed case, surrounding punctuation) — enough to verify the TS audit A1 fixes carry forward.

### OQ9. Cargo.lock policy (Day 14 L6 forward-feed)
`Cargo.lock` gitignored today; for a binary crate it should be committed. **Recommend** committing Cargo.lock + removing from `.gitignore` as the first Day 15 commit. Closes the forward-feed without scope cost.

---

## Scope concerns for Day 15-in-one-cycle

Estimated build sizes: pretrigger ~230 LOC (half-day), types ~200 LOC (quarter-day), classifier+mock ~250 LOC (half-day), attribution ~350 LOC (half-to-full day), `EngineEvent` field additions + adapter update ~50 LOC (quarter-day), `mod.rs` ~50 LOC. **Total ~1100 LOC across 5-6 files, ~2 days of focused build + half-day audit.**

Concerns:

1. **Pass 4 fallback entanglement.** Shipping `_with_fallback` Day 15 commits the closure-arg shape; Day 16 must adopt it on the classifier side. Mitigation: ship signature only; no caller until Day 16.
2. **Confidence-newtype proliferation** (three types). Mitigation: shared `Confidence` parent newtype with validation logic.
3. **Fixture YAML schema churn.** Mitigation: design fixtures around Day-15-stable inputs (text + loaded items + recent turns); orchestrator-level fixtures land separately.
4. **Day 14 stubs in storage.** Day 16 has to ship `put_if_version`/`get_with_version` AND migrate lessons AND build orchestrator. **Likely warrants a Day 16a/16b split.** Surface this in Day 15 post-research.
5. **Adapter field rename** (`cc_version` → `host_version`, `git_branch` → `project_tag`). Small mechanical change but touches host code; auditable.

**Verdict:** Day 15 fits in one cycle if we hold the locked scope and defer lessons migration. Main downstream risk is Day 16 overload; flag for 16a/16b split decision in Day 15 post-research.

---

## Sources / crate versions cited

- `regex` 1.11.1 (Oct 2024) — primary pretrigger engine. MIT/Apache.
- `aho-corasick` 1.1.3 — surveyed; not selected. MIT/Apache (Unlicense option).
- `regex-automata` 0.4.x — surveyed; subsumed by `regex`. MIT/Apache.
- `async-trait` 0.1 — already locked Day 14 D10. MIT/Apache.
- `thiserror` 2.x — already in deps. MIT/Apache.
- `chrono` 0.4 — already in deps. MIT/Apache.
- `bytes` 1.x — already in deps. MIT.
- `bitflags` 1.x / 2.x — surveyed for Hazards (Q8 S6); not selected for Day 15. MIT/Apache.
- `phf` 0.11 — surveyed for STOPWORDS (Q4); could land Day 15 but `LazyLock<HashSet<&'static str>>` is simpler. MIT.

No AGPL/GPL/SSPL dependencies recommended. All chosen crates verified MIT or MIT/Apache dual via crates.io as of 2026-05-13.

---

## Related

- [[feedback-rust-idiomatic-refactor]] — the hard rule this entire document operationalizes.
- `docs/research/sentiment-design-rules.md` — locked design rules (Audit A1/A2/A3 lineage).
- `docs/research/sentiment-engineering-2026-05-12.md` — engineering report; source for the architecture.
- `docs/research/sentiment-linguistics-2026-05-12.md` — linguistic basis for the lexicon and attribution priors.
- `docs/research/day-14-pre-research.md` — the engine-abstractions pattern Day 15 builds on.
- `docs/research/day-14-learn-notes.md` — Day 14 locked decisions.
- `docs/research/day-14-post-research.md` — L1/L2/L3 forward-feeds consumed by this document.
- `loop-archive-2026-05-13/core-ts/src/sentiment/*.ts` — TS reference (the *what*, not the *how*).
