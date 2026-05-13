# Day 13 learn notes — JSONL watcher design

Synthesizes the Day 13 pre-research deliverable into the design we're
about to build. Locked before any code lands.

## Locked decisions

### Crates
- `notify` 8.2.0 + raw `Watcher` (no debouncer). Confirmed FSEvents on
  macOS with `latency: 0.0` — true real-time event delivery.
- **License flag:** `notify` is CC0-1.0 (first non-MIT-or-Apache dep).
  Permissive, SPDX-recognized, satisfies the no-AGPL/GPL/SSPL rule.
  Add a note in `THIRD_PARTY_LICENSES.md` calling out the exception.
- No `linemux`, no `notify-debouncer-full` (latency budget conflict
  with sentiment-loop's <100ms target).

### Module split
```
src/watcher/
├── mod.rs       — public API barrel + spawn entrypoint
├── events.rs    — WatcherEvent enum + supporting types
├── cursor.rs    — FileCursor + tail-as-it-grows state machine
├── parser.rs    — JSON-line → WatcherEvent extraction
└── runner.rs    — notify-callback bridge + main read loop
```

### `WatcherEvent` enum (5 variants, locked)

`UserTurn`, `UserInterrupt`, `SessionStarted`, `SessionEnded`,
`ParseError`. Field shape per pre-research §E. Public API for Day 14
consumers.

### Filter chain for "real" user turn
Lifted from `core/src/lessons/ingest/auto_dream.ts` (Day 5 audit B3):
1. `type == "user"`
2. `isMeta != true`
3. `isSidechain != true` (defense in depth — caught in pre-research H7)
4. Content not a `tool_result` array
5. Text not matching `^\[Request interrupted` → emit as separate `UserInterrupt`, not `UserTurn`
6. Text not wrapped in `<command-name>...</command-name>`

### Channel
`tokio::sync::mpsc::UnboundedSender<WatcherEvent>`. Notify's callback
is sync; bounded `blocking_send` would stall it. Cap upstream by
JSONL append rate (~tens/sec peak per session).

### `FileCursor` state per file
```
{ path, session_id, inode, offset, last_size, last_mtime_ms, parse_error_count }
```

### State machine semantics
- Rotation: `stat.inode != cursor.inode` → reset offset to 0
- Truncation: `stat.size < cursor.offset` → reset offset to 0
- No change: `stat.size == cursor.offset` → no-op (metadata event)
- Append: read `cursor.offset..stat.size`, parse, emit, advance
- Partial line at tail: if final byte isn't `\n`, don't advance offset
  past last newline. Trailing fragment is buffered until next event.

## Open questions → decisions made (flag if you disagree)

1. **Replay vs tail-from-now on startup?** → **Tail-from-now.**
   Initial cursor offsets = current EOF. Sentiment is forward-looking;
   replaying 25MB × N sessions burns budget without value.
2. **Watch start-cwd only OR all of `~/.claude/projects/`?** →
   **All.** Cheap (per-dir watch is one inotify/FSEvent stream),
   captures cross-project user activity. Filter downstream by `cwd`
   field from each event.
3. **Subagent transcripts (`<session>/subagents/agent-*.jsonl`)?** →
   **Skip.** They're internal agent runs, not user turns.
   `RecursiveMode::NonRecursive` excludes them.
4. **ParseError handling?** → **Log + counter, never escalate.**
   Claude Code occasionally writes mid-flush; we tolerate. Emit one
   `ParseError` event per N failures (counter reset on success) to
   avoid spam.
5. **Sleep/wake gap?** → **Accept loss.** FSEvents during sleep are
   lost (notify uses `kFSEventStreamEventIdSinceNow`). On wake, our
   re-stat will see size > offset → we read the appended chunk
   (post-sleep events backfill naturally). Pre-sleep gap stays lost.

## In scope for Day 13

- The `watcher/` module ships.
- File watching, line parsing, event emission.
- Per-file cursor state.
- Test coverage:
  - FileCursor state transitions (rotation, truncation, append, partial)
  - Parser filters (isMeta, tool_result, interrupt sentinel, sidechain)
  - WatcherEvent emission shape
  - Integration: write to a JSONL file in a temp dir, observe events
    on the channel

## Out of scope for Day 13 (deferred)

- The actual sentiment classifier (Day 14)
- LLM HTTP calls (Day 14)
- Attribution algorithm port (Day 15)
- Per-session rate limiting (Day 16) — but the watcher's per-file
  state structure should not preclude adding rate-limit state later
- Wiring the watcher into the daemon's `run_body` (probably Day 17)

## Risks I'm accepting (from pre-research §H)

- **CC version drift:** acknowledged. `version` field is captured in
  `UserTurn` events; log a warning if we see != "2.1.x" for now.
  Daily heuristic, not load-bearing.
- **Daemon + TS MCP server both reading the same JSONLs:** harmless
  (reads don't conflict). Downstream dedup is Day 14's problem.
- **Notify Watcher Drop semantics:** the `RecommendedWatcher` must
  be held in a long-lived struct. The runner module's `Watcher` will
  be held by the daemon's lifecycle. Test that drops don't silently
  kill the stream.

## Dependency graph for the build

1. `events.rs` — pure types, no deps. Build first.
2. `parser.rs` — depends on `events.rs`. Build second.
3. `cursor.rs` — depends on `events.rs` + `parser.rs`. Build third.
4. `runner.rs` — depends on all of the above + notify + tokio. Build last.
5. `mod.rs` — barrel. Built incrementally as modules land.

Tests at each level. Integration test at runner level (write to a
JSONL, assert events arrive).

## What audit should check (advance flag)

- Notify callback runs sync; the bridge to async via mpsc must NOT
  block the callback thread (`try_send` semantics, unbounded queue OK).
- The partial-line guard at tail correctly handles a write that
  straddles a newline boundary.
- Rotation detection: a file replaced via atomic rename keeps the
  same path but new inode — must be detected.
- The CC version field is captured but not gated on (we emit events
  even for "unknown" versions, just log).
- Sidechain filter is applied (defense-in-depth, not just isMeta).
- Encoded-path quirk: project dir name is lossy, resolve cwd from
  the JSONL `cwd` field, not by reversing the dir name.
