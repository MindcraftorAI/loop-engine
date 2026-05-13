# ADR-0005: Hybrid memory retrieval — manifest at start + lazy recall on demand

**Status:** Accepted
**Date:** 2026-05-11

## Context

Two existing patterns:

- **Claude Code's model:** entire memory index loaded at session start as plain markdown. Simple, but doesn't scale past tens of memories.
- **Hermes Agent's model:** active per-turn prefetch via pluggable backends. Scalable, but heavier and runs even when nothing relevant exists.

LOOP needs to support hundreds-to-thousands of memories per scope (especially as marketplace bundles ship and tenant memory accumulates), while keeping context loads small.

## Decision

Use a **hybrid pattern**:

1. **Active manifest** in the system prompt at session start. ~20-50 entries. Each entry is `[memory_id] short_description`. Memories selected by scope filtering + pin priority + recency.
2. **Searchable corpus** outside context. All other memories live in the database but are not in context.
3. **Two retrieval modes exposed as MCP tools:**
   - `loop_recall_memory(id)` — fast path when the description told the model what it needs
   - `loop_search_memory(query, limit=5)` — semantic search fallback for unknown-unknowns

## Defense in depth against model-skips-relevant-memory

Six layers, three ship in beta:

| Layer | Beta? |
|---|---|
| Smart manifest ordering by similarity to current input | Yes |
| Auto-inject very-high-confidence matches (>0.92 sim) | Yes |
| Soft hints on disagreement (>0.75 sim, model skipped) | Post-beta |
| Lesson-driven trigger patterns (system learns its own misses) | Post-beta |
| Post-response audit (paid/opt-in) | Post-beta |
| User feedback loop ("which memories should have been used?") | Yes |

## Consequences

**Pros:**
- Token budget per session: ~2-5k manifest + 1-2k active lessons + 200-2000 per recall. <10k typical overhead. Sustainable on 200k context models.
- Pay-as-you-go context cost — only retrievals you make take token budget
- Auditable (every recall is a tool call)
- Permissions enforced at retrieval boundary
- Plays well with Claude Code's complement mode

**Cons:**
- Latency per recall (extra LLM round trip) — fine interactively, noticeable in high-throughput automation
- Description quality is load-bearing — vague descriptions = wrong retrievals
- "Don't know what you don't know" problem — mitigated by `search_memory` fallback and the defense-in-depth layers

## Memory record fields this requires

- `id`, `content`, `description` (separate high-signal short field), `embedding` (vector for semantic search), `scope`, `pin_priority`, `last_accessed_at`, `provenance`

## Alternatives considered

- **Static start-time load (Claude Code style):** doesn't scale beyond tens of memories
- **Pure per-turn prefetch (Hermes style):** runs even when irrelevant; harder to reason about which memories are visible
- **Vector-only retrieval:** loses the "model picks from descriptions" pattern that LLMs handle well

## Related

- [ARCHITECTURE.md](../ARCHITECTURE.md) — Memory section
- [DATA_MODEL.md](../DATA_MODEL.md) — Memory entity
