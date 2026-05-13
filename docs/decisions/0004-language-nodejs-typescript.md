# ADR-0004: Node.js / TypeScript as the implementation language

**Status:** Accepted
**Date:** 2026-05-11

## Context

LOOP's core needed a language decision. Two viable candidates:

- **Python** — dominant AI/ML ecosystem, Hermes is Python, mature LLM SDKs, mature embedding libraries
- **Node.js / TypeScript** — superior type system, better MCP ecosystem alignment, cleaner CLI distribution, single-language stack with future web UI

A polyglot stack was also considered and rejected.

## Decision

LOOP's core is built in **TypeScript on Node.js**.

If local on-device embeddings become a real user demand post-beta, a Python sidecar service will be added — but the core language is Node.

## Reasoning

### Why Node wins for LOOP specifically

| Factor | Why Node |
|---|---|
| Type system | TypeScript's type system is dramatically better than Python's mypy. Given LOOP's code-quality stance, this matters. |
| MCP ecosystem | Anthropic's reference MCP servers are TypeScript. Authoritative implementations live here. |
| CLI distribution | `npm install -g` works cleanly across platforms. Python packaging (pip/pipx/brew/pyinstaller/venv) is notoriously messy. |
| Startup performance | Node CLI: ~50-100ms. Python CLI: ~200-500ms. Real UX cost for an MCP server spawning on every session. |
| Single-language stack | Future paid-tier web UI is TypeScript regardless. One language across the stack reduces solo-dev context switching. |
| Developer tooling ecosystem | The CLI tools devs already use are mostly Node-based. |

### Why Python's advantages don't apply (much)

- **AI SDK maturity:** Anthropic and OpenAI TS SDKs are mature for beta scope. No real gap.
- **Local embeddings:** real Python advantage, but cloud embeddings (Voyage AI / OpenAI) are cheap and language-agnostic. Defer local embeddings until real demand justifies a Python sidecar.
- **Atropos-style training:** LOOP is not in the training business (see [ADR-0008](0008-closed-model-first.md) and trajectory-as-export strategy in [VISION.md](../VISION.md)). Even if LOOP eventually exposes trajectory data, training itself runs on partner infrastructure called via API — language-agnostic.
- **Hermes pattern extraction:** patterns are extracted, not code ported. Same-language porting would be a marginal time saving and would carry forward Hermes's monolithic code shape — anti-pattern for LOOP.

## Polyglot was considered and rejected

Two languages would mean two ecosystems, two type systems, two package managers, two test runners, two linters, two deployment paths — significant solo-dev tax. The cross-language seam (serialization, schema sync, IPC) is where complexity lives. Polyglot makes sense only when the boundary aligns with a team boundary or a narrow sidecar use case. Neither applies to LOOP at beta scope.

A Python sidecar may be added later for narrow needs (local embeddings) without affecting the core language choice.

## Consequences

**Pros:**
- Single language, single dependency tree, single tool chain
- Strict type checking catches refactor regressions
- Clean CLI + Docker distribution story
- Aligns with MCP ecosystem
- Same language as future web UI

**Cons:**
- Local on-device embeddings require either cloud APIs (beta solution) or eventual Python sidecar
- Hermes pattern extraction is translation, not copy — slightly more work
- If LOOP ever wants to fine-tune in-process, that path is closed (but trajectory-export-to-partner avoids needing it)

## Stack consequences

| Concern | Choice |
|---|---|
| Type checking | TypeScript strict mode |
| Test runner | vitest |
| Lint / format | eslint + prettier |
| Persistence | better-sqlite3 (with FTS5) |
| Vector search (local) | hnswlib-node or vectra |
| Vector search (cloud) | Pinecone / Qdrant / Weaviate / Chroma TS clients |
| LLM SDKs | @anthropic-ai/sdk, openai |
| MCP server | @modelcontextprotocol/sdk (Anthropic's official TS SDK) |
| Embeddings | Voyage AI or OpenAI cloud API for beta |
| CLI | commander or cac |
| Packaging | npm for laptop, Docker for server |

## Related

- [ARCHITECTURE.md](../ARCHITECTURE.md) — stack section
- [ADR-0009](0009-open-core-licensing.md) — license discipline that constrains Node dependency choices
