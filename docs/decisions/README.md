# Decision Log

Architectural Decision Records (ADRs) for LOOP. Each ADR captures one significant decision: the context, the choice, the consequences, and the alternatives rejected.

When a future decision contradicts a prior one, **don't edit the old ADR** — write a new one and mark the old as `Superseded by ADR-NNNN`.

## ADRs

| # | Title | Status |
|---|---|---|
| [0001](0001-multi-agent-orchestration-day-one.md) | Multi-agent orchestration is day-one, not v1.x | Accepted |
| [0002](0002-loop-complements-claude-code.md) | LOOP complements Claude Code's existing skill/memory systems | Accepted |
| [0003](0003-two-tier-free-self-hosted-paid-saas.md) | Two-tier product — free self-hosted + paid hosted SaaS | Accepted |
| [0004](0004-language-nodejs-typescript.md) | Node.js / TypeScript as the implementation language | Accepted |
| [0005](0005-hybrid-memory-retrieval.md) | Hybrid memory retrieval — manifest + lazy recall | Accepted |
| [0006](0006-lesson-model-not-counter-based.md) | Lesson model uses human-learning patterns, not counters | Accepted |
| [0007](0007-mcp-first-substrate-not-runtime.md) | LOOP is an MCP-first substrate, not its own runtime | Accepted |
| [0008](0008-closed-model-first.md) | Closed-model-first stance, with open-weight as non-special-cased | Accepted |
| [0009](0009-open-core-licensing.md) | Open Core licensing — MIT engine + proprietary platform | Accepted |
| [0010](0010-on-disk-file-layout.md) | On-disk file layout — files canonical, DB as derived index | Accepted |
| [0011](0011-dual-mcp-role-server-and-client.md) | LOOP is MCP server AND client (dual role) | Accepted |
| [0012](0012-claude-as-orchestrator.md) | Claude orchestrates, LOOP scaffolds and learns | Accepted |
| [0013](0013-persona-team-session-activation.md) | Persona × Team separation, session-activation loading | Accepted |

## Format

Each ADR follows:

```
# ADR-NNNN: Title

**Status:** Accepted | Open | Superseded by ADR-XXXX
**Date:** YYYY-MM-DD

## Context
What problem are we solving? What constraints existed?

## Decision
What did we decide?

## Consequences
What follows — pros and cons.

## Alternatives considered
What other options we evaluated and why we rejected them.

## Related
Links to other ADRs and docs that touch this decision.
```
