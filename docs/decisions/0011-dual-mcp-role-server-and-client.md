# ADR-0011: LOOP is MCP server AND client (dual role)

**Status:** Accepted
**Date:** 2026-05-12

## Context

Earlier design treated LOOP purely as an MCP **server** — exposes its skills, memory, and lessons to LLM hosts (Claude Code, Claude.ai, Cursor) as tools and resources.

The content creator reference app surfaced the gap: to publish across Twitter/X, LinkedIn, Instagram, and TikTok, LOOP needs to *call* tools exposed by other MCP servers (twitter-mcp, linkedin-mcp, instagram-mcp, tiktok-mcp). The same applies to any LOOP-powered agent reaching into Notion, Figma, Linear, GitHub, etc. via their respective MCP servers.

LOOP cannot be a real backbone substrate without consuming as well as serving.

## Decision

LOOP is both an MCP **server** AND an MCP **client**.

- **Server role:** LOOP exposes its own capabilities (skill invocation, memory recall/search, feedback signal capture, lesson visibility) to LLM hosts via MCP.
- **Client role:** LOOP connects to external MCP servers and routes their tools into the same toolset Claude and other LOOP-driven agents can see.

Together: external tools flow *in* through the client side; LOOP's own capabilities flow *out* through the server side. LOOP becomes a hub.

## Consequences

### What this enables
- Content creator skills can call `twitter-mcp.post_thread`, `linkedin-mcp.publish_article`, etc. without bespoke API integrations
- LOOP-powered agents inherit the entire MCP ecosystem (currently growing fast — Notion, Figma, Linear, GitHub, Slack, hundreds more)
- Skills can declare external-tool dependencies in their frontmatter; LOOP validates connectivity at load time
- Multi-platform automation flows become natural rather than special-cased

### What this requires
- **Connection management** — multiple persistent connections to external MCP servers, lifecycle, reconnection on failure
- **Tool discovery + registry** — at startup, each connected MCP is queried for its tools; LOOP maintains a unified registry
- **Tool routing** — when a skill or agent invokes a tool, LOOP resolves which MCP provides it
- **Credential management** — external MCPs require auth (API keys, OAuth tokens). LOOP needs an encrypted credential store, scoped per tenant
- **Schema-skew handling** — external MCPs evolve independently; LOOP's tool registry must refresh and surface breakage
- **Failure isolation** — one bad MCP must not crash LOOP

### Estimated cost
2-3 weeks of focused engineering on top of the base scope. The Anthropic TS SDK supports both client and server roles, so no language gap.

## Security model

External MCPs run untrusted code that produces untrusted output. Day-one safeguards:

- **Tool outputs are fenced** the same way recalled memory is — wrapped in `<tool-result>` markers with a streaming scrubber to prevent the fence from leaking across token deltas. This applies to all external MCP tool outputs uniformly.
- **Per-skill allow-list of external tools** — a skill must declare in its frontmatter which external tools it can call. Skills cannot dynamically reach for arbitrary tools at runtime. This bounds the blast radius if a skill is compromised.
- **Scope-isolated credentials** — Tenant A's stored credentials never leave Tenant A's scope. The credential store is encrypted at rest and decrypted only at point of use.
- **Tool-call audit log** — every external MCP call is logged as an `ExecutionStep` (see [ADR-0012](0012-claude-as-orchestrator.md)) with timing, inputs, outputs, errors. Auditable.
- **Conflict resolution** — if two MCP servers expose tools with the same name (e.g., `post_thread`), LOOP prefixes the source (`twitter.post_thread`, `bluesky.post_thread`) so skills can be explicit.

## Data model additions

Three new entities (see [DATA_MODEL.md](../DATA_MODEL.md)):

- `MCPClientConnection` — a connection LOOP holds open to an external MCP server. Sits alongside `MCPSession`, which represents external clients connecting *to* LOOP. Different direction, distinct entity.
- `ExternalMCPTool` — a tool registered for use by LOOP skills/agents, sourced from a connected `MCPClientConnection`.
- `MCPClientCredential` — encrypted credential storage for external MCPs. Separate from regular memory because of its sensitivity profile.

## Alternatives considered

- **Server-only (no client):** rejected — forces every external integration into bespoke API code or makes LOOP useless for cross-platform automation. Defeats the substrate identity.
- **Client-only (no server):** rejected — loses the MCP-host distribution channel (Claude Code, Cursor, etc.). The whole point of being on MCP is bidirectional.
- **External tools as a separate plugin system (non-MCP):** rejected — MCP is already the standard. Building a parallel plugin system would orphan LOOP from the ecosystem.

## Related

- [ARCHITECTURE.md](../ARCHITECTURE.md) — Surfaces section, expanded for dual role
- [ADR-0012](0012-claude-as-orchestrator.md) — orchestration model that consumes the unified toolset
- [DATA_MODEL.md](../DATA_MODEL.md) — `MCPClientConnection`, `ExternalMCPTool`, `MCPClientCredential` entities
