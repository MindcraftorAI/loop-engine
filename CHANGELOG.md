# Changelog

All notable changes to `loop-engine` are documented here.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
SemVer note: this crate is in active development at 0.x. Per
SemVer.org §4, anything MAY change at any time at 0.x — the wire
shape, the public API, the on-disk layout. 1.0 is reserved for when
the RPC surface is committed to backward-compatibility against an
external consumer.

---

## [Unreleased]

## [0.5.2] — 2026-05-17

### Changed — phase ledger goes per-task, not per-(session, task) (#166)

**Breaking RPC change.** `task.log_phase` and `task.get_ledger` no
longer accept a `session_id` field. The on-disk layout changes from
`phase_ledger/<session_id>/<task_id>/<phase>.yaml` to
`phase_ledger/<task_id>/<phase>.yaml`.

**Why:** writers (opensquid's `log_phase` MCP tool) supplied a
PID-derived MCP session id (`mcp-<pid>-<startMs36>`) while readers
(opensquid's workflow-gate hook) supplied Claude Code's session UUID
(`26e0203a-...`). The two id surfaces never matched — the ledger
was effectively unreadable across them, which made the headline
drift gate a silent no-op for the entire 2026-05-17 evening session.

**Migration:** any existing entries under
`~/.opensquid/phase_ledger/<old-session-id>/` (or `~/.loop/...` for
direct-engine consumers) are orphaned and will not be read by the
new code. They aren't deleted automatically; consumers can `rm -rf`
those subdirectories at their leisure.

**API surface impact:**
- `loop_engine::engine::phase_ledger::log_phase` — drops `session_id`
  parameter
- `loop_engine::engine::phase_ledger::get_ledger` — same
- `loop_engine::engine::storage::StorageKey::phase_log` — drops
  `session_id` parameter
- `loop_engine::engine::storage::StorageKey::phase_ledger_task_prefix`
  — same
- `task.log_phase` RPC response — drops `session_id` echo field
- `task.get_ledger` RPC response — same

**Tests:** 9 of the 10 RPC tests in `src/serve.rs` retained (params
updated to drop `session_id`). `task_get_ledger_isolates_sessions`
deleted — sessions no longer isolate by design, and a task spanning
multiple sessions (e.g. after `claude --resume`) must accumulate
phases across them. 587/587 lib tests green.

Pre-1.0, so this is a permitted breaking change per the SemVer 0.x
clause in this changelog's header. Sole consumer (opensquid) ships
its corresponding update in lockstep.

## [0.5.1] — 2026-05-16

**Patch fix: `task.get_ledger` chokes on LocalFs lock sidecars.**

End-to-end smoke test of the v0.5.0 binary surfaced an audit-missed
bug. `LocalFsStorage::list()` returns `<key>.lock` sidecar files
(advisory flocks created by the CAS layer) alongside the `<phase>.yaml`
entries. `get_ledger` then tried to YAML-parse the binary lock file
and returned an `internal: malformed entry` error.

The unit tests all used `MemoryStorage` (which has no lock sidecars),
so the audit pass didn't catch the cross-backend divergence. Fix
filters `list()` results to `.yaml` suffix before parsing.

New regression test uses `LocalFsStorage` against a `TempDir` so the
in-memory-only test surface isn't a blind spot for future ledger
changes.

First real dogfood-discovered bug of v0.5.0. Caught BEFORE any
consumer hit it because the v0.6.1 workflow gate (opensquid) is now
forcing end-to-end smoke tests on every release.

## [0.5.0] — 2026-05-16

**Phase ledger + Windows-supported binary.**

### Added

- `engine::phase_ledger` module — per-`(session, task, phase)`
  workflow ledger. Records which workflow phases (`pre_research`,
  `learn`, `code`, `test`, `audit`, `post_research`, `fix`) have been
  logged for a given task. Consumers use this to gate downstream
  operations on phase coverage (e.g. block `git commit` if `audit`
  hasn't been logged for the active task).
  - Storage layout: `phase_ledger/<session>/<task>/<phase>.yaml`
    (one file per phase entry, idempotent re-log via create-only
    `put_if_version`). Mirrors the per-session signal store pattern.
  - Substrate-pure: engine has no `Task` type. `task_id` is an opaque
    string discriminator from the caller.
  - Input validation: `session_id`, `task_id` validated against
    `[A-Za-z0-9_-]{1,128}` BEFORE constructing storage keys
    (defense-in-depth; the `StorageKey::from_raw` hard-assert never
    fires from valid RPC). `note` capped at 16 KB.
- `task.log_phase` RPC — `{session_id, task_id, phase, note?}` →
  `{ok, newly_recorded, ...}`. `newly_recorded: false` on idempotent
  re-log.
- `task.get_ledger` RPC — `{session_id, task_id}` → `{phases_logged,
  entries}`. Returns entries sorted by `logged_at` (deterministic
  chronological order regardless of storage backend).
- `StorageKey::phase_log` + `StorageKey::phase_ledger_task_prefix`
  key constructors. Trailing slash on the prefix is load-bearing
  (without it, MemoryStorage's `starts_with` prefix match would
  collide with sibling `task_id`s; LocalFs is unaffected but
  cross-backend divergence is a bug regardless).

### Audit-driven fixes (caught pre-commit)

- HIGH: prefix-list collision between sibling `task_id`s in
  MemoryStorage. Fixed via trailing slash + dedicated isolation test.
- MED: `get_ledger` ordering was backend-dependent. Now sorts by
  `logged_at` server-side.
- MED: `note` length DoS vector. Capped at 16 KB with `InvalidParams`
  rejection.

### Coverage

- 9 `phase_ledger` module unit tests (phase round-trip, validate_id
  boundaries, YAML render/parse round-trip including escape chars).
- 10 `serve.rs` RPC handler tests (record + readback, idempotent
  re-log, unknown phase, path traversal, sibling isolation, cross-
  session isolation, sort-by-logged_at, oversized note rejection).
- Full suite: 587 tests passing (584 prior + 3 phase ledger module
  net adds beyond the unit tests above).

### CI

- `windows-check` job scope narrowed from `--all-targets` to
  `--lib --bin loop-engine`. Integration tests under `tests/` use
  Unix-only tokio + filesystem features by design; the release.yml
  matrix only ships the lib + bin, so that's what we verify.

## [0.4.0] — 2026-05-16

**Version reset: 1.x → 0.4.0.**

Earlier commits (`69fa253 v1.1` through `3ba7e41 v1.4`) used
commit-message labels in the `vX.Y` shape but never bumped
`Cargo.toml` or cut releases. The crate was stuck at `1.1.0` while
the labels claimed v1.4. The first reflexive fix bumped Cargo.toml
to `1.4.0` to match the labels — but the deeper problem was that
the crate had been at 1.x since before it should have been. No
external consumer, no committed API stability, RPC surface still
being expanded weekly (5 new methods this week alone). 1.0 was
premature.

This release demotes the crate to **0.4.0** to reflect actual
maturity: pre-1.0, public API allowed to change, version bumps
allowed to be additive without ceremony. Re-cross 1.0 only when
there's an external consumer asking for stability + a commitment to
non-breaking changes for a defined window.

Also removes the strict `public-api stability gate` from CI. The
gate diffed against `public-api-v1.0.txt`; every additive RPC failed
it (CI red since 2026-05-13's external_id upsert commit). At 0.x the
gate is the wrong tool. Re-add it (with a fresh baseline) when
crossing 1.0 for real.

The actual feature content stays — all the work from `69fa253`
through `c9e5b0d` is in `main`. Only the version label changes. The
inventory below documents what shipped under the `v1.x` labels;
treat them as pre-1.0 milestones now.

Workflow polish included: tar.gz packaging step now runs `chmod +x`
before archiving (so the executable bit survives the tarball
roundtrip) and generates the sha256 sidecar from inside `dist/` (so
the embedded path is bare `<file>.tar.gz` — lets `shasum -c` work
cleanly from the dist dir).

### Added — v1.1 Pack-authored lesson seeding

- **`Authorship::Pack` variant** — new non-exhaustive enum variant for
  trusted-seeded lessons. Wire format: `"pack"`. Treated as user-equivalent
  for eviction-immunity invariants (see `Authorship::is_immune`).
- **`Authorship::is_immune(self) -> bool`** — new predicate. Returns true
  for `User` AND `Pack`. The 9 production sites that previously gated on
  `.is_user()` now gate on `.is_immune()` — semantic intent at those sites
  is "is this immune from eviction?" not "literally user-authored."
- **`LessonFrontmatter.pack_id: Option<String>`** — new optional field. Set
  when `authored_by = Pack`, carries the codex id (e.g.
  `"fullstack-react-atomic"`) for symmetric bulk-retirement on codex
  uninstall. Skipped from YAML when `None` (back-compat with v0.5 lessons).
- **`lesson.create` RPC** — accepts new `authored_by: "pack"` discriminator
  + required `pack_id: <string>` companion field. Response includes
  `pack_id` when present. Pre-v1.1 callers unaffected.

The lesson promotion path (wedge gate) is unchanged structurally — Pack
provides bypass *by design contract* (consumer creates Pack-authored
lessons directly in promoted state via lesson lifecycle, not by tricking
the gate). The wedge invariant holds: only `Authorship::Llm` / `Agent`
lessons run through `gate::check_promotion_gate`.

Engine v1.1 codex support is intentionally minimal — codex YAML format,
storage, activation, export, 3-way merge, AI-mediated import, doc-fetch,
verify-gates all live in the opensquid consumer. See
`~/projects/loop/docs/engine-v1.1-substrate-design.md` for the full design.

### Added — v0.5 hybrid recall

- **`engine::scoring`** module: shared `score_text_match(query, description, body)` helper
  with 2x description weighting. Promoted from `serve.rs`'s private `score()` so
  lesson_recall + the new memory text path share one authoritative scorer.

- **`memory::text_search`** — linear-scan token+substring scoring across all
  memories. Companion to `memory::search` (semantic). Soft-fails on parse
  errors like `search`. Respects `MemoryScopeFilter`.

- **`memory::hybrid_search`** — runs `search` + `text_search`, RRF-merges by
  `MemoryId` (k=60, Cormack 2009). Same-id collisions accumulate scores and
  flip `source` to `HitSource::Both` (strongest signal). `min_similarity`
  applied to RAW per-source scores BEFORE RRF — the threshold can't share a
  scale with RRF scores so it's enforced upstream.

- **`HitSource` enum** + optional `source: Option<HitSource>` field on
  `MemoryRef`. Pre-v0.5 refs have `source = None`; v0.5+ paths stamp the
  originating source. JSON-serialized via snake_case, skipped (key omitted)
  when None.

- **`memory.search` RPC**: new `mode: "semantic" | "text" | "hybrid"` param
  (default Semantic for back-compat) + `min_similarity: number` param
  (default 0.0). Wire shape is additive — v0.4 callers see identical output
  with no extra fields beyond optional `source` on hit objects.

Solves the v0.4 "Gianna" false-negative: a proper-noun query whose semantic
similarity (0.486) sat below the 0.5 default threshold despite the memory's
description literally containing the name. The text path catches it now
(substring score 1.0); RRF surfaces it cleanly.

### Added

- **`MemoryOrigin`** + optional `origin` block on `MemoryFrontmatter`
  (`engine::memory::origin`): provenance metadata — host, session_id,
  model, cwd_basename, written_at. All fields `Option<String>` so
  partial-detect hosts round-trip cleanly. v0.3.1 memories without
  the block load as `origin: None` via `#[serde(default)]`.

- **`insert_with_provenance()`** (`engine::memory::store`): the
  deepest write-path, taking both scope + optional origin.
  `insert_scoped` now delegates to it with `origin = None` so
  existing v0.3.1 call sites are zero-impact.

- **`update()`** (`engine::memory::store`): mutate description /
  content / scope on an existing memory. Identity (`id`, `created_at`,
  `consumed_by_user_lessons`, `derived_from`, `origin`) is always
  preserved. Re-embeds + replaces the vector index entry on content
  change; description/scope-only edits skip the embed path. Returns
  `Ok(None)` if the id doesn't exist; user-immunity invariant does
  not fire (citation chain unchanged by edits).

- **`memory.create` RPC** accepts optional `origin` (mirrors
  `MemoryOrigin` serde). **`memory.get` RPC** response now includes
  `origin` (null for pre-v0.4 memories).

- **`memory.update` RPC** — wraps `store::update`. Requires at least
  one of `description` / `content` / `scope`. Returns the updated
  frontmatter shape (`updated_at` reflects the edit timestamp).

- **`memory.delete` RPC** — the user-facing `forget` operation. Wraps
  the existing `store::delete`. Default `force = false` respects
  user-immunity (returns the new `DispatchError::UserMemoryImmune` →
  RPC error code `-32003`). Force=true bypass is the user-initiated
  override.

The wedge gate's v0.4+ `origin_diverse` signal (multi-session
reproducibility = harder to fake) consumes the origin fields. The
engine ships the storage half; the gate-side counting lands in a
later v0.4 cycle.

---

## [1.0.0] — 2026-05-14

First stable release. Engine surface is committed to SemVer; sealed
trait implementations are internal but additive growth is non-breaking
via `#[non_exhaustive]` on all public types.

### Added

- **Manifest assembly** (`engine::manifest`): structured context bundle
  surfaced to host LLMs. Sections: `active_lessons`, `memories`,
  `active_skills/personas/teams`, `assembly_stats`. Wedge `GateDecision`
  attached to every active lesson; no separate gate-check round trip.

- **Promotion gate** (`engine::lessons::gate`): the anti-self-grading
  wedge. `check_promotion_gate(fm, metadata, config, now)` is pure and
  side-effect-free, returning `GateDecision::{Promote, Block}` with
  typed `BlockReason` enum. Blocks tampered age, missing narrative,
  empty evidence, thumbs-down, time-floor violations.

- **Causal narrative generation** (`engine::lessons::narrative`): LLM-
  generated structured `(trigger, failure_mode, correction, confidence,
  evidence_refs)` tuples. Refusal-on-thin-input surfaces as
  `EngineError::NarrativeInsufficientContext`.

- **Memory store** (`engine::memory`): YAML-frontmatter memories with
  vector-embedding sidecars, semantic search via `VectorIndex` trait,
  `MemoryScope::{User, Team, Skill, Project, Global}`, and the
  `consumed_by_user_lessons` immunity counter — the wedge's persistence
  layer.

- **Memory compression** (`engine::memory::compress`): condense a window
  of raw memories into a summarized memory while preserving citation
  chains via `derived_from`. `get_by_id_chasing_derived_from` walks the
  chain forward; `recompute_citation_counts` repairs drift.

- **Skills + personas + teams** (`engine::skills`, `engine::personas`,
  `engine::teams`): Claude-Skills-parity skill model with typed hooks
  enum, persona identity descriptors, team groupings. User-authored
  entries are eviction-immune from engine-initiated archive/delete.
  Skills cite memories via `EvidenceRef::Memory(_)` — the cross-cutting
  wedge.

- **Lesson lifecycle transitions** (`engine::lessons::transitions`):
  `promote`, `discard`, `supersede`, `capture_feedback`. CAS-RMW with
  5-retry budget; idempotent move helper survives crash replay;
  user-authored discard/supersede decrements cited memories'
  immunity counters (the wedge symmetry).

- **Sentiment classifier + orchestrator** (`engine::sentiment`):
  pretrigger gate + classifier trait + attribution + signal writer.

- **Storage trait** (`engine::storage`) with `MemoryStorage` (in-memory,
  used by tests + adapters) and `LocalFsStorage` (filesystem backend
  with CAS-RMW via `put_if_version`).

- **JSONL watcher** (`host::claude_code`): Claude Code adapter that
  parses session transcripts and emits engine events.

- **`serve` subcommand — JSON-RPC over stdio**: `loop-engine serve`
  exposes the engine as a programmatic RPC endpoint for host
  adapters (opensquid MCP server, future TS/Python launchers). Line-
  delimited JSON-RPC 2.0 on stdin/stdout; stderr stays free for
  diagnostics. v1 methods:
  - `ping`
  - `lesson.create`, `lesson.recall`, `lesson.promote`, `lesson.discard`
  - `memory.create` (optional `scope` param → `MemoryScope`-aware insert),
    `memory.search` (`include_body: bool` returns FULL content;
    `scope_filter` restricts results by `MemoryScope`), `memory.get`
    (fetch a memory by id with full content + scope tag)

- **`OpenAiCompatibleEmbedder`** (`engine::embedding`): production
  embedder over the OpenAI-compatible `/v1/embeddings` API surface.
  Drop-in for Ollama (`qwen3-embedding:4b`, 2560 dims, the dogfood
  default) or any provider matching that interface (Voyage, etc).
  Configured via env (`OPENSQUID_EMBEDDER_URL/MODEL/DIMENSIONS/API_KEY`)
  or constructed directly.

- **Scope-aware semantic search**: `memory::search()` accepts an
  optional `&MemoryScopeFilter`, filtering hits in-loop against
  the loaded frontmatter. Zero extra disk roundtrip (`get_by_id`
  already runs per hit). The Phase F manifest assembly path
  continues to use the over-fetch-then-post-filter pattern;
  scope-filtered `search()` is the new direct-caller path for
  serve-mode hosts.

- **Vector index rehydration on serve startup**
  (`memory::rehydrate_vector_index`): the HNSW index is in-process
  memory; persisted `.md` + `.vec` pairs survive but the index
  doesn't. `loop-engine serve` now scans `memories/` on startup and
  rebuilds the index from disk. This enables cross-session memory
  recall — Claude Code, Claude Desktop, and IDE-plugin hosts all
  spawn their own engine subprocess against the shared
  `~/.opensquid/` store, and each spawn now starts with all
  persisted memories already searchable.

### Wedge defense

Four-layer ratchet (B/E2/F/G) defended end-to-end by:
- `gate::tests::*` (26 promotion-gate scenarios)
- `tests/compression_wedge_e2e.rs` (7 compression-chain tests)
- `tests/skill_wedge_e2e.rs` (3 skill-immunity tests)
- `transitions::tests::*_decrements_memory_citations` (2 lifecycle-
  decrement tests)

### Notes

- 534 lib tests + 17 integration tests; clippy clean under `-D warnings`.
- Zero AGPL/GPL/SSPL dependencies.
- `publish = false` in `Cargo.toml` until the standalone repo split.
- Body audit-line format (`<!-- ... -->`) and `lesson-history.yaml`
  sidecar are flagged UNSTABLE in v1.0 — may graduate to typed events
  in v1.1.

[1.0.0]: https://github.com/MindcraftorAI/loop-engine/releases/tag/v1.0.0
