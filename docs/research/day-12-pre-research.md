# Day 12 pre-research — lesson loader + signal writer + cross-process lock

**Backfilled 2026-05-13.**

## Question

Port TS-side `getLessonById` + `recordLessonSentimentSignal` semantics
into the Rust daemon, with cross-process safe write coordination so
the daemon and TS MCP server can coexist on the same lesson files.

## Pre-research inputs

From Day 11 post-research:
- Body normalization required (`trim_start_matches('\n')` before
  `combine_frontmatter`) to prevent unbounded newline drift.
- Writer reuse: `serialize_lesson_frontmatter` gives byte-stable output.

From TS source review:
- `core/src/lessons/loader.ts::getLessonById` scans 5 status dirs in
  canonical order, returns first match. ID validation via
  `isValidLessonId` (`les-` prefix + alphanumeric/dash).
- `core/src/lessons/signals.ts::recordLessonSentimentSignal` uses
  `async-mutex` per file path. In-process only, not cross-process.

From Phase A plan (`docs/phase-a-daemon-plan.md`):
- "Cross-process file lock via `fd-lock` (advisory flock)" called out
  as the approach.
- Atomic rename pattern for write.

## What we did NOT research and should have

This is where the gap surfaced — the audit caught a correctness bug
that pre-research would have prevented:

- **flock-and-atomic-rename interaction.** `fd-lock` operates on a file
  descriptor (per-OFD on Linux modern flock, per-inode on macOS BSD-style).
  Atomic rename replaces the inode. The held lock then references an
  unlinked inode. A second caller opening the path post-rename gets a
  FRESH inode and takes its own flock instantly. The advisory lock
  becomes meaningless across rename boundaries.

The build initially used lock-on-data-file. The Day 12 audit caught
this race. Fix: sidecar lock pattern — lock a `.<filename>.lock` file
in the same directory whose inode is stable.

## What Day 12 actually needed pre-research to cover

1. **Cross-process lock semantics under atomic rename** — the question
   above. Should have been answered before any code.
2. **Sidecar vs lock-on-data-file vs lock-on-parent-dir** as patterns —
   prior art exists (git uses `.lock` sidecars; `tempfile-fast` has
   similar). Should have surveyed.
3. **TS-side adoption path** — does the TS code need to adopt the same
   sidecar lock for full mutual exclusion? Spoiler: yes; deferred to
   Phase B.

## Risks identified

- Body-drift from Day 11 known quirk. Mitigation: `trim_start_matches`.
- Lock-then-rename race. CAUGHT BY AUDIT, fixed via sidecar pattern.
- Lesson-not-found vs IO-error differentiation. Loader returns
  `Ok(None)` for not-found, `Err` for IO. Matches TS semantics.
