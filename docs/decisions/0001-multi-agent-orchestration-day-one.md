# ADR-0001: Multi-agent orchestration is day-one, not v1.x

**Status:** Accepted
**Date:** 2026-05-09

## Context

LOOP was initially framed as substrate underneath orchestrators like LangGraph / CrewAI / AutoGen — a memory + skill layer that any orchestrator could plug into. The "substrate-underneath" framing positioned LOOP as non-competitive with orchestration frameworks.

User reframed this: LOOP itself includes multi-agent orchestration as a first-class capability — it is a vertically integrated stack (4-stage loop + persistent skill+memory + multi-agent orchestration in one).

## Decision

Multi-agent orchestration ships in the day-one beta. `Agent` is a first-class data-model entity from the start. Inter-agent messaging, shared vs private memory boundaries, skill-sharing rules, and sequential/parallel orchestration patterns are required for beta. Hierarchical and swarm patterns are post-beta.

## Consequences

**Pros:**
- Vertically integrated stack matches the Vercel / Supabase pattern where integration is the value
- Avoids leaving the multi-agent layer to fragmented external orchestrators that don't share LOOP's persistence model
- Multi-agent setups inherit the same Lesson model and memory scopes that single-agent does

**Cons:**
- Build scope grows meaningfully — orchestration is its own subsystem (registry, messaging, error propagation, observability)
- Direct competition with mature multi-agent frameworks (LangGraph, CrewAI, AutoGen, OpenAI Swarm, Anthropic Agent SDK) on orchestration
- Adds 4-6 weeks to the beta build estimate vs single-agent-only

## Alternatives considered

- **Substrate-only (rejected):** LOOP serves memory + skills via MCP; users bring their own orchestrator. Smaller build but loses the integrated-stack thesis and forces users to manage two layers.
- **Multi-agent in v1.1 (rejected):** Ship single-agent beta first, add multi-agent later. Risks data-model migration pain since `Agent` would need to be retrofitted as first-class. User explicitly stated multi-agent is "what makes LOOP whole."

## Update (2026-05-12) — mechanism, not capability

The capability (multi-agent first-class) is unchanged. The mechanism has been refined by [ADR-0012](0012-claude-as-orchestrator.md): instead of LOOP managing a DAG of agents explicitly, the host LLM (Claude) orchestrates via a `spawn_subagent` tool. Subagents run with their own scoped memory + skills + persona. `Agent` remains a first-class data entity. The original "what makes LOOP whole" framing still holds — multi-agent simply gets there through a simpler, more LLM-native mechanism.

## Related

- [ADR-0012](0012-claude-as-orchestrator.md) — orchestration mechanism update
- [ARCHITECTURE.md](../ARCHITECTURE.md) — orchestration section
- [DATA_MODEL.md](../DATA_MODEL.md) — Agent and ExecutionStep entities (ExecutionStep replaces AgentInteraction)
