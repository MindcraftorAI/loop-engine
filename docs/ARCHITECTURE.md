# Architecture

## The four-stage loop

LOOP runs a `Listen → Observe → Operate → Publish` cycle. Each stage is configurable per-app:

- **Autonomous** — runs without human intervention
- **Gated** — pauses for human approval
- **Skipped** — not used for this app

Example: RankLabs runs Listen / Observe / Operate autonomously and gates Publish for human review.

## Orchestration: Claude orchestrates, LOOP scaffolds and learns

LOOP does NOT manage orchestration through a DAG engine. The host LLM (Claude, GPT, etc.) decides what to do, when, and in what order — calling tools, invoking skills, spawning subagents. LOOP provides three categories of value to that orchestrator:

1. **The unified toolset** — LOOP's own skills + all connected external MCP tools (see [ADR-0011](decisions/0011-dual-mcp-role-server-and-client.md)) — available as one toolset to the host LLM
2. **Memory + Lessons** that make orchestration decisions smarter over time
3. **Light deterministic primitives** for the narrow cases where the LLM alone struggles:

| Primitive | Purpose |
|---|---|
| `loop_schedule(at_time, prompt)` | Cron-triggered LLM invocations (scheduled posts, recurring jobs) |
| `loop_parallel(tasks=[...])` | Explicit fan-out when driving many tools simultaneously without context bloat |
| `loop_retry(tool, policy)` | Coded retry with backoff for flaky external tools |
| `loop_gate(prompt, approver)` | Pause for human approval (per-stage autonomy policy) |

See [ADR-0012](decisions/0012-claude-as-orchestrator.md) for the full reasoning.

### Multi-agent through subagent-as-tool

Multi-agent orchestration is still first-class (see [ADR-0001](decisions/0001-multi-agent-orchestration-day-one.md)), but the mechanism is simple:

- Parent LLM has a `spawn_subagent(persona, task, scoped_memory_access)` tool
- Subagent runs as its own LLM call with its own scoped memory + skills + persona
- Subagent results return as tool results to the parent
- Parent continues orchestrating based on the results

The `Agent` entity remains a first-class data-model entity — it's a persona + memory scope + skill access configuration. It just gets invoked as a subagent tool call rather than scheduled by a LOOP orchestrator. Sequential and parallel patterns work naturally; hierarchical/swarm are post-beta.

### How orchestration improves over time

The orchestration **gets smarter** because the context fed to the host LLM gets richer, not because LOOP develops a smarter orchestration algorithm. Examples of what Lessons learn here:

- "When posting a builder-journey update, Claude usually drafts the thread, then adapts to LinkedIn — surface the linkedin-adapter skill in the manifest when twitter-thread skill activates"
- "Claude tends to skip checking past-post performance before drafting; auto-inject `past_post_performance` memory when content-generation skills load"

This is meta-learning about *routing patterns*, not just generation patterns. Distinguishes LOOP from any memory-only product.

### ExecutionStep as observability

Every meaningful action in a session — tool call, skill invocation, memory access, subagent spawn, deterministic primitive call — is logged as an `ExecutionStep`. This is a pure observability/audit layer, not a control layer. Used for:

- Audit and debugging
- Feedback signal attribution (which step produced which outcome)
- Lesson seeding (over time: "the parent agent's pattern for publishing flows is X")
- Replay / partial retry (if a step failed, recover from the last good step)

## Skills

Skills are the unit of capability. Each has:
- A **type** — `generative` (produces content) or `analytical` (extracts insight from data)
- A **versioned content body** (Markdown for user-authored, code references for programmatic)
- Optional **external sources** for live ingestion (e.g., skill that tracks the latest React docs)
- A **feedback-signal profile** that controls how it learns
- A **promotion policy** that controls when attached lessons get committed

User-authored skills are Markdown files with frontmatter, matching Claude Code's pattern. See [On-disk Layout](#on-disk-layout) below for full paths.

Skills can chain — analytical output feeds into generative input.

## Memory

Memory is scoped hierarchically:

| Scope | Visible to |
|---|---|
| Tenant | Everything in the tenant |
| App | All skills/agents within an app |
| Skill-set | A related skill cluster |
| Skill | One skill only |
| Agent-shared | All agents in one execution |
| Agent-private | One agent only |

### Retrieval: hybrid manifest + lazy recall

Memory uses a hybrid pattern that combines Claude Code's lightweight model with Hermes-style scalability:

1. **Active manifest** in the system prompt (~20-50 entries) — memory IDs + short descriptions for highest-priority memories in current scope
2. **Searchable corpus** outside context — accessed via semantic search on demand

Two retrieval tools exposed via MCP:

- `loop_recall_memory(id)` — fast path when the description already told the model what it needs
- `loop_search_memory(query, limit)` — semantic search fallback for unknown-unknowns

**Token budget per session:** ~2-5k manifest + 1-2k active lessons + 200-2000 per on-demand recall. <10k typical overhead.

### Mitigating model-skips-relevant-memory

Defense in depth. Beta ships the first three layers:

| Layer | When | Beta? |
|---|---|---|
| 1. Smart manifest ordering by similarity to current input | Always-on | Yes |
| 2. Auto-inject very-high-confidence matches (>0.92 sim) | Always-on | Yes |
| 3. User feedback loop ("which memories should have been used?") | Per-response, optional | Yes |
| 4. Soft hints on disagreement (>0.75 sim, model skipped) | Per-turn | Post-beta |
| 5. Lesson-driven trigger patterns (system learns its own misses) | Continuous | Post-beta |
| 6. Post-response audit | Opt-in, paid tier | Post-beta |

## Lessons

Lessons are provisional learnings that sit between raw `FeedbackSignal`s and committed skill/memory updates. The pattern is the same as git's working-directory → staging → commit.

### Status lifecycle

```
observed → hypothesized → active → applied → validated → consolidated → promoted
                                       ↓
                             (or discarded, expired, superseded)
```

When `active`, a Lesson is layered onto a skill's effective content at inference time as supplementary context — so users get in-flight learning **immediately**, not only after the next promotion event. This is the mechanism behind "continuous context compounding."

### Promotion is a judgment, not a counter

LOOP's lesson promotion deliberately rejects pure counter-based accumulation. Promotion is shaped after how humans actually encode lessons (salience overrides volume; surprise/expectation-violation is the strongest encoder; causal narrative required; pattern crossover validates; consolidation needs reflection time).

**Required for promotion:**
- Causal narrative exists (the *why*)
- Lesson survived application phase with positive signal
- Lesson passed a consolidation event
- Zero strong negatives during application

**Any one of:**
- Salience high enough alone (one severe event can promote)
- Volume accumulated
- Pattern crossover validated (same insight in 2+ contexts)

Behind these checks, a configurable `PromotionPolicy` controls how six dimensions (volume, score, ratio, time window, negative floor, app-defined metric) weight together. Each dimension can be `REQUIRED`, `WEIGHTED`, or `INFORMATIONAL`. Presets: `default`, `safety_critical`, `fast_iteration`, `trending`, `production_critical`. Skills override per-skill in MD frontmatter.

### Consolidation events

Distinct from continuous accumulation. Periodic explicit reviews of active lessons:

- Pattern crossover detection across skills
- Narrative convergence (multiple lessons proposing the same *why*)
- Contradiction resolution
- Application validation

Time-triggered (nightly), event-triggered (after N applied uses), or user-triggered. This is the "sleep on it" step no existing memory product does — and a strategic differentiator.

## Memory bundles (marketplace primitive)

Memory is packageable. Bundles export memory + lessons + skill references as portable units. Two intent types:

- **Performance packs** — make a known skill measurably better at a known task (e.g., subject lines that hit >40% open rate, playbooks from successful executions)
- **Versatility packs** — extend what an agent can do at all (e.g., medical regulatory knowledge, regional market norms, edge-case recovery patterns)

Bundle format: directory tree with `manifest.yaml` (intent, provenance, compatibility, signature) + content files, zipped as `.loop`. Provenance metadata is required.

The marketplace itself is post-beta. The format ships day-one so portability isn't a painful retrofit later.

## Surfaces

LOOP has **dual MCP role**: it is both an MCP server (exposes capabilities to LLM hosts) AND an MCP client (consumes tools from external MCP servers). See [ADR-0011](decisions/0011-dual-mcp-role-server-and-client.md).

### MCP server side (LOOP exposes capability)

For end-users plugging LOOP into existing LLM hosts. Runs locally as an MCP server. Tool surface:

**Status / discovery**
- `loop_status` — runtime snapshot (version, counts)
- `loop_list_skills` — list skills with descriptions
- `loop_list_teams`, `loop_list_personas` — list available teams and personas

**Content retrieval**
- `loop_get_skill(slug)` — full content of a single skill
- `loop_recall_memory(id)`, `loop_search_memory(query)` — memory retrieval
- `loop_get_session_context()` — full content of everything currently active in this session

**Session activation** (see [ADR-0013](decisions/0013-persona-team-session-activation.md))
- `loop_load_team(slug)` / `loop_load_persona(slug)` — load and activate for the session; returns content immediately, persists activation
- `loop_active_teams()` / `loop_active_personas()` — list currently active
- `loop_deactivate_team(slug)` / `loop_deactivate_persona(slug)` — remove one
- `loop_clear_active_teams()` / `loop_clear_active_personas()` — clear all

**Feedback and observability**
- Feedback capture (explicit signal tools: "this worked" / "this didn't")
- Lesson visibility ("what is LOOP currently learning")

**Deterministic orchestration primitives** (see [ADR-0012](decisions/0012-claude-as-orchestrator.md))
- `loop_schedule`, `loop_parallel`, `loop_retry`, `loop_gate`

**Subagent**
- `spawn_subagent(persona, task, scoped_memory_access)`

**Complements** Claude Code's existing `.claude/skills/` and auto-memory — does not replace them. LOOP lives entirely under `~/.loop/` to avoid namespace collisions (see [On-disk Layout](#on-disk-layout)).

### MCP client side (LOOP consumes external tools)

LOOP connects to external MCP servers and routes their tools into the same toolset the orchestrating LLM sees. Examples:

- `twitter-mcp`, `linkedin-mcp`, `instagram-mcp`, `tiktok-mcp` — multi-platform posting for the content creator app
- `notion-mcp`, `figma-mcp`, `linear-mcp`, `github-mcp` — productivity / dev workflow integrations
- Any MCP server in the broader ecosystem

Connections, tool discovery, credential management, schema-skew handling, and failure isolation are all part of the client implementation. Security model: external tool outputs are fenced with `<tool-result>` markers + streaming scrubber (same pattern as recalled memory); skills must declare their external-tool dependencies in frontmatter; credentials are scope-isolated per tenant.

### API tier (heavy)

For app builders (RankLabs, content generator, indie AI products) and multi-agent system authors. Multi-tenant, embedded in product architecture. Consumer apps define their own feedback signals.

First-class support for **programmatic non-human callers**: structured I/O, deterministic versioning, rate limits, API keys + service accounts (not just user OAuth).

## Deployment tiers

| Tier | Where it runs | What you get |
|---|---|---|
| **Free** | Self-hosted (laptop, server, container, your own cloud) | Local skills + memory, MCP server, multi-agent orchestration, user-authored skills, best-effort live-source ingestion |
| **Paid** | LOOP-hosted SaaS | Device sync, hosted 24/7 ingestion, multi-tenancy, team features, API tier, marketplace participation (post-beta), centralized pool participation (post-beta) |

Data model and skill/memory formats are **identical** across both modes. Cloud tier adds sync + hosted services on top — no divergent codebases.

**"Local" means self-hosted, not laptop-only.** RankLabs runs LOOP inside its own cloud as a free-tier deployment during beta.

## Stack

| Concern | Choice |
|---|---|
| Language | TypeScript / Node.js |
| MCP server | Anthropic's official TS SDK |
| Persistence | SQLite (`better-sqlite3`) with FTS5 |
| Vector search (free tier) | `sqlite-vec` (Apache 2.0) — in-process, single file, cosine distance |
| Vector search (paid tier) | Postgres + pgvector — same RRF hybrid pattern, multi-tenant |
| Hybrid search ranking | Reciprocal Rank Fusion (k=60) across FTS5 + vec0 |
| Embeddings | Pluggable: Voyage (distinct API) + OpenAI-compatible (covers OpenAI cloud, Ollama, TEI, LM Studio). Default recommended preset: Qwen3-Embedding-4B via Ollama (local, Apache 2.0). FTS5-only fallback when no provider configured. |
| LLM calls | Anthropic and OpenAI TS SDKs |
| Packaging | npm (laptop installs) + Docker (server deployments) |
| Tests | vitest |
| Type checking | TypeScript strict mode |
| Lint / format | eslint + prettier |

See [ADR-0004](decisions/0004-language-nodejs-typescript.md) for the language decision reasoning.

## Design principles

1. **In-context learning, not training.** Works on any closed model and any open model. No GPU dependency.
2. **Local-first.** Free tier runs entirely self-hosted; paid tier adds hosted services as an additive layer.
3. **Complement, don't replace.** LOOP slots alongside existing tools (Claude Code, Cursor) via MCP — doesn't ask users to abandon what works.
4. **Continuous, not stepwise.** Active Lessons inject in-flight; users feel learning immediately, not after a training cycle.
5. **Provenance-tagged from day one.** Every memory and bundle carries creator, source, last-validated, performance claims — even before the marketplace ships.
6. **Defense-in-depth on retrieval.** Multiple layers prevent the model from skipping relevant memory.
7. **Code-quality discipline.** No file over ~500 lines without strong reason. Module boundaries defined upfront, not emergent. Linter/formatter/type-checker from day one.

## On-disk layout

For the self-hosted free tier, all LOOP state lives under `~/.loop/`. Files are canonical and user-readable; the SQLite database is a derived index over them for fast retrieval.

```
~/.loop/
├── config.yaml              # user configuration
├── db/                      # SQLite + vector index (derived, not authoritative)
│   ├── loop.sqlite
│   └── vectors.idx
├── skills/                  # canonical skill files
│   └── <name>/
│       ├── SKILL.md
│       ├── references/
│       └── templates/
├── memory/                  # canonical memory files; DB indexes them
│   └── <scope>/
│       └── <memory-id>.md
├── lessons/                 # provisional learnings, organized by status
│   ├── active/              # currently layered into inference
│   ├── pending/             # observed / hypothesized — not yet active
│   ├── promoted/            # archived after merge into skill/memory (audit trail)
│   └── discarded/           # kept briefly for debugging
├── bundles/                 # installed marketplace packs (unpacked)
│   └── <bundle-id>/
└── logs/                    # debug + audit logs
```

Principles:

- **Files are canonical, DB is an index.** The user can edit a skill or lesson file manually; LOOP detects the change and re-indexes. Deleting `~/.loop/db/` rebuilds the index from files; deleting files loses data.
- **Status-as-directory for lessons** (not status-in-frontmatter). `ls ~/.loop/lessons/active/` shows in-flight learning at a glance. Status transitions move files between subdirs.
- **Inspectability over performance.** Performance comes from SQLite + vectors.idx; trust comes from being able to read what's actually on disk.

For server / container deployments (RankLabs's use case), the same layout applies but rooted at a configurable path (e.g., `/var/lib/loop/`) via `LOOP_HOME` env var. The hosted SaaS tier abstracts the filesystem layer entirely; the equivalent records live in Postgres + a vector DB.

See [ADR-0010](decisions/0010-on-disk-file-layout.md) for the design rationale.

## Key flows

### Free-tier end-user vibe coding in Claude Code

1. User installs LOOP via `npm install -g @loop/core`
2. User configures LOOP as an MCP server in Claude Code settings
3. Claude Code session starts → LOOP injects active manifest into context
4. User asks a coding question → Claude Code may call `loop_recall_memory` for relevant prior decisions
5. Skill the user has defined (`vibe-coder-react.md`) is exposed as an MCP tool — Claude Code can invoke it
6. After response, LOOP captures implicit feedback signals from session pattern + explicit feedback if user marks "this worked"
7. Eventually, signals seed a Lesson; Lesson goes through lifecycle; promotion adds permanent improvement

### RankLabs running LOOP in its own cloud

1. RankLabs deploys LOOP runtime (Docker image) inside its infrastructure
2. RankLabs's brand-level apps consume the LOOP API tier — each brand is an `App` instance
3. RankLabs's content generation pipeline invokes LOOP-managed skills (schema, copy, posts, reels)
4. Analytical skills extract gap-detection from crawled data and feed generative skills via skill chaining
5. Publish stage is gated — humans approve outputs
6. RankLabs feeds outcome signals back (gap closed, content published, engagement metrics) → LOOP's Lesson system promotes improvements over time
