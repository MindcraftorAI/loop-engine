# Competitive Landscape (verified 2026-05-12)

This document was overhauled on 2026-05-12 after a verified competitive audit revealed that the prior framing was based on outdated assumptions. The original document's positioning ("nobody fuses memory + skill auto-update") is no longer accurate as of May 2026.

## What changed between Feb–May 2026

Three things shipped in the last 60 days that closed the original wedge:

1. **Anthropic Dreaming** (2026-05-06, Code with Claude SF)
   - Scheduled background process that reviews agent sessions + memory, extracts recurring patterns, curates memory, writes plain-text notes + structured playbooks
   - Claude Managed Agents only — closed platform via request-access form
   - Reported ~6x task completion uplift at Harvey
   - Sources: [claude.com blog](https://claude.com/blog/new-in-claude-managed-agents), [VentureBeat](https://venturebeat.com/technology/anthropic-introduces-dreaming-a-system-that-lets-ai-agents-learn-from-their-own-mistakes)

2. **Claude Code Auto Memory + Auto Dream** (built-in, GA, on by default)
   - Auto Memory: heuristic capture during normal work, stores at `~/.claude/projects/<project>/memory/*.md`. Toggle via `/memory`
   - Auto Dream: 4-phase consolidation (Orient → Gather Signal → Consolidate → Refresh), reads JSONL session transcripts, prunes stale, resolves contradictions. Lockfile-protected, read-only on project code during dream
   - Sources: [claudefa.st auto-memory](https://claudefa.st/blog/guide/mechanics/auto-memory), [claudefa.st auto-dream](https://claudefa.st/blog/guide/mechanics/auto-dream)

3. **`affaan-m/everything-claude-code`** (OSS, MIT)
   - 140K+ stars, 21K forks, v2.0.0-rc.1 (April 2026)
   - 225+ skills, "instincts" with auto-pattern extraction + confidence scoring, `/evolve` cluster-into-skills, hooks-based session memory persistence, AgentShield security
   - Cross-host: Claude Code, Cursor, OpenCode, Codex, Antigravity
   - **No explicit lesson layer, no causal narrative, no anti-self-grading mechanism**
   - Source: [github.com/affaan-m/everything-claude-code](https://github.com/affaan-m/everything-claude-code)

Adjacent patterns also shipped or matured in the same window:
- **`learnings.md` pattern** — community freeform markdown, no gate, no structured narrative ([MindStudio guide](https://www.mindstudio.ai/blog/self-learning-claude-code-skill-learnings-md))
- **Karpathy AutoResearch + universal-skill adaptation** — eval-driven prompt mutation, blind-judge isolation (one form of anti-charitable grading) ([Medium](https://medium.com/@k.balu124/i-turned-andrej-karpathys-autoresearch-into-a-universal-skill-1cb3d44fc669))
- **Earlier competitors still relevant:** Mem0 ($24M raised), Letta/MemGPT, Zep + Graphiti, Hermes Agent (Nous Research)

## Feature comparison (verified)

| Feature | Loop | Anthropic Dreaming | CC Auto Memory | CC Auto Dream | everything-claude-code | learnings.md | AutoResearch-skill | Mem0 | Hermes |
|---|---|---|---|---|---|---|---|---|---|
| Pattern extraction across sessions | ✓ | ✓ | partial | ✓ | ✓ (instincts) | ✗ | ✗ | ✓ | ✓ |
| Memory consolidation / prune | partial | ✓ | ✗ | ✓ | partial | manual | ✗ | ✓ | partial |
| File-canonical YAML/MD | ✓ | unverified | ✓ MD | ✓ MD | ✓ | ✓ | ✓ | ✗ (DB) | ? |
| Runs locally | ✓ | ✗ (cloud-only) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ option | ✓ |
| MIT / OSS | ✓ | ✗ | ✗ (closed feature) | ✗ (closed feature) | ✓ | ✓ pattern | ✓ | ✓ Apache | ✓ MIT |
| Multi-host (Codex/Cursor/OpenCode) | ✗ | ✗ | ✗ | ✗ | ✓ | n/a | ✓ (CC+Cursor) | n/a | ✗ |
| Vector / RRF retrieval | ✓ (sqlite-vec+FTS5) | unverified | ✗ | ✗ | ✗ | ✗ | ✗ | ✓ | partial |
| **Structured causal narrative** | ✓ | ✗ | ✗ | ✗ | ✗ | partial | ✗ | ✗ | ✗ |
| **Anti-self-grading promotion gate** | ✓ | ✗ | ✗ | ✗ | ✗ | ✗ | ✓ (blind judge) | ✗ | ✗ |
| **Tamper-proof age (filesystem birthtime)** | ✓ | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ |
| External thumbs-up/down signal | ✓ | unverified | ✗ | ✗ | ✗ | ✗ | partial (evals) | ✗ | ✗ |
| Eval-driven mutation loop | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | ✓ | ✗ | partial |
| MCP-native | ✓ | ✗ | ✗ | ✗ | partial | ✗ | ✗ | partial | ✗ |
| Distribution / mindshare | 0 | Anthropic megaphone | default-on in CC | default-on in CC | 140K stars | viral pattern | growing | $24M raised | open weights ecosystem |

## What Loop uniquely has

Three items not matched by any verified competitor:

1. **Anti-self-grading promotion gate with tamper-proof birthtime + evidence_refs requirement + external signal required + thumbs_down hard-block** — nobody else has this combination. AutoResearch-skill's blind judge is the closest single piece; everyone else trusts the model to grade itself.

2. **Structured CausalNarrative** with confidence ladder (observed / inferred / speculative) and required evidence_refs for non-speculative claims. Everyone else is freeform markdown or no narrative at all.

3. **sqlite-vec + FTS5 + RRF + 3-axis scoring + auto-inject high-confidence** on top of MCP-native. Auto Memory and Auto Dream are pure text/file — no vector retrieval.

## What Loop uniquely DOES NOT have

1. **Distribution** — Anthropic megaphone (Dreaming), default-on in Claude Code (Auto Memory / Auto Dream), 140K stars (everything-claude-code). Loop has zero external users.
2. **Multi-host support** — everything-claude-code and dream-skill both ship Cursor / Codex / OpenCode. Loop is Claude Code only.
3. **Eval-driven mutation loop** (the Karpathy primitive — actual self-improvement). Loop's promotion is downstream of capture; doesn't generate variations.
4. **JSONL session-transcript ingestion** — Auto Dream pulls signal from `~/.claude` transcripts; Loop has no equivalent ingestion path.
5. **Hook-based auto-capture during normal work** — Auto Memory and everything-claude-code both have it. Loop requires explicit MCP tool calls today.

## Strategic position (committed 2026-05-12)

**Narrow to verification.** Loop is a verifier that sits on top of any capture mechanism (Anthropic Dreaming, Claude Code Auto Memory, learnings.md, everything-claude-code). It runs candidates through the promotion gate and accepts / rejects with reasons.

This is the only piece of the original positioning that survives the 2026-05-12 audit. See [BETA_SCOPE.md](BETA_SCOPE.md) for the resulting committed roadmap.

## Sources (verified)

- [claude.com — New in Claude Managed Agents](https://claude.com/blog/new-in-claude-managed-agents)
- [VentureBeat — Anthropic Dreaming](https://venturebeat.com/technology/anthropic-introduces-dreaming-a-system-that-lets-ai-agents-learn-from-their-own-mistakes)
- [SiliconANGLE — Dreaming](https://siliconangle.com/2026/05/06/anthropic-letting-claude-agents-dream-dont-sleep-job/)
- [github.com/affaan-m/everything-claude-code](https://github.com/affaan-m/everything-claude-code)
- [claudefa.st — Auto Memory](https://claudefa.st/blog/guide/mechanics/auto-memory)
- [claudefa.st — Auto Dream](https://claudefa.st/blog/guide/mechanics/auto-dream)
- [MindStudio — Auto Memory](https://www.mindstudio.ai/blog/what-is-claude-code-auto-memory)
- [MindStudio — learnings.md](https://www.mindstudio.ai/blog/self-learning-claude-code-skill-learnings-md)
- [Medium — AutoResearch universal skill](https://medium.com/@k.balu124/i-turned-andrej-karpathys-autoresearch-into-a-universal-skill-1cb3d44fc669)
- [github.com/karpathy/autoresearch](https://github.com/karpathy/autoresearch)
- [github.com/grandamenium/dream-skill](https://github.com/grandamenium/dream-skill)
- [anthropics/claude-code issue #38461](https://github.com/anthropics/claude-code/issues/38461)
