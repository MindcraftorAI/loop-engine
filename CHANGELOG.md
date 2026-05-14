# Changelog

All notable changes to `loop-engine` are documented here.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project follows [SemVer 2.0.0](https://semver.org/) starting at 1.0.

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
