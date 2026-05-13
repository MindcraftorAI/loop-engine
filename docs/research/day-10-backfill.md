# Day 10 backfill — workspace scaffold + ecc2 cherry-pick

**Backfilled 2026-05-13.** Day 10 had a pre-research agent (the ecc2
cherry-pick map) but missed the separate learn + post-research
artifacts. Adding them retrospectively for the record.

## Pre-research (existed)

The ecc2 cherry-pick research agent (referenced earlier in the workspace
session). Delivered:
- File-by-file recommendation: lift `output.rs` (172 LOC), lift the
  `pid_is_alive` helper (~20 LOC), pattern-adapt main.rs entrypoint
  shape, config layered-merge, notifications HTTP wrapper.
- Skip list: all of ecc2/session/manager.rs (8190 LOC), store.rs
  (7109 LOC), worktree/, tui/, the bulk of main.rs (12,570 LOC).
- Cargo.toml deps surveyed for MIT/Apache compliance — clean.
- MIT compliance checklist: per-file SPDX header on lifted files,
  THIRD_PARTY_LICENSES.md preserving upstream copyright, README
  acknowledgement.

The key finding: ecc2 is much less of a parts donor than expected.
Tightly coupled around a SQLite `StateStore` god-object. We lift ~200
LOC and build the rest fresh.

## Learn notes (backfilled)

### Architectural decisions made before Day 10 code

- **Two-process model.** Existing TS MCP server stays untouched. New
  Rust daemon runs alongside, both reading/writing the same lesson
  files via cross-process file locking (sidecar pattern landed Day 12).
- **Language: Rust** (decided in conversation, not by research agent).
  Footprint + single binary + ratatui-future + ecc2 toolchain
  alignment. With continuous AI dev pair, language-friction concerns
  that favored Go dissolved.
- **Cargo workspace at `loop-daemon/` as sibling to `core/`.** Not a
  workspace member of core — separate crate, separate git repo,
  separate version. Coexists by file conventions only.

### Module split for Day 10 scaffold

- `src/main.rs` — clap entrypoint, subcommand dispatch (Run/Status/Stop)
- `src/cli.rs` — clap definitions (separate so subcommand handling
  doesn't bloat main)
- `src/lib.rs` — library root (re-exports for tests + integration test
  consumers)
- `src/lifecycle.rs` — PID file, signal handlers, shutdown coordinator,
  heartbeat loop
- `src/observability.rs` — tracing setup (stderr for foreground, JSON
  to file for detached)
- `src/config.rs` — layered YAML config (pattern from ecc2, adapted
  to ~/.loop/config.yaml shared with TS)
- `src/paths.rs` — XDG / ~/.loop / ~/.claude resolution mirroring
  TS-side `paths.ts`
- `src/buffer.rs` — LIFTED from ecc2/session/output.rs (verbatim with
  SPDX header)
- `src/pid.rs` — LIFTED pid_is_alive helper from ecc2

### Discipline guardrails locked Day 10

- LICENSE file (MIT) + THIRD_PARTY_LICENSES.md with affaan-m
  attribution + pinned ecc2 SHA (`9a5ed32`)
- `cargo verify` script: build + lint + fmt + test + license-check
- File-size ≤500 lines per Rust source file (matches TS discipline)
- No AGPL/GPL/SSPL deps; verified for every direct + transitive

## Post-research (backfilled)

### What we learned building Day 10

1. **ecc2's daemon isn't actually a daemon.** Their "daemon" is a
   foreground tokio::main { loop { } } left to systemd. No fork, no
   PID file, no signal handling, no graceful shutdown. We have to
   build all of that fresh, which is straightforward Rust work using
   the `daemonize` crate — but a useful realization: ECC's runtime
   model is "shell-supervisor" not "self-managed daemon."

2. **The `daemonize` crate has its own PID file management** —
   conflicted with our `lifecycle::write_pid_file`. Initial Day 10
   wrote the PID twice (once via daemonize, once via our code).
   Audit caught it; fix: drop `daemonize.pid_file()`, let our
   `write_pid_file` own the PID file entirely. Single source of truth.

3. **`tokio::sync::Notify` is edge-triggered**, not sticky. A signal
   that fires before any waiter has called `notified()` is lost.
   Audit caught this in the daemon shutdown path: SIGTERM arriving
   between `install_signal_handlers` and `heartbeat_loop` registering
   would hang. Fix: `tokio_util::sync::CancellationToken` (sticky).

4. **Cargo dep audit must include transitive licenses.** Surface-level
   "MIT/Apache" is not enough; need to check what each direct dep
   pulls in. `cargo tree` is the right tool. We confirmed no AGPL/GPL/
   SSPL anywhere in the dep graph.

5. **Test env-var mutation is a parallelism hazard.** Initial Day 10
   tests touched LOOP_HOME with per-module `static ENV_LOCK`. Tests
   in different modules raced because each had its own lock. Day 12
   fixed by promoting ENV_LOCK to a crate-wide `pub static` in paths.rs.

### What this implied for Day 11+

- The signal-shutdown semantics matter for any code path that waits
  on cancellation. Use `CancellationToken`, not `Notify`. Carried
  forward into Day 12 + 13 code.
- File-system layout decisions (XDG, ~/.loop subdirs) are stable;
  Day 11+ code uses `paths::*` helpers without re-deciding.
- The "verify pipeline" pattern (typecheck + lint + fmt + test +
  license) is the standard pre-commit gate. Every subsequent day
  passes through the same gate.

### What the Day 10 audit caught that pre-research should have

The signal-arrives-before-waiter race (`Notify` vs `CancellationToken`)
is a known async-Rust pattern. Pre-research that explicitly asked
"how do we handle signal-driven shutdown safely in async Rust?" would
have surfaced `CancellationToken` as the standard answer. The
ecc2-cherry-pick agent didn't cover this because ecc2 didn't HAVE
signal handling for us to lift.

Going forward: pre-research questions need a "concurrency / consistency"
checklist item for any module that touches async + signals or
async + file I/O.
