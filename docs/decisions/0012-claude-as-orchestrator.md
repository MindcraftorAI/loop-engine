# ADR-0012: Claude orchestrates, LOOP scaffolds and learns

**Status:** Accepted
**Date:** 2026-05-12

## Context

Earlier design implied LOOP would manage orchestration explicitly — a DAG-style execution graph with steps fired by dependency resolution, scheduling logic, and explicit per-step control flow.

Modern Claude (and comparable frontier models) is genuinely excellent at multi-step tool use: reading user intent, picking tools, calling them, reasoning over results, deciding the next step. This is how Claude Code, Cursor, and Windsurf actually work — they expose tools and let the model think. They do not run heavy orchestrators.

Building a deterministic orchestrator in LOOP would:
- Underutilize Claude's planning ability
- Compete with Claude's natural strengths
- Add weeks of engineering for capability the LLM already provides
- Force pre-programmed flows over context-adaptive ones

## Decision

**Claude (or whichever LLM the host is running) orchestrates. LOOP scaffolds and learns.**

LOOP provides:
1. **The unified toolset** — internal skills + external MCP tools (see [ADR-0011](0011-dual-mcp-role-server-and-client.md)) — all exposed to the orchestrating model
2. **Memory + Lessons** — context that makes orchestration decisions smarter over time
3. **Light deterministic primitives** for the narrow cases where Claude alone struggles:
   - `loop_schedule(at_time, prompt)` — cron-triggered Claude invocations (for scheduled posts, recurring runs)
   - `loop_parallel(tasks=[...])` — explicit fan-out when Claude needs to drive many tools simultaneously without context bloat
   - `loop_retry(tool, policy)` — coded retry with backoff for flaky external tools
   - `loop_gate(prompt, approver)` — pause for human approval (the per-stage autonomy policy from RankLabs's design)

That's the minimum scaffolding that complements Claude's reasoning without competing with it.

## How orchestration actually works

1. The LLM host (Claude Code, etc.) starts a session and connects to LOOP via MCP.
2. LOOP injects the active memory manifest + active lessons + available skill names into the session context.
3. Claude reads the user's intent and decides what to do — calling LOOP tools, external MCP tools, and LOOP's deterministic primitives as needed.
4. Every tool call is logged as an `ExecutionStep` (see below) with timing, inputs, outputs, status.
5. LOOP captures feedback signals from Claude's decisions and from external tool results (e.g., engagement data fetched later).
6. Lessons emerge from accumulated patterns. Examples of what Lessons learn:
   - "When posting a builder-journey update, Claude usually drafts the thread, then adapts to LinkedIn — surface the linkedin-adapter skill in the manifest when twitter-thread skill activates"
   - "Claude tends to skip checking past-post performance before drafting; auto-inject `past_post_performance` memory when content-generation skills load"

The orchestration **improves over time** because the context fed to Claude gets richer, not because LOOP develops a smarter orchestration algorithm.

## ExecutionStep as observability, not control

The `ExecutionStep` entity becomes a **logging / observability** layer rather than a control layer:

- LOOP records what Claude did (tool calls, skill invocations, memory accesses, subagent spawns, deterministic primitive calls) — not what to do next
- Used for: audit, debugging, feedback signal attribution, lesson seeding, replay/partial retry

This is a cleaner role than the original DAG-control model and is significantly easier to build.

## Multi-agent through subagent-as-tool

The multi-agent first-class commitment from [ADR-0001](0001-multi-agent-orchestration-day-one.md) is preserved, but the mechanism is simpler:

- Parent Claude has a `spawn_subagent(persona, task, scoped_memory_access)` tool
- Subagent runs as its own LLM call with its own scoped memory + skills + persona
- Subagent results return as tool results to the parent
- Parent Claude continues orchestrating based on the results

This is much simpler than coordinating two parallel LOOP-managed agents. The Agent entity remains first-class (it's a persona + memory scope + skill access configuration) — it just gets invoked as a subagent tool call rather than scheduled by a LOOP orchestrator.

Sequential and parallel multi-agent patterns still work:
- **Sequential:** parent calls subagent A, then subagent B with A's result
- **Parallel:** parent uses `loop_parallel` to spawn multiple subagents at once

## Consequences

**Pros:**
- Plays to Claude's strengths (multi-step planning, context-adaptive reasoning)
- Significantly smaller build scope — skips weeks of orchestrator engineering
- Matches the architecture of every leading AI-agent product (Claude Code, Cursor, Windsurf)
- Aligns with LOOP's substrate identity — LOOP serves the orchestrator, doesn't try to be one
- Lesson model becomes more powerful (meta-learning about *routing patterns*, not just generation patterns)
- Simpler failure modes — Claude reasons through errors the same way it reasons through everything else

**Cons:**
- Less deterministic — same input may produce different flows on different runs
- Reliability for strictly-scheduled production flows is lower (mitigated by `loop_schedule` primitive)
- Cost per orchestration decision = LLM call (mitigated because total LLM calls are similar; LOOP-managed orchestration would have had its own LLM-driven sub-steps anyway)
- Heavy parallel fan-out may need the `loop_parallel` primitive to avoid context bloat in the orchestrating Claude session

## Where LOOP-managed orchestration would have won

Honest accounting of cases where pure Claude-orchestration is weaker, and how LOOP's primitives mitigate:

- **Strictly-scheduled production flows:** `loop_schedule` triggers Claude at the right time with the right prompt
- **High-fan-out parallelism:** `loop_parallel` runs many subagents/tools in true parallel
- **Cost-sensitive automation at scale:** the deterministic primitives reduce LLM-call overhead for repetitive flows
- **Strict retry policies:** `loop_retry` with explicit backoff and circuit breakers

For everything else, Claude-orchestration wins.

## Beta scope impact

This decision **simplifies** beta scope:
- No DAG engine to build
- No dependency-resolution scheduler
- ExecutionStep model is logging, not control — much easier to implement
- Multi-agent reduces to subagent-as-tool (cleaner than coordinated parallel Claudes)

Combined with the MCP client work from [ADR-0011](0011-dual-mcp-role-server-and-client.md) (which adds 2-3 weeks), the net effect on the 4-6 month beta estimate is roughly neutral — the orchestration simplification cancels out the dual-role addition. Result: 4-6 months remains realistic.

## Alternatives considered

- **LOOP-managed DAG orchestrator (original plan):** rejected. Underutilizes Claude, weeks of engineering for capability the model already provides, fights against LOOP's substrate identity.
- **No orchestration support at all (let host fully drive):** rejected. The deterministic primitives (`loop_schedule`, `loop_parallel`, `loop_retry`, `loop_gate`) genuinely cover Claude's weak spots and are small to build.
- **Hybrid with heavy fallback to LOOP-managed flows:** rejected. Adds complexity without clear payoff; Claude is good enough that the fallback rarely activates.

## Related

- [ADR-0001](0001-multi-agent-orchestration-day-one.md) — multi-agent day-one (mechanism updated by this ADR)
- [ADR-0011](0011-dual-mcp-role-server-and-client.md) — dual MCP role (provides the toolset Claude orchestrates over)
- [ARCHITECTURE.md](../ARCHITECTURE.md) — orchestration section
- [DATA_MODEL.md](../DATA_MODEL.md) — `ExecutionStep` entity
