# Day 12 post-research — what we learned

**Distinct from audit.** Audit asked "is THIS correct?"; this asks
"what did we LEARN from building it that we didn't know going in,
and what should Day 13 pre-research cover?"

## New knowledge surfaced during Day 12

1. **`fd-lock`'s lock-on-data-file pattern is racy under atomic rename.**
   Lock + rename + unlock + new caller's open + new caller's lock can
   all complete with both threads thinking they had mutual exclusion —
   in reality they locked different inodes. The audit traced this
   explicitly: the saving grace was the re-read-inside-lock that picked
   up the latest content, but the flock semantic itself was a lie.
   FIXED via sidecar pattern.

2. **The sidecar approach has a small additional cost.** One extra
   file per lesson (the `.lock`). They're zero-byte and never read,
   but they multiply the file count by 2x. Acceptable but worth
   noting. Future: could use a single global lock file if per-lesson
   contention is rare (it is — most signal writes don't collide).

3. **`fd_lock 4.x` is actively maintained and uses real OS flock.**
   Verified by reading `fd-lock-4.0.4/src/sys/unix/*.rs` — it's a thin
   wrapper around `flock(2)` on Unix and `LockFile` on Windows. Not
   internal Rust mutex.

4. **TS-side currently has NO cross-process lock.** `async-mutex` is
   in-process only. So daemon↔TS coordination today relies on:
   - Atomic rename for write (no half-written files visible)
   - Read-inside-lock-window for the daemon (latest content always read)
   - Lost-update tolerance: if TS writes the same lesson at the same
     instant, one update may overwrite the other
   - Idempotent Set semantics for signal sources mitigates lost updates
     in the common case

5. **`chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)`
   produces byte-equivalent output to JS `Date().toISOString()`.**
   Verified empirically — both emit `YYYY-MM-DDTHH:MM:SS.sssZ`.

6. **The TS body-drift quirk converges with our normalization.** After
   one cycle, the body shape is stable; further cycles are no-ops on
   the body. Tested across 5 cycles + 8 concurrent threads — all stable.

## What this means for Day 13 (JSONL watcher)

JSONL watching is the next non-trivial concern. Pre-research should cover:

1. **`notify` crate semantics on macOS.** macOS uses FSEvents under the
   hood. There's a debounce vs raw question — does `notify` deliver
   events at the raw FSEvent rate (~10/sec) or batch them? Raw is
   needed for low-latency sentiment classification.

2. **Tail-as-it-grows pattern for append-only JSONL.** Files grow with
   new events appended. We want to read only the NEW bytes since last
   read, not re-parse the whole file. `notify::Watcher` + a per-file
   offset cursor is the standard pattern; verify.

3. **What does Claude Code's JSONL actually look like.** Day 5 already
   inspected real JSONL — event types, "interrupted" markers, etc.
   That research applies; refresh for any since-Day-5 changes.

4. **Edge cases:**
   - File truncation (mid-session log rotation by Claude Code, if any)
   - Multiple sessions writing concurrently (8+ projects, multiple
     terminal windows)
   - Initial-state handling (do we replay all old events on startup or
     just tail from now?)
   - File deletion (session ended; rotated; user deleted manually)
   - Symlinks (Claude Code's projects dir uses `-`-encoded paths;
     should be regular files)

5. **Performance:** how many bytes-per-second does an active session
   produce? Need an estimate to size buffer / read-window.

6. **Output of the watcher:** what type does it emit to the sentiment
   loop? Likely an enum like `WatcherEvent::UserTurn { session_id,
   text, turn_index }`. Define before building.

## What the Day 12 audit caught that we'd missed

The lock-then-rename race. Documented in `day-12-pre-research.md`'s
"What we did NOT research" section. Going forward: pre-research must
include "concurrency / consistency semantics" as a checklist item for
any module that does file I/O.
