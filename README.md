# Loop

**Verification layer for AI agent learnings.**

Anthropic Dreaming, Claude Code Auto Memory + Auto Dream, and OSS kits like `everything-claude-code` already capture patterns from your sessions automatically. They promote what *the model graded itself on*.

Loop is what sits between capture and commit: a **promotion gate** that requires external evidence before a learning becomes permanent.

```
[ Auto Memory / Dreaming / instincts / learnings.md ]  ←  capture candidates
                          ↓
                   [ Loop's verifier ]                  ←  this layer
                          ↓
              ┌─ admitted lessons (audited)
              └─ rejected (with reason)
```

Brings the same anti-self-grading wedge published research uses (Reflexion-derived structured narrative + Voyager-derived external verification) — but locally, MIT, and composable with whatever capture mechanism you already have.

## What Loop actually does

| Feature | What it gives you |
|---|---|
| **Anti-self-grading gate** | A candidate lesson can't promote unless: causal narrative is non-speculative, age ≥ filesystem birthtime threshold (not just frontmatter — can't be backdated), at least one external signal source recorded, zero thumbs-down. Hermes-style self-grading is mechanically prevented. |
| **Structured causal narrative** | Every lesson has `trigger / failure_mode / correction / confidence (observed | inferred | speculative) / evidence_refs / generated_by`. Speculative narratives can't be the sole basis for promotion. |
| **Tamper-proof age** | Uses `max(filesystem birthtime, ctime, frontmatter created_at)` so a backdated frontmatter can't bypass the time floor. |
| **File-canonical, local-first** | All lessons at `~/.loop/lessons/<status>/<id>.md`. Your sqlite + vec store is one file you can `cat`. Inspection-friendly by design. |
| **Hybrid retrieval (memory side)** | FTS5 + sqlite-vec via RRF (k=60) + 3-axis scoring (similarity × recency × importance). |
| **MCP-native** | 32 MCP tools. Plugs into Claude Code today; per-host wrappers can extend to Cursor, ChatGPT Apps, etc. later. |
| **Real-time daemon (in progress)** | Rust daemon watches JSONL transcripts and runs sentiment classification after every user turn. Continuous signal accumulation across sessions. Built on cherry-picked scaffolding from `affaan-m/everything-claude-code/ecc2` with MIT attribution. |

## How it composes

Loop is designed to sit **on top of** existing capture mechanisms, not replace them:

| Existing tool | Loop's role |
|---|---|
| **Anthropic Dreaming** (Managed Agents) | Dreaming surfaces patterns; Loop verifies via the gate before adding to permanent memory |
| **Claude Code Auto Memory + Auto Dream** | Auto Memory captures candidates into `~/.claude/projects/<project>/memory/*.md`; Loop ingests them and runs the gate |
| **`learnings.md` pattern** | Same: candidates flow in, gate decides what gets promoted |
| **`everything-claude-code` instincts** | Instincts ship to skills; Loop adds verification before they harden |

Roadmap: a `loop_ingest_claude_memory` adapter so Loop becomes immediately useful to every Claude Code user without changing how they already work.

## Status

**Batch verifier shipped** (Days 1-9, 249 tests passing, MIT):
- Memory layer: file-canonical YAML + SQLite + sqlite-vec + FTS5 + RRF + 3-axis scoring
- Lesson layer: 5-status lifecycle, structured causal narrative, promotion gate with all the guards listed above
- Four ingest sources: Auto Memory adapter, everything-claude-code instincts adapter, Auto Dream JSONL interrupt mining, `loop verify` CLI for arbitrary markdown
- Solicitor: surfaces stale unsignaled lessons for active user-asking
- Sentiment subagent (orchestrator + 5-pass attribution + asymmetric thresholds + hazard auto-abstain — shadow mode on MCP surface)

**Live verifier — Phase A in progress (Days 10-17):**
- Rust daemon (`loop-daemon/`) cherry-picking scaffolding from ECC's `ecc2/` with MIT attribution
- Real-time JSONL watching → sentiment classification → signal emission
- Cross-process file locking with the TS MCP server
- See [docs/phase-a-daemon-plan.md](docs/phase-a-daemon-plan.md)

**Phase B (validation, ~2 weeks):** dogfood, iterate thresholds, wire the solicitor scheduler, Auto Memory file-watcher trigger, Auto Dream real-time tailing.

**Phase C (full Rust port, conditional on Phase B traction).**

**Deferred indefinitely:** event log + tier-2 classifier + reflection tier. Anthropic Dreaming + Auto Memory + Auto Dream cover that ground. Loop's wedge is downstream of capture, in verification.

## Docs

- [Architecture](docs/ARCHITECTURE.md) — engine internals
- [Data Model](docs/DATA_MODEL.md) — entities, scopes
- [Beta Scope](docs/BETA_SCOPE.md) — what's shipped, Phase A daemon, Phase B + C
- [Phase A daemon plan](docs/phase-a-daemon-plan.md) — Rust daemon day-by-day
- [Competitive Landscape](docs/COMPETITIVE.md) — verified 2026-05-12
- [Decision Log](docs/decisions/) — ADRs

## Layout

```
loop/                  # this workspace
├── docs/              # design documentation
├── core/              # batch verifier — TypeScript MCP server (its own git repo)
└── loop-daemon/       # live verifier — Rust daemon (Phase A, in progress)
```

## License

MIT for both `core/` and `loop-daemon/`. See [ADR-0009](docs/decisions/0009-open-core-licensing.md).
`loop-daemon/` includes cherry-picked code from `affaan-m/everything-claude-code/ecc2/`
under MIT terms with full attribution.
