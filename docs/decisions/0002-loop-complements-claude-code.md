# ADR-0002: LOOP complements Claude Code's existing skill/memory systems

**Status:** Accepted
**Date:** 2026-05-09

## Context

LOOP is dogfooded inside Claude Code during development. Claude Code already has its own skill system (`.claude/skills/`) and an auto-memory system. Three options existed for how LOOP relates to them:

- **Replace:** LOOP takes over both — cleanest from LOOP's perspective, most disruptive for users
- **Complement:** LOOP runs alongside via MCP, exposing additional skills/memory — lowest friction, two systems mentally coexist
- **Wrap:** LOOP reads/writes Claude Code's existing files plus augments with auto-update — best UX, hardest to implement cleanly

## Decision

LOOP **complements** Claude Code's existing systems. Both run in parallel; LOOP is exposed entirely through its MCP server as additional tools and resources. Wrap mode may revisit in v1.x once both systems are stable.

## Consequences

**Pros:**
- Lowest implementation risk (LOOP doesn't touch Claude Code's files)
- Users keep working with Claude Code's native systems as-is
- One clean integration surface: MCP. No file-system entanglement.
- Namespace isolation: LOOP lives entirely under `~/.loop/` (skills, memory, lessons, bundles, db, logs); Claude Code keeps `.claude/skills/` and its own memory dir. See [ADR-0010](0010-on-disk-file-layout.md) for the full on-disk layout.

**Cons:**
- **Feedback signal capture is harder.** LOOP can't passively observe user behavior (kept output, edited, rejected) because it doesn't sit between user and Claude Code. Signals must come from MCP-call patterns (implicit), dedicated MCP tools (explicit), or inferred follow-up sequences.
- Two systems users must mentally track
- LOOP can't auto-enhance content already in `.claude/skills/` — only its own skills

## Mitigation

Feedback signal design must lean explicit and infer from MCP traffic. The defense-in-depth retrieval mitigation (see [ARCHITECTURE.md](../ARCHITECTURE.md)) is partly compensating for this weakness.

## Scope

This decision applies only to the Claude Code dogfooding context. For other consumers:
- **RankLabs** embeds LOOP directly — complement question doesn't apply
- **Other end-user MCP hosts** (Claude.ai, Cursor) — complement applies similarly

## Related

- [ARCHITECTURE.md](../ARCHITECTURE.md) — MCP tier section
