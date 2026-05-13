# loop-daemon

The **live verifier** half of [Loop](../README.md). Persistent Rust
daemon that watches Claude Code session transcripts, runs sentiment
classification on user turns, and emits external signals to the lesson
layer in real time.

Sits alongside the existing TypeScript MCP server in
[`core/`](../core/). Both processes coordinate on the same lesson files
via cross-process file locking.

Status: **Phase A in progress** — see
[../docs/phase-a-daemon-plan.md](../docs/phase-a-daemon-plan.md) for
day-by-day plan.

## Quickstart (dev)

```sh
cargo build
cargo run -- run        # start daemon (detaches; logs to ~/.loop/logs/daemon.log)
cargo run -- status     # report uptime + PID
cargo run -- stop       # send SIGTERM
```

## Layout

```
src/
├── main.rs           # binary entry — clap dispatch
├── cli.rs            # subcommand definitions
├── lib.rs            # library root (re-exports for tests)
├── lifecycle.rs      # daemonize, PID file, signal handlers
├── buffer.rs         # LIFTED from ecc2 — ring buffer + broadcast (see THIRD_PARTY_LICENSES.md)
├── pid.rs            # LIFTED from ecc2 — pid_is_alive helper
├── config.rs         # layered ~/.loop/config.yaml loader (pattern adapted from ecc2)
├── paths.rs          # XDG / ~/.loop / ~/.claude resolution
└── observability.rs  # tracing-subscriber init
```

## License

MIT. See `LICENSE`. Cherry-picked code from
`affaan-m/everything-claude-code` is preserved under MIT terms with full
attribution; see [`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md).

## Dependency discipline

No AGPL / GPL / SSPL. Every Cargo dep verified MIT or Apache-2.0.
