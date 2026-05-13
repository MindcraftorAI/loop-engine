# ADR-0007: LOOP is an MCP-first substrate, not its own runtime

**Status:** Accepted
**Date:** 2026-05-09

## Context

Two product shapes exist in the agent-infrastructure space:

- **Own runtime** — Hermes Agent, AutoGen, CrewAI. Users install the agent platform and run it. The platform owns the user's interface (TUI, web UI, CLI).
- **Substrate** — Mem0, LangChain. Provides services to other tools via API/MCP. Users keep their existing interface.

LOOP must pick a shape. The choice affects every architectural decision: dependency direction, API surface, deployment model, what "ship" means.

## Decision

LOOP is a **substrate**, not a runtime. The primary distribution surface is the LOOP MCP server, which slots into existing LLM hosts (Claude Code, Claude.ai, Cursor, ChatGPT) via MCP. There is no LOOP TUI, no LOOP chat UI, no LOOP user-facing runtime in beta.

## Reasoning

- **User population is already in existing tools.** Asking developers to switch from Claude Code or Cursor to "the LOOP runtime" is a hard ask with no clear payoff.
- **MCP is mature enough to be a real distribution surface.** Claude Code supports MCP natively, Claude.ai supports it via custom connectors, Cursor and Windsurf support it.
- **Substrate positioning is broader.** Runtimes serve users; substrates serve users + builders + agent systems. LOOP serves all three.
- **Avoids head-to-head competition with mature runtimes.** Hermes / AutoGen / CrewAI have years of head start on the runtime shape. LOOP doesn't need to fight there.
- **Aligns with the backbone-infrastructure identity.** The "AGI primitive" framing requires being underneath products, not being one.

## Consequences

**Pros:**
- Smaller build scope (no UI to design, no chat interface to maintain)
- Distribution leverages existing user habits (npm install, configure in your existing tool)
- Naturally model-agnostic (whatever LLM the host uses, LOOP serves)
- Compatible with eventual proprietary hosted SaaS and API tiers without changing the substrate shape

**Cons:**
- **Feedback signal capture is harder** (see [ADR-0002](0002-loop-complements-claude-code.md)) — LOOP can't passively observe user behavior because it doesn't sit between user and LLM
- LOOP depends on MCP-host adoption (currently strong, but a fragile dependency)
- Less control over the end-user experience — UX is whatever the host provides
- Smaller-feeling product to outside observers compared to a full runtime

## What this means concretely

- **No LOOP terminal interface** ships in beta
- **No LOOP chat UI** ships in beta
- **All end-user interaction with LOOP** happens through the host's MCP integration
- **For app builders**, LOOP exposes an API tier — they embed LOOP into their product, not the other way around
- **For multi-agent system authors**, LOOP is the persistent layer beneath their orchestrator, not the orchestrator itself (even though LOOP includes its own multi-agent orchestration — see [ADR-0001](0001-multi-agent-orchestration-day-one.md))

## Alternatives considered

- **Own runtime (Hermes shape):** rejected — head-to-head competition with mature runtimes, asks users to switch, doesn't fit backbone identity
- **API-only, no MCP:** rejected — leaves the entire end-user distribution channel unserved
- **Hybrid (own runtime + MCP):** rejected for beta — doubles the build scope; could revisit post-beta if MCP-host adoption stalls

## Related

- [ARCHITECTURE.md](../ARCHITECTURE.md) — Surfaces section
- [ADR-0002](0002-loop-complements-claude-code.md) — complement mode for Claude Code
