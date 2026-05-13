# 2026-05-13 Workspace Collapse — Post-Research Notes

**Phase:** Post-research (workflow cycle phase 4 — forward-looking)
**Cycle:** Workspace restructure (Steps 1+2 of the 2026-05-13 plan)
**Commit:** `a4a08b2`
**Archive:** `/Users/slee/projects/loop-archive-2026-05-13/` (core-ts + loop-daemon + loop-parent)

---

## What shipped

- **Step 1**: archive snapshot with three directories, each preserving full `.git/` history:
  - `core-ts/` — TS code (was `loop/core/`)
  - `loop-daemon/` — Rust code as of pre-collapse, including uncommitted Day 13 watcher
  - `loop-parent/` — workspace docs + parent `.git` history (added mid-execution; see learning #1)
- **Step 2**: collapsed `loop/` into a single Rust core engine repo. Parent's `.git` replaced by daemon's; docs merged; `.claude/` gitignored; daemon README deleted (preserved in archive); parent README installed as primary.
- All 127 tests pass post-collapse (`cargo test --all`).
- Workspace import committed as `a4a08b2`.

---

## Learnings (forward-feeding)

### 1. Archive-first as a pattern for destructive workflow operations

The plan originally archived only `core/` and `loop-daemon/`. During execution I caught that this would have lost the parent workspace's `docs/`, ADRs 0001-0013, README, and parent `.git` history. Extending the archive to include `loop-parent/` was a real save — without it, the ADR series would have been destroyed.

**Apply forward:** any destructive workflow op (file moves, repo collapses, history rewrites) needs a "what is at risk that the plan didn't enumerate?" pause BEFORE execution. The cost of an extra archive is trivial; the cost of an unrecoverable artifact is permanent. This is a workflow-cycle protection that belongs in [[feedback-workflow-cycle]].

### 2. ADR 0004 (NodeJS/TypeScript language decision) is now stale

`docs/decisions/0004-language-nodejs-typescript.md` is preserved as-is in the new repo, but the language decision has changed. The engine is now Rust.

**Apply forward:** add a new ADR superseding 0004 — capturing the Rust pivot reasoning (live verification latency budget, single static binary deploy, compile-time concurrency guarantees, the Day 10 Notify race that TS would have shipped). Original 0004 stays as superseded-history.

### 3. ADR 0010 (On-disk file layout) needs updating for multi-tenancy

`docs/decisions/0010-on-disk-file-layout.md` was written assuming single-user `~/.loop/` paths. Day 14 introduces `Context` + `Storage` abstractions that re-shape the path hierarchy (`tenant > team > user > session`).

**Apply forward:** when Day 14 lands, supersede 0010 with a new ADR documenting the `Context` + `Storage` scope hierarchy and how `FilesystemStorage` resolves a `Context` to a path. Original 0010 stays as history.

### 4. Cargo.lock decision: currently gitignored, probably wrong for a binary

For a binary application (loop-daemon has `src/main.rs`), `Cargo.lock` is conventionally committed to pin dependency versions for reproducible builds. The current `.gitignore` inherits the daemon's earlier "library crate" convention which is wrong for a binary.

**Apply forward:** Day 14 should remove `Cargo.lock` from `.gitignore` and commit the current lockfile. Reproducibility matters for a deployable daemon. Single-line change + commit.

### 5. README rationalization needed in Day 14

The current `README.md` (parent's content, now primary) uses earlier strategic framing. Per the 2026-05-13 strategic update + [[project-brand-mindcraftor]], the engine is now framed as "the central brain for AI agent memory + orchestration" with verification as one wedge inside a broader cognitive substrate. README should reflect that.

**Apply forward:** Day 14 build phase should include a README update aligned with the central-brain framing.

### 6. Day 13 audit fixes carry over and intentionally stayed uncommitted

Pre-existing uncommitted state preserved on purpose:

- `M Cargo.toml` (notify=8 dep addition)
- `M src/lib.rs` (watcher module declaration)
- `?? src/watcher/` (Day 13 watcher impl with 4 audit findings pending: A1 pre-existing file replay, A2 short-read offset bug, A3 MAX_APPEND_READ stall, A4 THIRD_PARTY_LICENSES notify declaration, A5 SessionStarted gap)

**Apply forward:** Day 14 module restructure should move `src/watcher/` → `src/host/claude_code/watcher/` first. After the move, apply the audit fixes in the new location. A single commit covers restructure + audit fixes for the watcher portion.

### 7. `.claude/` gitignore policy

Currently `.claude/` contains only `settings.local.json` (Claude Code's per-project local state). Gitignored in this collapse.

**Apply forward:** revisit when (and if) we accumulate any shared Claude Code config worth tracking — e.g., project-specific slash commands at `.claude/commands/`. At that point, switch to tracking `.claude/commands/` specifically while continuing to ignore `.claude/settings.local.json`. Until then, ignore the whole directory.

### 8. docs/research merge worked cleanly — but verify in future

Daemon's `docs/research/day-10..14` + parent's `docs/research/ecc-source-patterns-*` + sentiment-research files coexisted without filename conflict. Got lucky.

**Apply forward:** when merging directories from two sources, audit filename collisions FIRST (`comm -12 <(ls source-a/) <(ls source-b/)` or similar). Won't always be so lucky.

---

## Open questions for Day 14 learn phase

These came out of the restructure and feed into the Day 14 pre-research deliverable (currently being produced by the spawned research agent):

- Does `Context` wrap `Storage`, or are they separate parameters on each function? (`Context::new(storage)` vs `fn foo(&ctx: &Context, &storage: &dyn Storage)`)
- Does `Storage` know about `Context` (i.e., `Storage` impls resolve paths from a `Context`), or is `Storage` `Context`-agnostic (caller resolves paths upstream)?
- Where does `EventSource` live: `src/engine/event_source.rs` (as a peer of `Context` + `Storage`) or `src/engine/host_interface.rs` (grouped with host-facing traits)?

The Day 14 pre-research agent is researching these against patterns from `tokio`, `tower`, `axum`, `object_store`, `opendal`, `notify`. Decisions land in the Day 14 learn-notes after the agent returns.

---

## Patterns to reuse

1. **Archive-first** for any destructive op (item 1 above)
2. `rsync -a --exclude=…` instead of `cp -a` — faster and more controllable
3. **Staging via `/tmp`** instead of mutating in-place — recoverable if a step fails
4. **Inline audit** (file counts + key paths + `cargo test`) before commit, even when the change is "just moves"
5. **Workflow-phase naming in status reports** — keeps the user oriented to where we are in the cycle

## Patterns to NOT repeat

1. Plans that enumerate only the obvious artifacts to archive — always do the "what does the plan miss?" pause
2. Conflating post-research and audit in the same agent call — keep them separate per [[feedback-dont-skip-audit-cycle]]
3. Committing without verifying file counts match the archive
4. Renaming primary README without preserving a copy of the old one for one cycle (we deleted `README.daemon.md`; if anyone needs daemon's framing, archive at `loop-archive-2026-05-13/loop-daemon/README.md`)

---

## Workflow cycle status for the restructure

| Phase | Status | Artifact |
|---|---|---|
| 1. Pre-research | ✅ done (conversational) | discussion log + plan memory |
| 2. Learn | ✅ done | `project_2026_05_13_restructure_plan.md` (memory) |
| 3. Build | ✅ done | archive + collapse |
| 4. Post-research | ✅ done | this file |
| 5. Audit | ✅ done (inline: file count match, key paths present, all 127 tests pass, git history intact) | terminal output preserved |
| 6. Commit | ✅ done | `a4a08b2` |

Next cycle begins: Day 14 (single-crate module restructure + Context/Storage/EventSource). Pre-research agent running in background; learn-notes will follow once it returns.

Related: [[feedback-workflow-cycle]], [[feedback-execute-the-plan]], [[feedback-rust-idiomatic-refactor]], [[project-2026-05-13-restructure-plan]]
