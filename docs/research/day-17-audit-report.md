# Day 17 Audit Report

**Cycle:** Day 17 — minimum-viable scope (solicitor + tripwire + e2e test)
**Audit window:** `d7b75f3..HEAD` (single build commit `9050b8a`)
**Phase:** 5 (cycle-close audit)
**Build deliverables actually shipped:** D1-D6 (solicitor pure function + `HostVersionPolicy` tripwire + e2e integration test). D7 deferrals (lessons migration, TestHarness, signal aggregation, main.rs wiring, scenario (b)) explicitly out of scope.

## CRITICAL findings

_None._

## MAJOR findings

### M1. `max_candidates_per_call = 0` returns one candidate, not zero
`solicitor.rs:174-176` — the bound check is `if output.stale_candidates.len() >= config.max_candidates_per_call { break; }` placed AFTER the push. For the configured value `0`, the first stale candidate is still pushed before the check breaks the loop, so the output contains one candidate instead of zero. This violates the documented "hard bound" semantics (D2 says "max per call"; `0` should mean "disabled").

Fix is a one-line swap: check the bound BEFORE pushing (re-order the push and the `if … break;`), or equivalently check `>=` before the conditional push site. The 5-candidate test (`max_candidates_per_call_bounds_output`) doesn't catch this — it sets the value to 3, not 0.

Severity bumped to MAJOR because user-facing semantics deviate from documentation; cheap fix.

## MINOR findings

### m1. `lesson_status_prefix` is a bare string prefix, not a directory prefix
`storage/key.rs:55-58` returns `lessons/<status>` with no trailing slash. `MemoryStorage::list` (and the parallel filesystem impl) does a `str::starts_with` match. Today's `LessonStatus` set (`pending|active|promoted|discarded|superseded`) has no overlapping prefixes, so this is currently safe. A future status whose name extends an existing one (e.g. `"active-pinned"`) would be incorrectly scanned by an `"active"` solicitor pass. Defensive fix: produce the prefix as `lessons/<status>/` (with trailing slash). Not blocking — call this out and revisit when a new status is added.

### m2. Solicitor `window_days` from learn-notes D2 silently dropped
Learn-notes D2 specs `window_days: 14 — only signals within this window count toward density`. The shipped `SolicitorConfig` has no `window_days` field, and signal density is `external_signal_sources.len()` over all-time. This is actually unavoidable with the current data shape — `LessonFrontmatter::external_signal_sources: Vec<String>` carries no per-signal timestamps, so windowing is structurally impossible without a schema bump. The build commit message accurately documents the flat proxy. The drift is between learn-notes and reality, not between commit and learn-notes. Mark this in post-research and either (a) backfill timestamps to `external_signal_sources` (probably as a parallel structured field) before re-introducing `window_days`, or (b) drop the windowing language from future plans.

### m3. `tripwire_off_by_default_no_abstain` asserts only the negative
`orchestrator/mod.rs:623-633` verifies that the abstentions list contains no `UntestedHostVersion`. It does NOT explicitly assert that a Mock-classifier abstention is what we get instead, nor that the classifier was actually called. A regression that returns `OrchestratorOutput::empty()` early (e.g. someone "optimizes" the off path) would pass this test. Adding `assert_eq!(classifier.call_count(), 1)` would tighten it; cheap.

### m4. Lex-comparison semver gotcha is documented in code AND the commit message, but only one test scenario exercises a near-boundary version
The documented caveat — `"2.1.139"` < `"2.1.40"` lex-wise — is acknowledged in `config.rs:51-54`. The in-range test uses `"2.1.139"` against `"2.0.0"..="2.1.999"`, which happens to be correct lex-wise. A test pair that demonstrates the documented WRONG behavior (e.g. `"2.0.10"` lex-comparing wrong against `"2.0.9"`) is not present. Not a defect (the code matches the spec), but the test surface doesn't capture the boundary the docs warn about. Day 18 should add a `// known-broken: lex comparison fails for ...` test marked `#[ignore]` so the limitation is executable, not just textual.

## Verified clean

- **S44 (pure function, no executor ownership)**: `solicit_stale_lessons` takes `&Context, &dyn Storage, &SolicitorConfig, DateTime<Utc>` and returns `Result<SolicitorOutput, EngineError>`. No `tokio::spawn`, no `Interval`, no `CancellationToken`. Host owns cadence per D1. ✓
- **S46 (typed StaleReason)**: `StaleReason` is an enum (`NoSignalsInWindow | BelowDensityThreshold`), `#[non_exhaustive]`. ✓
- **S51 (tripwire fires in orchestrator, before classifier call)**: `orchestrator/mod.rs:166-193` runs the tripwire BEFORE critical-section-1 (push_turn / state mutation) and BEFORE the classifier `.classify()` call at line 220. No LLM latency paid on an out-of-range host; no state pollution on abstain. ✓
- **S52 (HostVersionAction is enum)**: `enum HostVersionAction { Warn, Abstain }`, `#[non_exhaustive]`. Not a bool. ✓
- **No `anyhow::Error` leakage**: `grep anyhow` across new Day 17 surfaces returns empty. Solicitor returns `EngineError`; the `parse_lesson_frontmatter`-via-anyhow error is swallowed into the `skipped_count`. ✓
- **Default policy = `off()` produces no false positives**: `HostVersionPolicy::is_out_of_range` short-circuits to `false` when `tested_range = None`. Existing tests pass. ✓
- **Tripwire abstain leaves no session state behind**: the `return` at `mod.rs:186-189` is BEFORE any `entry.or_default()` / `push_turn` / rate-limit mutation. ✓
- **Integration test would catch signal-emit regression**: `tests/orchestrator_e2e.rs` asserts `out.signals.len() == 1` AND `writer.captured().len() == 1` AND `classifier.call_count() == 1`. Any orchestrator regression that drops the signal, the classifier call, OR the writer record would flip this test red. ✓
- **`update_manifest` usage**: the e2e test calls `update_manifest` with a `LoadedItem` whose `keywords` ("quokka-special") matches the user-turn text, so attribution succeeds. Day 16a's path is exercised end-to-end. ✓
- **Skipped-count counts ALL parse failures**: non-`.md` keys, absent body (race), invalid UTF-8, frontmatter split failure, frontmatter parse failure, and `created_at` parse failure all increment `skipped_count`. ✓
- **No overflow on negative duration**: `age.num_days().max(0) as u64` handles future-dated `created_at` safely (rare but not undefined). ✓
- **File-size discipline**: `solicitor.rs` 343 lines (~165 prod). `orchestrator/config.rs` 80 lines. `orchestrator/mod.rs` 707 lines (~390 prod after subtracting inline tests). All under the 500-prod-LOC ceiling per audit checklist. ✓
- **Module hygiene**: `solicitor` declared in `sentiment::mod.rs:20`, re-exports at `mod.rs:34-36` (function + 4 types). `HostVersionPolicy` / `HostVersionAction` re-exported via `orchestrator::pub use config::…` and the public `sentiment::mod.rs` re-export. Clean. ✓
- **Clippy clean both default + `--features test-fixtures`**. ✓
- **249 unit + integration tests pass**. ✓

## Day 17 deferrals (carry to post-adapter-discussion)

Per Day 17 D7 (explicit in learn-notes and commit message):

- Lessons module migration to async (loader + signals to `&Context, &dyn Storage` API)
- `TestHarness` in `engine::test_support` + `ENV_LOCK` retirement
- Signal-array aggregation in `StorageBackedSignalWriter` (lesson YAML append)
- `main.rs` orchestrator stub wiring
- Scenario (b) integration test (`JsonlWatcherSource → Orchestrator` end-to-end)
- Sync-wrapper retirement (Day 16b carryover)

These are NOT Day 17 failures. The user explicitly scope-tightened to "ship the namesake + minimum verification" before the adapter-design discussion.

## New findings carried forward to next cycle

- M1: fix `max_candidates_per_call = 0` boundary (one-line re-order + a test).
- m1: switch `lesson_status_prefix` to include a trailing `/` (defensive against future status names).
- m2: reconcile learn-notes D2 `window_days` with the actual `external_signal_sources` shape — either drop the windowing language or expand the schema.
- m3: tighten `tripwire_off_by_default_no_abstain` with a `classifier.call_count()` assertion.
- m4: add an `#[ignore]`-marked executable test that demonstrates the lex-comparison semver gotcha (so the documented limitation is visible from `cargo test --include-ignored`).

## TL;DR

Day 17 is small, tight, and exactly the minimum the user scope-tightened to. The pure-function solicitor, the orchestrator-layer tripwire fired BEFORE classifier and BEFORE state mutation, the enum-typed `StaleReason` / `HostVersionAction`, and the `#[non_exhaustive]` discipline all match the locked decisions and the relevant pre-research smells (S44/S46/S51/S52) audit clean. One MAJOR finding: `max_candidates_per_call = 0` returns one candidate instead of zero — off-by-one in the break-after-push ordering, missed by the existing test that uses `max=3`. Four MINOR findings (prefix-without-slash, dropped `window_days`, weak negative-only assertion in one tripwire test, no executable lex-comparison test). No `anyhow` leakage, no executor ownership, 249 unit + 4 integration tests pass, clippy clean. Recommend M1 fix in the post-adapter-discussion cycle alongside the documented deferrals; the rest are minor.
