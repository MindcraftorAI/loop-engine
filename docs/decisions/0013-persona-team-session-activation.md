# ADR-0013: Persona × Team separation, session-activation loading

**Status:** Accepted
**Date:** 2026-05-12

## Context

The data model originally defined `Persona` as "curated bundle of voice + skills + frameworks." That conflated three concerns:

- Voice / role / perspective (the **WHO** an agent is acting as)
- Toolkit / skills (the **WHAT** capabilities it has)
- Implicit defaults binding the two

Dogfooding surfaced that voice and toolkit vary independently in real use:

- **Same persona, different toolkits:** a "Senior Engineer" persona uses a TypeScript-focused team on this repo and a Python-research team on another. Voice stays; toolkit changes.
- **Same toolkit, different personas:** "UX Designer" and "PM" personas can both use a design-thinking team. Same skills, different perspective.
- **Either alone:** Sometimes you only want voice (no toolkit), or only toolkit (no voice).

Concurrently, the loading semantic for skill bundles had two viable shapes:

- **Bundled-content** — the LLM calls `loop_load_*(slug)`, gets all content in the response, manages retention manually
- **Session-activation** — the LLM activates once; LOOP remembers and surfaces the content automatically in subsequent tool calls

The first puts the LLM fully in control of context. The second is meaningfully more convenient for non-developer end users (e.g. the content-creator app's user-and-wife scenario) where "set it once at the start of a session and let it just work" is the right UX.

## Decision

Three related parts:

### 1. Persona and Team are distinct entities

| Entity | Captures | Fields |
|---|---|---|
| **Persona** | WHO the agent is acting as — voice, role, perspective | name, voice_profile, description |
| **Team** | WHAT toolkit the agent has access to — bundle of skills | name, description, members (skill slugs) |

At runtime an Agent composes them: Persona × Team(s). Either or both can be loaded independently. Personas no longer reference Skills directly; that responsibility moves to Team.

### 2. Session-activation is the default loading semantic

`loop_load_team(slug)` and `loop_load_persona(slug)` both default to **activating for the current MCP session**:

- Content is returned immediately in the response (LLM gets it in context now)
- LOOP also remembers the activation per-session
- Future LOOP tool responses include a small `activeContext: { teams: [...], personas: [...] }` metadata stub indicating what's active
- If the LLM needs full active content back after context compression, it calls `loop_get_session_context()` to pull it explicitly

A `persist=false` parameter (deferred post-beta) will give bundled-content one-shot behavior for programmatic callers who want explicit control. No architectural rework needed to add it later — it's purely additive.

### 3. Companion tools for visibility and control

For both Persona and Team, the surface is symmetric:

- `loop_load_{team,persona}(slug)` — load and activate
- `loop_active_{teams,personas}()` — list what's currently active
- `loop_deactivate_{team,persona}(slug)` — remove one from active
- `loop_clear_active_{teams,personas}()` — clear all

Plus the shared retrieval:

- `loop_get_skill(slug)` — full content of a single skill (used both directly and by team loaders)
- `loop_get_session_context()` — full content of everything currently active in this session

## On-disk layout

```
~/.loop/
├── personas/
│   └── <slug>/
│       └── PERSONA.md           # voice_profile + description in frontmatter, voice notes in body
├── teams/
│   └── <slug>/
│       └── TEAM.md              # description + member skill slugs
└── skills/
    └── <slug>/
        └── SKILL.md             # unchanged — Teams reference these by slug
```

## Auto-injection: middle-ground approach

True automatic injection-into-every-LLM-turn is impossible under MCP's request-response shape. The practical approximation has three options:

| Approach | Behavior | Tradeoff |
|---|---|---|
| Prefix every LOOP tool response with full active content | Heavy injection | Bloats every response, couples content to all tools |
| Metadata stub in tool responses + explicit re-fetch | LLM sees what's active; pulls full content via `loop_get_session_context()` when needed | Clean responses; relies on LLM to fetch when relevant |
| Auto-load at session start only | One-shot at MCP connection open | Content fades after compression |

**Chosen: option 2 (metadata stub + explicit re-fetch).** Cleanest responses, automatic-feeling, no bloat. The `activeContext` stub keeps the LLM aware of what's loaded; `loop_get_session_context()` is the explicit recovery path.

## Consequences

**Pros:**
- Composability — Persona × Team(s) freely combinable
- Voice and capability evolve independently
- Non-developer-friendly default (session-activation, just works)
- Programmatic-explicit path preserved for the future
- Auto-injection middle ground avoids both bloat and fade
- Tool surface is symmetric and small (4 verbs per entity + 2 shared)

**Cons:**
- Two concepts instead of one
- Per-session state on the LOOP side (active_teams, active_personas)
- Build complexity ~3 hrs vs ~30 min for bundled-content-only

**Risk mitigation:**
- `loop_active_*` makes hidden state visible — users can answer "why is the LLM doing X?"
- `loop_deactivate_*` and `loop_clear_active_*` are kill switches
- Activations are bounded by MCP session lifecycle — process restart clears state (correct behavior, sessions are explicit contracts)

## Alternatives considered

- **Option A — Collapse Persona into Team** (or vice versa): rejected. Voice and toolkit are genuinely independent axes; collapsing forces rigid bindings.
- **Option C — Team is just Persona renamed**: rejected. Renaming the conflated concept doesn't fix the conflation.
- **Pure bundled-content**: rejected for end-user UX. Acceptable for programmatic callers; preserved as future `persist=false` parameter.
- **Heavy auto-injection** (prefix every response): rejected. Couples team content to every tool response, bloats output, makes responses harder to reason about.

## Related

- [ADR-0001](0001-multi-agent-orchestration-day-one.md) — multi-agent: an Agent at runtime composes Persona × Team(s)
- [ADR-0005](0005-hybrid-memory-retrieval.md) — hybrid memory retrieval: session-activation here mirrors the active-manifest pattern
- [ADR-0010](0010-on-disk-file-layout.md) — on-disk file layout: extended with `personas/` and `teams/` directories
- [ARCHITECTURE.md](../ARCHITECTURE.md) — MCP tool surface
- [DATA_MODEL.md](../DATA_MODEL.md) — Persona and Team entities
