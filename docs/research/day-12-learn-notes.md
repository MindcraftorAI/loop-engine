# Day 12 learn notes — lesson loader + signal writer design

**Backfilled 2026-05-13.**

## Module split

- `src/lessons/mod.rs` — public API barrel
- `src/lessons/loader.rs` — `get_lesson_by_id`, `load_lesson_file`,
  `lesson_file_path`, `is_valid_lesson_id`, `LoadedLesson` struct
- `src/lessons/lock.rs` — `with_lock(target, fn)`, sidecar lock path
  helper
- `src/lessons/signals.rs` — `record_sentiment_signal(id, polarity)`,
  `SignalPolarity` enum, atomic-rename write helper

## Cross-process lock decision (post-audit revision)

**Sidecar lock pattern.** Each lesson `<id>.md` paired with
`<dir>/.<id>.md.lock`. All callers `flock()` the sidecar — its inode
is stable across atomic rename of the data file, so every caller
serializes through the same kernel mutex.

Initial implementation locked the data file directly. Audit caught
the lock-then-rename race (`with_lock` callback's held fd references
an unlinked inode after rename; a fresh open gets a new inode and
takes its own flock instantly → no actual mutual exclusion).
Sidecar fix lands before commit.

## Atomic write pattern

`.<filename>.tmp.<pid>.<ns>` staging path → write contents → fsync
(best-effort on macOS) → rename over target. The temp filename
includes PID + nanosecond timestamp to make `create_new` collision-
resistant under concurrent attempts.

## Body normalization

`lesson.body.trim_start_matches('\n')` before passing to
`combine_frontmatter`. Prevents the Day 11 known-quirk newline drift
from accumulating across signal-emit cycles. Converges to a stable
shape after the first cycle.

Caveat: a user-authored leading blank line after the closing `---`
gets squished on first signal write. Acceptable — markdown renders
identically.

## SignalPolarity → external_signal_sources mapping

- `Positive` → `sentiment_positive`
- `Negative` → `sentiment_negative`

Idempotent Set semantics: if the signal source is already present,
the source list is unchanged (but `updated_at` advances).

## What we deferred

- TS-side adoption of the sidecar lock (full daemon↔TS mutual
  exclusion). Today's protection is daemon↔daemon only; daemon↔TS
  relies on atomic rename + read-inside-lock-window. Phase B.
- Second-process integration test (currently threads simulate cross-
  process). Day 17 will spawn a real binary.
- `now_iso()` byte-equivalence with TS `Date().toISOString()` verified
  but not asserted in a test. Worth a fixture test later.

## Test coverage achieved

- Loader: invalid-id rejection, missing-lesson None, all-5-status-
  dirs scan, lesson_file_path helpers
- Lock: serialization across threads, release on completion, error
  propagation, target-doesn't-exist case, rename-in-critical-section
  regression test (proves sidecar fix works)
- Signals: empty-sources case, both polarities, preserves existing
  signals, idempotent, body-drift convergence over 5 cycles,
  body-content preservation, atomic-rename inode change
- Concurrent integration: 8 threads alternating polarities, both
  signals end up in file, no lost updates, no body drift
