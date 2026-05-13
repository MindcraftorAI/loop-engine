# Beta Scope (revised 2026-05-13)

History: scope was originally pivoted on 2026-05-12 after the competitive
audit (see [COMPETITIVE.md](COMPETITIVE.md)). This revision adds the
**Phase A Rust daemon decision** made 2026-05-13 once it became clear
the MCP-only build is a *batch* verifier and the *live* verifier needs
a persistent daemon.

## Positioning

**Loop is a verification layer for AI agent learnings.** Sits downstream
of capture mechanisms (Anthropic Dreaming, Claude Code Auto Memory + Auto
Dream, learnings.md, everything-claude-code instincts) and runs candidates
through an anti-self-grading promotion gate. One-sentence pitch:

> "Anthropic Dreaming and Auto Memory promote patterns the model graded
> itself on. Loop's verifier requires external evidence before a learning
> is promoted."

## Architecture (current)

Two-process model:

- **`@loop/core` MCP server (TypeScript)** — already shipped. Tool-call-
  scoped surface inside Claude Code. Handles manual ingest, batch
  verification, lesson CRUD via MCP tools.
- **`loop-daemon` (Rust, in progress)** — persistent process outside
  Claude Code. Watches JSONL transcripts in real-time, runs sentiment
  classification after user turns, tails Auto Dream / Auto Memory for
  live ingest, accumulates signals across sessions. Cherry-picks
  scaffolding from `affaan-m/everything-claude-code/ecc2/` with MIT
  attribution.

Both processes coordinate via cross-process file lock on shared lesson
files at `~/.loop/lessons/<status>/<id>.md`.

See [phase-a-daemon-plan.md](phase-a-daemon-plan.md) for the full daemon
plan.

## What's shipped — Days 1-9 batch verifier (commits 1fb0b79 → c828a21)

| Area | Status |
|---|---|
| MCP server (stdio, 32 tools) | ✅ |
| Memory layer (file-canonical, FTS5 + sqlite-vec, RRF k=60, 3-axis scoring, manifest, feedback) | ✅ |
| Skills / Teams / Personas (file-canonical, session-activation per ADR-0013) | ✅ |
| Per-project session-state persistence (sha256(cwd) keyed) | ✅ |
| Lesson layer 5-status lifecycle (`pending → active → promoted \| discarded \| superseded`) | ✅ |
| Structured causal narrative + anti-self-grading promotion gate | ✅ |
| LLM-assisted causal narrative generation (Anthropic Structured Outputs) | ✅ |
| Skill audit trail (lesson-history.yaml per skill) | ✅ |
| async-mutex for read-modify-write race safety | ✅ |
| CLI: `loop mcp`, `loop setup`, `loop thumbs-up`, `loop thumbs-down`, `loop verify` | ✅ |
| Shared ingest pipeline (`CandidateLesson` + `ingestCandidate` + ingest_provenance frontmatter) | ✅ Day 1 |
| Auto Memory adapter (`loop_ingest_claude_memory`) | ✅ Day 2 |
| `loop verify` CLI (wedge / ripening classification, --strict, --json) | ✅ Day 3 |
| everything-claude-code instincts adapter (`loop_ingest_ecc_instincts`) — hard-cap speculative | ✅ Day 4 |
| Auto Dream JSONL adapter (`loop_ingest_auto_dream`) — interrupt + correction mining (batch) | ✅ Day 5 |
| Solicitor (`loop_solicit_stale_lessons`) — ACTIVE-source allowlist + forced-choice templates | ✅ Day 6 |
| Sentiment subagent core (`loop_classify_sentiment` shadow-mode on MCP, classifySentiment orchestrator) | ✅ Days 7-9 |
| Audit-fix commits for Days 2, 3, 4, 5, 6, 7-9 (13 critical findings caught and fixed) | ✅ |
| Tests: vitest 249 passing | ✅ |

## What's NEXT — Phase A daemon (Days 10-17, ~8 working days)

See [phase-a-daemon-plan.md](phase-a-daemon-plan.md) for full plan.

- **Day 10:** Cargo workspace scaffold + cherry-picked ecc2 lifecycle
- **Day 11:** Purpose-built YAML reader/writer (round-trip parity with TS)
- **Day 12:** Lesson loader + signal writer + cross-process file lock
- **Day 13:** JSONL real-time watcher
- **Day 14:** Sentiment pretrigger + Anthropic Haiku HTTP client
- **Day 15:** Attribution algorithm + orchestrator port
- **Day 16:** State holder + per-session rate limiting (Days 7-9 audit A4 fix)
- **Day 17:** End-to-end integration + dogfood readiness

Exit criteria: daemon detects a user "thanks" turn in a real JSONL and
writes `sentiment_positive` to a real lesson frontmatter without
corrupting any other field. Cross-process file lock proven safe under
concurrent write contention.

## Phase B — Validation (~2 weeks after Phase A)

- Dogfood on real coding sessions
- Iterate pretrigger regex + attribution thresholds
- Solicitor daemon-side scheduler (the query module already exists; needs
  periodic-tick wiring in the Rust daemon)
- Auto Memory file-change watcher (currently manual ingest)
- Auto Dream real-time tailing (currently batch on completed transcripts)

## Phase C — Full port (only if Phase B shows traction, ~3-4 weeks)

- Rewrite MCP server + ingest adapters in Rust
- Single Rust binary owns everything
- Archive `core/` (TS) as porting reference

## What's CUT (deferred indefinitely)

These overlap with Anthropic Dreaming / Auto Memory / Auto Dream /
everything-claude-code which now own that ground:

- Event log + tier-2 classifier subagent + reflection tier (the old "Phase 3
  cognitive architecture")
- Monorepo refactor (premature; loop-daemon as sibling crate is sufficient)
- Auto-capture from session events as a primary feature (Auto Memory + Auto
  Dream already do this; Loop's role is verification downstream)
- Capability decomposition config schema refactor (premature)
- Marketplace, hosted external-source ingestion, bundle format (no user
  base to monetize)
- `spawn_subagent` tool, deterministic primitives, ExecutionStep log

Also explicitly deferred from Days 7-9 sentiment work (audit findings B1
+ A5): calibration table refit pipeline, plural-pronoun multi-emit
attribution.

## Fallback plan if verification doesn't get traction in 60 days

Open conversation with `affaan-m` about contributing Loop's gate (or the
sentiment subagent itself) as an ECC plugin module. Cherry-picking from
their `ecc2/` keeps the codebase architecturally aligned for this path.
Distribution comes from their 140K-star surface; we contribute the IP.

## Discipline rules (locked)

1. No scope creep. New items go to a "future" list, not into this week's
   build.
2. ≤500 lines per file (Rust + TS) without strong reason.
3. Tests from the start for novel logic. Not retrofit.
4. **5-phase cycle per Day-N:** pre-research → learn → build → post-research
   → audit → commit. No phase skipped. See
   [feedback_research_first_pattern](../../../.claude/projects/-Users-slee-projects-loop/memory/feedback_research_first_pattern.md)
   and [feedback_dont_skip_audit_cycle](../../../.claude/projects/-Users-slee-projects-loop/memory/feedback_dont_skip_audit_cycle.md).
   Days 4-9 audit-skip surfaced 13 critical findings retroactively; that
   pattern does not repeat.
5. **Audit-fix commits separate from feature commits** when audit reveals
   real bugs.
6. No AGPL/GPL/SSPL deps. License-check runs in `verify` pipeline (TS:
   already wired; Rust: wired Day 10).
7. Every roadmap decision checked against the competitive audit + the
   daemon-architecture decision before committing.

## Connects to

- [phase-a-daemon-plan.md](phase-a-daemon-plan.md) — Phase A day-by-day
- [COMPETITIVE.md](COMPETITIVE.md) — verified 2026-05-12 landscape
- `core/README.md` — engine for the verifier (TS surface)
- `loop-daemon/README.md` (forthcoming Day 10) — daemon surface
