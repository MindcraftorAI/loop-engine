# Changelog

All notable changes to `loop-engine` are documented here.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project follows [SemVer 2.0.0](https://semver.org/) starting at 1.0.

---

## [Unreleased]

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
