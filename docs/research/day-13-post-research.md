# Day 13 post-research — what we learned

**Distinct from audit** (which asks "is THIS correct?"). Post-research
asks "what did we LEARN from building the watcher that the next day's
pre-research must know, and which open questions just became
decidable?"

## New knowledge surfaced during Day 13

1. **`spawn_watcher` takes ONE directory, not "all of
   `~/.claude/projects/`."** The pre-research §G decision was "watch
   ALL project dirs," but the implementation in
   `src/watcher/runner.rs:47` accepts a single `dir: PathBuf`. The
   surface we shipped is a *primitive* — multi-project orchestration
   (one `WatcherHandle` per project + a future re-scan as new project
   dirs appear) is left to a higher-level supervisor. The "watch all"
   policy decision was deferred without making the deferral explicit.
   Day 14/17 has to make this call.

2. **`SessionStarted` is fired from a `Create` *file* event, not from
   a directory rescan.** `runner.rs:144` emits `SessionStarted` only
   when the path is unknown AND the notify event kind was `Create`.
   That means a session JSONL that exists *before* `spawn_watcher`
   attaches will NEVER produce a `SessionStarted` — its first event
   on the channel will be the first `UserTurn` (or nothing, if the
   user is idle). This is a meaningful contract: downstream code
   must not assume `SessionStarted` precedes every session's events.

3. **The `new_at_eof` constructor in `cursor.rs:48` exists but isn't
   currently called from the runner.** Every cursor the runner
   creates uses `new_at_start` (`runner.rs:147`). The "tail-from-now"
   decision in the learn note applies only to *daemon-startup
   scanning of pre-existing files*, which is itself out of scope for
   Day 13 (the runner only sees files via notify after attach). Day
   14/17 wiring that does an initial directory scan must explicitly
   choose between `new_at_eof` (skip backlog) and `new_at_start`
   (replay everything).

4. **Partial-line preservation re-reads, doesn't buffer.** The
   trailing-fragment design (`cursor.rs:147-163`) reports
   `fragment_len` and the runner advances `cursor.offset` to *exclude*
   the fragment (`runner.rs:201`). So the next classify sees
   `stat.size > offset` again and re-reads the fragment plus
   anything new. This is simpler than buffering in memory and works
   because Claude Code flushes per write — but it means each
   straddling event causes ~one extra `read()` syscall. Negligible
   at observed rates.

5. **`MAX_APPEND_READ = 1 MB` (`runner.rs:33`) is a per-event chunk
   cap, NOT a per-file cap.** A 25 MB file that grows by 5 MB in one
   notify event will be read 1 MB at a time across five subsequent
   classify cycles. Each cycle currently fires only when a fresh
   notify event arrives, so a slow consumer could fall behind a
   bursty writer until a new notify event re-triggers the loop.
   Acceptable today (FSEvents fire on every flush), worth noting for
   Day 14's backpressure model.

6. **Cursor state is purely in-memory.** `BTreeMap<PathBuf,
   FileCursor>` in `run_loop` (`runner.rs:110`). Nothing is
   serialized. Daemon restart = all cursors lost = next observation
   of each file effectively becomes a fresh "tail-from-now" via the
   first notify event that arrives. The pre-research §G open
   question on replay-vs-tail was answered "tail-from-now" but the
   *persistence* angle (what happens across daemon restarts) was
   never raised — and we shipped no persistence.

7. **The notify event-kind classifier collapses `Other`.**
   `classify_event_kind` (`runner.rs:96`) maps anything not
   `Create/Modify/Remove` to `Other`, and `handle_change` treats
   `Other` identically to `Modify` (synthesizes nothing, just
   reclassifies the cursor). FSEvents on macOS occasionally emits
   ambiguous `Modify(Any)` and `Access(*)` events; both fall into
   the "just re-stat" bucket. This is correct by construction but
   wasn't called out in the pre-research.

## What this means for Day 14 (sentiment pretrigger + Haiku client)

- **Channel contract:** Day 14 consumes
  `tokio::sync::mpsc::UnboundedReceiver<WatcherEvent>`. The producer
  is `spawn_watcher` (`runner.rs:47`); the receiver half is what
  Day 14 owns. Events are interleaved across sessions on a SINGLE
  channel.
- **Cross-session interleaving is real.** The runner uses one
  `BTreeMap` and one channel for all files in the watched dir.
  Sentiment classification per turn is independent — interleaving
  is fine — but any state Day 14 holds (rate limiter, recent-turn
  ring) MUST be keyed on `session_id`, not global.
- **`MAX_APPEND_READ` is upstream of Day 14's queue.** A 5 MB
  burst translates to one
  `WatcherEvent` per real user turn (~one per a few KB), not
  thousands of events. Day 14 doesn't need its own chunking; just
  a bounded sentiment queue with `try_send` is plenty.
- **`ParseError` events should be logged + counted, not surfaced.**
  `PARSE_ERROR_REPORT_EVERY = 5` (`events.rs:80`) already aggregates;
  Day 14 just feeds `metrics::counter!("watcher.parse_errors")` and
  moves on.
- **`cc_version` is the shape-drift tripwire.** When the field is
  missing it defaults to `"unknown"` (`parser.rs:155`). Day 14
  should `warn!` on any value outside the known 2.1.x range so
  schema drift surfaces immediately.

## What this means for Day 15 (attribution algorithm port)

- **`UserInterrupt.interrupted_assistant_text` is wired as
  `None`** (`parser.rs:181`). Day 15's correction-window mining
  needs a per-session ring buffer of recent assistant turns. The
  watcher does NOT track assistant turns at all today — they're
  filtered at parser step 1.
- **Day 15 should own the lookback buffer**, not the watcher.
  Rationale: the watcher's job is "user turns + interrupts +
  lifecycle." Buffering assistant text inflates watcher memory and
  couples it to a downstream consumer's window. A separate stage
  (a "context tracker") that subscribes to a parallel
  `assistant_text` channel from a *second* parser pass is cleaner.
  Day 15 pre-research needs to design this.
- **`parent_uuid` traversal probably can't recover interrupted
  text from the watcher's current outputs.** `parentUuid` points
  to the prior event by UUID, but the watcher discards
  non-user events. Day 15 either needs (a) a watcher extension
  that also emits assistant-text events, or (b) a separate JSONL
  re-read keyed on the event_uuid → parentUuid chain. (b) is the
  simpler port — it matches the TS reference.

## What the Day 14 pre-research should explicitly cover

1. **Daemon-restart cursor persistence.** Do we checkpoint
   `(path, inode, offset)` to disk on a timer, or accept the loss?
   If we accept loss, document explicitly that restarts skip
   anything appended while down.
2. **Sleep/wake replay policy.** Learn note locked "accept loss,"
   but on wake the OS may fire one synthetic notify event per
   modified file — we'll read forward from the pre-sleep offset
   and naturally catch up. Verify empirically that FSEvents does
   this. If not, we lose everything written during sleep silently.
3. **Multi-project supervisor.** `spawn_watcher` watches one dir.
   What spawns N of these for N project dirs under
   `~/.claude/projects/`, and what detects when a new project dir
   appears? (Project dirs come and go as the user runs `cd`-changes
   into new repos.) This must land before Day 17 wiring.
4. **Pre-existing JSONLs at startup.** Use `new_at_eof` (skip
   backlog) or `new_at_start` (replay)? Recommendation:
   `new_at_eof` for sentiment, but make it a config knob — Day 15
   attribution may want backfill on first run.
5. **Multiple terminal windows on the same project.** Does Claude
   Code share ONE JSONL across windows (concurrent writes — torn
   lines possible) or one-per-window (separate session_id)? Day 5
   research suggested one-per-session; verify with two terminals
   in the same project.
6. **Sentiment queue policy on overload.** `try_send` with a
   bounded queue + drop-with-counter, or unbounded + Haiku-side
   rate limit? Latency budget says drop-old-not-new on overload.
7. **`SessionStarted` is not guaranteed.** Documented at point 2
   above. Day 14 must not gate any setup on receiving it.

## What this means for Days 16-17

- **Rate limiter (Day 16) state must be per-session.** The runner
  already routes by `session_id` in every event; Day 16 can index
  directly without re-deriving.
- **Wiring (Day 17) needs a project-dir supervisor.** Single
  `spawn_watcher` won't suffice unless we land the multi-project
  decision (#3 above) earlier.
- **The `WatcherHandle` must be held by the daemon lifecycle
  struct.** Dropping it kills the FSEvent stream silently
  (`runner.rs:38`). Day 17 wiring needs an explicit `Vec<WatcherHandle>`
  on the daemon state.

## What we got wrong (architectural-shape, not bugs)

1. **`spawn_watcher` should probably have been `spawn_watcher(dirs:
   Vec<PathBuf>)` or a `WatcherSupervisor` builder.** Shipping the
   single-dir primitive forces every caller to re-implement
   multi-project orchestration. The learn note locked "watch all
   project dirs" but we built the lower-level piece without the
   supervisor on top. Day 14/17 will rebuild it.
2. **No `SessionStarted` on attach to existing files.** Tying
   `SessionStarted` to the `Create` notify event means downstream
   code that wants a per-session init hook needs a separate
   "session discovered" event. We should have emitted
   `SessionStarted` for any cursor we initialize, regardless of
   discovery mechanism.
3. **Cursor state ephemeral with no persistence story documented.**
   Should have at least written a "no-persistence, accept-loss"
   note in the pre-research §G open questions. The omission means
   Day 14 has to re-derive the decision under pressure.
4. **No `assistant_text` channel.** The interrupt lookback Day 15
   needs is foreseeable from the pre-research §C event-shape
   work. Emitting it from Day 13 would have been a small
   addition; deferring forces a parser revisit in Day 15.
