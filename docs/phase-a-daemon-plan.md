# Phase A — Rust daemon (live verification layer)

Plan locked 2026-05-13. Builds on the batch verifier shipped Days 1-9
(commits 1fb0b79 through c828a21).

**Updated 2026-05-13** after pre-research on ecc2 revealed it's a poor
parts donor (see "Cherry-pick reality" below). Plan adjusted from
"~60% lifted scaffolding" to "lift ~200 LOC + build the rest fresh."
Still ~7-8 days because the fresh work is well-trodden Rust daemon
territory.

## Why this is foundational, not Phase-3

The MCP server is tool-call-scoped: runs when the LLM invokes a tool,
dies between calls. It cannot:

- Watch JSONL transcripts as they grow
- Subscribe to user turns for real-time sentiment classification
- Persist per-session rate-limiting state
- Accumulate signals across multiple Claude Code sessions
- Trigger Auto Memory / Auto Dream ingest on file change

Days 1-9 are the BATCH verifier. Phase A is the LIVE verifier.

## Architecture

Two-process model. Existing TS MCP server stays untouched. New Rust
daemon runs alongside, both reading/writing the same lesson files via
cross-process file locking.

```
┌─────────────────────────┐   ┌─────────────────────────┐
│  Claude Code session    │   │  loop-daemon (Rust)     │
│   └─ @loop/core MCP     │   │   ├─ watches JSONL      │
│        (TS, existing)   │   │   ├─ sentiment loop     │
│                         │   │   ├─ Auto Memory tail   │
│                         │   │   └─ signal emit        │
└────────────┬────────────┘   └────────────┬────────────┘
             │                              │
             └──── both write ──────────────┘
                   ~/.loop/lessons/<status>/<id>.md
                   (cross-process file lock)
```

## Language: Rust (decision memory: project_loop_daemon_architecture.md)

Confirmed by pre-research findings — async daemon territory is well
supported in Rust; the YAML, file-watch, and locking concerns all have
mature MIT/Apache crates.

## Cherry-pick reality (2026-05-13 pre-research)

ecc2 is **NOT a parts donor**. Tightly coupled around a SQLite
`StateStore` god-object; every module reaches into it. `main.rs` is
12,570 lines (412KB monolith — exactly what `feedback_code_quality`
warns against). Most of the file names looked like daemon scaffolding
but the contents are ECC-business-specific.

**What we actually lift** (MIT-attributed):

| Source | Target | LOC | Why |
|---|---|---|---|
| `ecc2/src/session/output.rs` | `loop-daemon/src/buffer.rs` | 172 | Ring-buffer + broadcast pattern. Clean, zero deps. Verbatim. |
| `ecc2/src/session/daemon.rs:475-496` | `loop-daemon/src/pid.rs` | ~20 | `pid_is_alive` via `libc::kill(pid, 0)` with EPERM handling. |

**Pattern adaptations** (rewrites informed by ecc2, with attribution):

| Source | Target | LOC | Why |
|---|---|---|---|
| `ecc2/src/main.rs:1309-1322` | `loop-daemon/src/main.rs` (shape) | ~30 | tracing + clap + anyhow + tokio entrypoint shape. |
| `ecc2/src/config/mod.rs:493-607` | `loop-daemon/src/config.rs` (algorithm) | ~115 | Layered-merge pattern (default → global → project). Adapted to YAML. |
| `ecc2/src/notifications.rs:80-130,257-310,404-420` | `loop-daemon/src/http.rs` (shape) | ~60 | HTTP wrapper pattern. **Rewritten async with `reqwest`** — do NOT copy ecc2's sync-ureq-on-tokio pattern (it blocks the executor). |

**Built fresh** (no ecc2 prior art):

- Daemonization (fork + setsid + PID file + SIGTERM/SIGHUP handling) — use `daemonize` crate
- File watching (JSONL tail + memory dir watch) — use `notify` crate
- Cross-process file locking — use `fd-lock`
- Purpose-built YAML reader/writer for Loop's narrow frontmatter shape
- Anthropic Haiku HTTP client (no SDK) — `reqwest` + `serde_json`
- Sentiment pretrigger + 5-pass attribution + orchestrator (port from TS)
- Lesson loader + signal writer (port from TS)
- Per-session state holder + rate limiting (new — TS audit A4 gap)

## Stack corrections from pre-research

- **`reqwest 0.12` with `rustls-tls`**, not `ureq`. We're tokio-async; sync HTTP would block the executor.
- **Avoid OpenSSL transitive deps.** Stay rustls-backed throughout.
- **Skip `git2`, `rusqlite`, `cron`, `ratatui` (v1)** — pulled by ecc2's Cargo.toml but irrelevant to us. We pick deps fresh.

## MIT compliance checklist

1. `loop-daemon/THIRD_PARTY_LICENSES.md` containing the full MIT text verbatim, preserving `Copyright (c) 2026 Affaan Mustafa`
2. Per-file SPDX header on every lifted file:
   ```rust
   // Portions adapted from ecc2 (everything-claude-code)
   // Copyright (c) 2026 Affaan Mustafa — MIT License
   // Source: https://github.com/affaan-m/everything-claude-code/blob/<sha>/ecc2/src/session/output.rs
   // SPDX-License-Identifier: MIT
   ```
3. Pin the git SHA we lift from in `THIRD_PARTY_LICENSES.md` for reproducibility
4. Workspace README acknowledgement: "Built on ideas from affaan-m/everything-claude-code (MIT)"
5. No upstream-change obligation, no copyleft propagation — MIT terms

## Workflow (per locked cycle)

For EVERY day below, ALL 5 phases apply. No phase skipped. This is the
lesson from `feedback_dont_skip_audit_cycle` — Days 4-9 audit gap
surfaced 13 critical findings retroactively; pattern does not repeat.

1. **Pre-research** — spawn agent on the specific problem
2. **Learn** — synthesize into design notes
3. **Build** — code AND tests interleaved
4. **Post-research** — agent hunts bugs in what was just built
5. **Audit** — critical review; audit-fix is a SEPARATE commit when issues are real
6. **Commit** at end of Day-N

## File-size + dependency discipline

- ≤500 lines per Rust source file. Same as TS.
- No AGPL/GPL/SSPL. Every Cargo dep verified MIT/Apache.
- License-check wired into `cargo verify` Day 10.

## Day-by-day (revised)

### Day 10 — Workspace scaffold + lift output.rs + daemon skeleton

- Cargo workspace at `loop-daemon/`
- `loop-daemon/THIRD_PARTY_LICENSES.md` with MIT text + Affaan Mustafa attribution + lifted-from SHA
- Lift `output.rs` → `buffer.rs` verbatim (172 LOC) + SPDX header
- Lift PID-alive helper → `pid.rs` (~20 LOC) + SPDX header
- Build fresh: daemonize entry (`daemonize` crate), SIGTERM/SIGHUP handlers, structured logging (`tracing` + `tracing-subscriber`), config loader skeleton (layered-merge pattern adapted to YAML)
- `loop-daemon run` detaches, logs heartbeat every N seconds, shuts down cleanly on signals
- Audit cycle

### Day 11 — Purpose-built YAML reader/writer

- Custom parser sized to Loop's frontmatter shape (~10 fields, no anchors, no comments)
- Sidesteps the serde_yaml deprecation issue
- Round-trip tested against TS-written lesson fixtures
- TS and Rust must produce byte-identical output when given identical inputs (defined tolerance)
- Audit cycle

### Day 12 — Lesson loader + signal writer with cross-process lock

- Port `getLessonById` and `recordLessonSentimentSignal` semantics
- Cross-process file lock via `fd-lock` (advisory `flock`)
- Integration test: TS process + Rust process both writing `external_signal_sources`, verify no race, no lost updates
- Audit cycle

### Day 13 — JSONL watcher

- Build on `notify` crate (no ecc2 prior art here)
- Tail `~/.claude/projects/<encoded>/<session>.jsonl` as it grows
- Parse appended events, emit normalized `UserTurnEvent`
- Handle log rotation, file truncation, malformed lines (Day 5 lesson)
- Audit cycle

### Day 14 — Sentiment pretrigger + Anthropic Haiku client

- Port `SENTIMENT_PRETRIGGER` regex (with Days 7-9 audit fixes)
- Hand-roll Anthropic Haiku 4.5 HTTP client via **async `reqwest 0.12`** + `serde_json`
- Use HTTP wrapper pattern from ecc2's notifications.rs as structural reference (with attribution comment) — but ASYNC, not sync
- Pretrigger short-circuit before HTTP call
- Audit cycle

### Day 15 — Attribution algorithm + orchestrator port

- Port 5-pass attribution from TS `src/sentiment/attribution.ts`
- Port `classifySentiment` orchestrator (with Days 7-9 audit A2 fix already applied — attribution-abstain skips emission)
- Hazard auto-abstain (Days 7-9 audit A3 fix)
- Audit cycle — specifically exercise the attribution-disagreement path

### Day 16 — State holder + per-session rate limiting

- Use the lifted `buffer.rs` ring-buffer pattern for per-session output windows
- Per-lesson sentiment rate limiting (Days 7-9 audit A4: documented but unimplemented in TS; lives in the daemon by design)
- Survives across watcher events; cleared on session end
- Audit cycle

### Day 17 — End-to-end integration + dogfood readiness

- Daemon runs, watches a test JSONL, sentiment fires, lesson gets `sentiment_positive`
- Cross-process file-lock test: TS MCP server + Rust daemon both writing concurrently
- Manual smoke test on real user transcripts
- Audit cycle

## Phase A exit criteria

- All 7 days shipped, each with audit pass
- 100% verify pipeline green (typecheck/lint/format/tests/license)
- Integration test: daemon detects a user "thanks" turn in a real JSONL and writes `sentiment_positive` to a real lesson's frontmatter without corrupting any other field
- Cross-process file lock proven safe under concurrent write contention
- `loop daemon start|stop|status` CLI commands working

## Phase B — Validation (~2 weeks after Phase A)

- Dogfood on real coding sessions
- Iterate pretrigger regex + attribution thresholds
- Solicitor daemon-side scheduler (the query module exists in TS already)
- Auto Memory file-change watcher (currently manual ingest)
- Auto Dream real-time tailing (currently batch on completed transcripts)

## Phase C — Full port (only if Phase B shows traction, ~3-4 weeks)

- Rewrite MCP server + ingest adapters in Rust
- Single binary owns everything; archive `core/` (TS) as porting reference
- Anthropic Rust SDK if available, else continue hand-roll

## Out of scope for Phase A

- TUI (`loop daemon status` returns plain text; ratatui is Phase B or C)
- Worktree management (ecc2 has it; LOOP doesn't need it)
- Distribution via brew/scoop/cargo install (Phase B at earliest)
- Calibration table refit pipeline (Days 7-9 audit B1 — deferred)
- Plural-pronoun multi-emit attribution (Days 7-9 audit A5 — deferred)

## Risk register

| Risk | Likelihood | Mitigation |
|---|---|---|
| Audit-cycle compression returns under build momentum | High (the pattern I just fell into) | Daily commits FOLLOW post-research + audit, not precede them. Memory `feedback_dont_skip_audit_cycle` loaded. |
| Cross-process file lock unreliable on macOS/Linux mixed | Low | Day 12 integration test exercises both real OS filesystems |
| TS YAML and Rust YAML drift on round-trip | Med | Day 11 fixtures, byte-comparison tests against TS output |
| Anthropic Haiku rate-limit on heavy dogfood | Med | Phase B observation; can swap to Sonnet or local model |
| Daemonize crate edge cases on macOS launchd | Low-Med | Test detach behavior Day 10; fall back to manual fork+setsid if needed |
| ecc2 cherry-pick yielded less than expected | RESOLVED | Pre-research caught it; plan revised before any Day 10 commits |

## Connects to

- [project_loop_daemon_architecture.md](../../../.claude/projects/-Users-slee-projects-loop/memory/project_loop_daemon_architecture.md) — decision memory
- [feedback_dont_skip_audit_cycle.md](../../../.claude/projects/-Users-slee-projects-loop/memory/feedback_dont_skip_audit_cycle.md) — workflow discipline
- `docs/BETA_SCOPE.md` — narrowed scope this fits inside
- `docs/COMPETITIVE.md` — ecc2 toolchain alignment
- `docs/research/sentiment-design-rules.md` — rules the daemon must enforce
