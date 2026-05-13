# Data Model

The day-one beta entities. Reserve fields for paid-tier additions (User, Account, ApiKey, ServiceAccount, Billing) but don't build them yet.

## Identity & isolation

### Tenant
Top-level isolation boundary.

| Field | Notes |
|---|---|
| `id` | UUID |
| `name` | Display |
| `created_at` | Timestamp |
| `marketplace_contribution_opt_in` | Default `false`. Explicit consent required for data to flow into the centralized pool (post-beta). |

For free self-hosted: usually one tenant per install. For paid hosted: many.

### App
A consuming application within a tenant.

| Field | Notes |
|---|---|
| `id` | UUID |
| `tenant_id` | FK |
| `name` | Display |
| `description` | Free text |
| `default_stage_autonomy_policy` | JSON — per-stage (Listen/Observe/Operate/Publish) `autonomous` / `gated` / `skipped` |
| `default_promotion_policy` | JSON — Lesson promotion defaults for skills in this app |
| `created_at` | Timestamp |

Examples: RankLabs, content-generator, "my Claude Code dogfood app".

## Core actors

### Agent
First-class entity. Multi-agent orchestration is day-one.

| Field | Notes |
|---|---|
| `id` | UUID |
| `app_id` | FK |
| `name` | Display |
| `role` | Free text |
| `active_persona_id` | Nullable FK to Persona |
| `stage_autonomy_overrides` | JSON — overrides app defaults if set |
| `created_at` | Timestamp |

### Persona
Captures WHO an agent is acting as — voice, role, perspective. Does NOT bundle skills directly; that responsibility moves to [Team](#team). See [ADR-0013](decisions/0013-persona-team-session-activation.md).

| Field | Notes |
|---|---|
| `id` | UUID |
| `tenant_id` | FK (nullable) — null = global; set = tenant-private |
| `app_id` | FK (nullable) — null = available to all apps in tenant |
| `name` | Display |
| `voice_profile` | JSON — voice/tone configuration |
| `description` | Free text |
| `created_at` | Timestamp |

At runtime, an Agent may compose Persona × Team(s) — the agent uses this Persona's voice while accessing one or more Teams' skills.

### Team
Captures WHAT toolkit the agent has access to — a curated bundle of skills loaded as a unit. See [ADR-0013](decisions/0013-persona-team-session-activation.md).

| Field | Notes |
|---|---|
| `id` | UUID |
| `tenant_id` | FK (nullable) — null = global; set = tenant-private |
| `app_id` | FK (nullable) — null = available to all apps in tenant |
| `name` | Display |
| `description` | Free text |
| `created_at` | Timestamp |

N:M relationship to Skill via `TeamMembership` (which skills are members of this team). On disk: `~/.loop/teams/<slug>/TEAM.md` with frontmatter and a `members:` list referencing skill slugs.

## Capability layer

### Skill
Unit of capability.

| Field | Notes |
|---|---|
| `id` | UUID |
| `tenant_id` | FK |
| `app_id` | FK (nullable) — null = tenant-wide; set = app-private |
| `name` | Display |
| `slug` | URL-friendly identifier, unique per scope |
| `skill_type` | Enum: `generative`, `analytical` |
| `source` | Enum: `user_authored_md`, `installed_pack`, `programmatic` |
| `current_version_id` | FK to SkillVersion |
| `external_source_id` | FK (nullable) — for live-docs skills |
| `promotion_policy` | JSON — overrides app default if set |
| `created_at` | Timestamp |

### SkillVersion
Required for rollback and auto-update auditability.

| Field | Notes |
|---|---|
| `id` | UUID |
| `skill_id` | FK |
| `version` | Monotonic int per skill |
| `content` | The MD body or programmatic reference |
| `created_by_trigger_id` | FK (nullable) to UpdateTrigger — what caused this version |
| `promoted_lesson_ids` | Array — lessons that justified this version (audit trail) |
| `feedback_signal_aggregate` | JSON — snapshot of signals at promotion time |
| `created_at` | Timestamp |

### ExternalSource
Feed for live ingestion (e.g., React docs).

| Field | Notes |
|---|---|
| `id` | UUID |
| `name` | Display |
| `url_or_identifier` | Source location |
| `fetch_schedule` | Cron-like expression |
| `diff_strategy` | Enum — how to detect meaningful changes |
| `last_fetched_at` | Timestamp |
| `last_content_hash` | For change detection |

## Memory layer

### Memory
Context that persists.

| Field | Notes |
|---|---|
| `id` | UUID |
| `scope_type` | Enum: `tenant`, `app`, `skill_set`, `skill`, `agent_shared`, `agent_private` |
| `scope_id` | Polymorphic FK based on scope_type |
| `content` | Text + structured fields |
| `description` | Short high-signal field — load-bearing for retrieval (used in active manifest) |
| `embedding` | Vector — for semantic search |
| `pin_priority` | Integer — controls inclusion in active manifest |
| `last_accessed_at` | Timestamp — recency ranking |
| `provenance` | JSON: source, derived_from, performance_claims, last_validated_at |
| `bundle_id` | FK (nullable) — if installed from a memory bundle |
| `created_at` | Timestamp |
| `updated_at` | Timestamp |

### MemoryBundle
Packageable, portable memory.

| Field | Notes |
|---|---|
| `id` | UUID |
| `name` | Display |
| `version` | Semver |
| `intent` | Enum: `performance`, `versatility` |
| `creator_signature` | Bundle creator identity |
| `integrity_hash` | For verification |
| `compatibility` | JSON — which skill_types, skill_slugs, personas this augments |
| `provenance_manifest` | JSON — creator, data source, performance claims, last-updated, sample count |
| `source_url` | Where it was installed from |
| `installed_at` | Timestamp |

1:N with Memory entries.

## Update mechanics

### FeedbackSignal
Drives auto-updates.

| Field | Notes |
|---|---|
| `id` | UUID |
| `target_type` | Enum: `skill`, `memory`, `agent` |
| `target_id` | Polymorphic FK |
| `source` | Enum: `implicit_mcp_pattern`, `explicit_user`, `inferred_followup`, `app_defined` |
| `signal_kind` | Enum: `kept`, `edited`, `rejected`, `rated`, `usage_frequency`, `custom` |
| `score` | Normalized -1..1 or task-specific |
| `raw_data` | JSON for custom metrics |
| `context_run_id` | FK to LoopExecution |
| `captured_at` | Timestamp |

### Lesson
Provisional learning between FeedbackSignal and committed update.

| Field | Notes |
|---|---|
| `id` | UUID |
| `target_type` | Enum: `skill`, `memory` |
| `target_id` | Polymorphic FK |
| `content` | The proposed update (delta, replacement, or supplementary context) |
| `causal_narrative` | Text — REQUIRED for promotion. The proposed *why*. LLM-assisted draft, refined as signal accumulates. |
| `status` | Enum: `observed`, `hypothesized`, `active`, `applied`, `validated`, `consolidated`, `promoted`, `discarded`, `expired`, `superseded` |
| `salience_score` | Composite of source × consequence × stakes — high salience can warrant promotion alone |
| `surprise_score` | How unexpected vs prior skill/memory state |
| `pattern_crossover` | Array of related lesson/skill IDs — cross-context validation |
| `contradiction_score` | Does this contradict existing content |
| `application_record` | JSON — was lesson served in inference, did outputs improve |
| `source_feedback_signal_ids` | Array — which signals seeded this |
| `confidence_score` | Running aggregate |
| `created_at` | Timestamp |
| `last_validated_at` | Timestamp |
| `expires_at` | TTL — auto-discard if no validation signal |
| `promoted_to_version_id` | FK (nullable) — set when promoted |

### UpdateTrigger
What causes an actual update event.

| Field | Notes |
|---|---|
| `id` | UUID |
| `target_type` | Enum: `skill`, `memory` |
| `target_id` | Polymorphic FK |
| `trigger_kind` | Enum: `lesson_proposed`, `lesson_promoted`, `lesson_discarded`, `lesson_expired`, `threshold`, `schedule`, `external_source_changed`, `manual` |
| `result` | Enum: `succeeded`, `failed`, `rolled_back` |
| `why` | JSON — observability record of which promotion-policy dimensions passed/failed |
| `resulting_version_id` | FK (nullable) |
| `triggered_at` | Timestamp |

## Orchestration

### LoopExecution
One iteration of the four-stage loop.

| Field | Notes |
|---|---|
| `id` | UUID |
| `app_id` | FK |
| `triggering_agent_id` | FK (nullable) |
| `stage_log` | JSON — timing + outcome per Listen/Observe/Operate/Publish |
| `inputs` | JSON |
| `outputs` | JSON |
| `status` | Enum |
| `started_at` | Timestamp |
| `ended_at` | Timestamp |

Critical for observability and feedback-signal attribution.

### ExecutionStep
Unified observability record for every meaningful action within a LoopExecution. Replaces the earlier `AgentInteraction` concept; agent-to-agent messages become a kind of ExecutionStep (`step_kind = agent_message`). LOOP does not use this entity for control flow — it is purely an observability / audit / replay layer. The orchestrating LLM (Claude or comparable) decides what to do; LOOP records what was done.

See [ADR-0012](decisions/0012-claude-as-orchestrator.md) for the orchestration model that makes this an observability layer rather than a control layer.

| Field | Notes |
|---|---|
| `id` | UUID |
| `loop_execution_id` | FK |
| `step_index` | Position in the execution sequence |
| `parent_step_id` | FK (nullable) — for nested steps (e.g., agent calling a tool) |
| `step_kind` | Enum: `agent_run`, `tool_call`, `skill_invocation`, `memory_access`, `lesson_event`, `agent_message`, `primitive_call` |
| `target_type` | Enum tied to step_kind — what is being executed |
| `target_id` | Polymorphic FK to the target entity (agent, tool, skill, memory, lesson, primitive name) |
| `inputs` | JSON |
| `outputs` | JSON |
| `status` | Enum: `pending`, `running`, `succeeded`, `failed`, `gated_for_approval`, `skipped` |
| `error` | JSON (nullable) — error details if failed |
| `started_at` | Timestamp |
| `ended_at` | Timestamp |
| `triggering_step_id` | FK (nullable) — what step caused this one to fire |

## MCP integration

LOOP has dual MCP role — it serves clients (LLM hosts connecting to LOOP) and also acts as a client itself (connecting to external MCP servers). See [ADR-0011](decisions/0011-dual-mcp-role-server-and-client.md).

### MCPSession
Captures one external MCP client connecting **to** LOOP (LOOP-as-server).

| Field | Notes |
|---|---|
| `id` | UUID |
| `tenant_id` | FK |
| `app_id` | FK |
| `client` | Free text (e.g., "claude-code", "claude-ai", "cursor") |
| `exposed_skill_ids` | Array — which skills were available this session |
| `active_team_slugs` | Array — Teams currently activated in this session (session-activation semantic, [ADR-0013](decisions/0013-persona-team-session-activation.md)) |
| `active_persona_slugs` | Array — Personas currently activated in this session |
| `started_at` | Timestamp |
| `ended_at` | Timestamp |

Implicit feedback signals derive from this session's call patterns. Active team / persona state lives here and is bounded by the session lifecycle.

### MCPClientConnection
Captures one connection LOOP holds open **to** an external MCP server (LOOP-as-client). Sits alongside `MCPSession` — different direction, distinct entity.

| Field | Notes |
|---|---|
| `id` | UUID |
| `tenant_id` | FK |
| `name` | Display (e.g., "twitter", "linkedin", "notion") |
| `server_url_or_command` | Connection target (URL for SSE, command for stdio) |
| `transport` | Enum: `stdio`, `sse`, `streamable_http` |
| `status` | Enum: `connected`, `disconnected`, `failed`, `pending` |
| `last_connected_at` | Timestamp |
| `last_error` | JSON (nullable) |

### ExternalMCPTool
A tool registered for use by LOOP skills/agents, sourced from a connected `MCPClientConnection`.

| Field | Notes |
|---|---|
| `id` | UUID |
| `connection_id` | FK to MCPClientConnection |
| `tool_name` | The tool's name as exposed by the external MCP |
| `prefixed_name` | LOOP's namespaced version (e.g., `twitter.post_thread`) — used for conflict resolution |
| `description` | From the external MCP's tool schema |
| `input_schema` | JSON Schema from the external MCP |
| `discovered_at` | Timestamp |
| `last_seen_at` | Timestamp |

Skills must declare which `ExternalMCPTool` IDs they're allowed to invoke; LOOP enforces at runtime.

### MCPClientCredential
Encrypted credential storage for external MCPs. Separate from regular memory because of its sensitivity profile.

| Field | Notes |
|---|---|
| `id` | UUID |
| `connection_id` | FK to MCPClientConnection |
| `tenant_id` | FK — scope-isolation enforced; Tenant A's creds never leak to Tenant B |
| `credential_key` | The credential's purpose (e.g., "api_key", "oauth_token", "refresh_token") |
| `encrypted_value` | Encrypted blob; decrypted only at point of use |
| `expires_at` | Timestamp (nullable) — for OAuth refresh logic |
| `created_at` | Timestamp |
| `updated_at` | Timestamp |

## Relationships overview

```
Tenant 1─N App 1─N Agent
                  └─N─M Skill (via AgentSkillAccess)
                  └─N Memory(scope = agent_shared | agent_private)
       1─N Persona (or Persona scoped at App)
       1─N Team    (or Team scoped at App) 1─N─M Skill (via TeamMembership)
       1─N Skill 1─N SkillVersion
                 └─N─0..1 ExternalSource
                 └─N─M ExternalMCPTool (declared dependencies)
       1─N Memory (any scope) 0..N─0..1 MemoryBundle
       1─N MCPSession                                     (LOOP-as-server)
       1─N MCPClientConnection 1─N ExternalMCPTool        (LOOP-as-client)
                                1─N MCPClientCredential
       1─N LoopExecution 1─N ExecutionStep
                          1─N FeedbackSignal
       1─N Lesson 0..1─FK SkillVersion (promoted_to)
       1─N UpdateTrigger
```

## Persistence

- **Free tier (shipped):** SQLite (`better-sqlite3`) with FTS5 for full-text + `sqlite-vec` for embeddings in the same database file. Hybrid retrieval combines both via Reciprocal Rank Fusion (k=60). Files on disk remain canonical; the SQLite database is a rebuildable index over them.
- **Paid tier:** Postgres with `pgvector` for multi-tenant data + embeddings in the same store. Same RRF hybrid pattern, same memory/lesson record shapes — only the backend differs.

Same schema concepts, different backends. Persistence is abstracted behind a provider interface so swapping doesn't ripple.

## NOT in the day-one data model

- User / Account / ApiKey / ServiceAccount — paid-tier auth. Reserve fields; don't build.
- Billing / Subscription — paid tier.
- MarketplaceListing / Purchase / Review — post-beta.
- CentralizedPoolContribution — post-beta.
- AgentGroup / Swarm coordination tables — v1.x.
