# Day 13 pre-research — JSONL watcher

**Goal:** watch `~/.claude/projects/<encoded-cwd>/*.jsonl` for newly-appended
user turns, emit normalized events to the sentiment loop (Day 14+). No
code yet — surface constraints + edge cases.

## A. Concrete `notify` setup

**Selected crate stack** (verified May 2026 on docs.rs + GitHub):

| Crate | Version | License | Notes |
|---|---|---|---|
| `notify` | 8.2.0 (stable) | **CC0-1.0** | v9.0.0-rc.4 published 2026-05-02 — too fresh, stay on 8.x |
| `notify-debouncer-full` | 0.5 (matching notify 8.x) | MIT OR Apache-2.0 | Rename-tracking + dedup |
| `linemux` | 0.3.0 | MIT OR Apache-2.0 | Last commit 2026-03-30; uses `notify` underneath |

**License caveat:** `notify` itself is **CC0-1.0** (public domain
dedication), not MIT/Apache. This is permissive and SPDX-recognized, and
explicitly not AGPL/GPL/SSPL — so it clears the Loop license discipline
gate. Worth flagging in `THIRD_PARTY_LICENSES.md` because we've been
defaulting to MIT/Apache and CC0 is the first exception.

**macOS backend:** FSEvents (default feature `macos_fsevent`). Verified
in `notify/src/fsevent.rs` source — stream created with `latency: 0.0`,
so notify does NOT add a coalescing window on top of FSEvents. Raw
write events delivered as the kernel emits them (typical FSEvent
latency: 10-30ms wall clock on idle macOS, longer under load).

`EventKind` is a tree: `Any | Access | Create(CreateKind) | Modify(ModifyKind) | Remove(RemoveKind) | Other`.
For our watch on a project directory we'll see `Create(File)` when a new
session starts and `Modify(Data(Any))` (or just `Modify(Any)` on macOS —
FSEvents doesn't distinguish data vs metadata reliably) on each append.

**Code skeleton (~40 LOC, illustrative — NOT for commit yet):**

```rust
use notify::{RecommendedWatcher, RecursiveMode, Watcher, EventKind, Event};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

pub fn spawn_dir_watch(
    project_dir: PathBuf,
    tx: mpsc::UnboundedSender<PathBuf>, // emits a path on any change
) -> notify::Result<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        let Ok(ev) = res else { return };
        // Filter to .jsonl only, ignore the subagent + tool-results subdirs
        for p in ev.paths {
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
            // Bridge sync notify callback -> async consumer via mpsc
            let _ = tx.send(p);
        }
    })?;
    watcher.watch(&project_dir, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}
```

`RecursiveMode::NonRecursive` matters — each project dir has a
`subagents/` and `tool-results/` subdir we do NOT want to watch.
Subagent JSONLs are out of scope for sentiment (they're agent-internal,
not user turns).

## B. Tail-as-it-grows decision tree

**Decision: use raw `notify` + manual per-file offset cursor.** Reject
`linemux`. Reasoning:

1. **linemux abstracts away the events we need.** It emits lines, not
   filesystem events. We need to know when a file is *deleted* or
   *replaced* (session rotation), not just when lines appear.
2. **linemux's rotation/truncation behavior is undocumented.** README
   doesn't address it; source uses notify but the recovery logic
   isn't audit-friendly.
3. **Our parse step is JSON-line, not regex-line.** We get no real value
   from line-buffering in linemux when we'd just re-parse as JSON anyway.
4. **State is simpler.** A `BTreeMap<PathBuf, FileCursor>` where
   `FileCursor { offset: u64, inode: u64, size_at_last_read: u64 }` is
   trivial to reason about, easy to checkpoint, easy to test.

`notify-debouncer-full` is also rejected for the same module — its
2-second debounce window directly conflicts with the sentiment-loop
latency budget (we want <100ms from user pressing Enter to classifier
firing). Use raw `notify` and dedupe ourselves if we see duplicate
Modify events.

## C. Verified Claude Code event shape (May 2026, version 2.1.139)

Sampled `~/.claude/projects/-Users-slee-projects-loop/<UUID>.jsonl`
(7,630 lines, 25.6 MB, 4 days of active sessions).

**Top-level `type` values seen:**

| Type | Count | Action |
|---|---|---|
| `assistant` | 2576 | **Skip** (assistant turn — Day 14 consumes for context only) |
| `user` | 1694 | **Inspect** — only some are real user turns |
| `file-history-snapshot` | 747 | **Skip** noise |
| `system` | 587 | **Skip** noise |
| `permission-mode` | 527 | **Skip** noise |
| `ai-title` | 527 | **Skip** noise |
| `last-prompt` | 500 | **Skip** noise |
| `attachment` | 316 | **Skip** (sub-types: `task_reminder`, `edited_text_file`, `queued_command`, `file`, `deferred_tools_delta`, `mcp_instructions_delta`, `skill_listing`, `date_change`) |
| `queue-operation` | 156 | **Skip** noise |

**Of the 1,694 `user` events, content shape splits:**

| content shape | Count | Action |
|---|---|---|
| Array, first element `tool_result` | 1359 | **Skip** — tool output, not typed input |
| String (plain text) | 329 | **Candidate user turn** |
| Array with `text` block | 6 | **Candidate** (image attaches, slash-command results) |

**Mandatory filters for "real" user turn** (lifted from
`core/src/lessons/ingest/auto_dream.ts`, audit B3):

1. `type == "user"`
2. `isMeta != true` — drops `<local-command-caveat>` and image-source
   sentinels (Claude Code injects these on `/`-commands and image paste)
3. If content is array, all elements must NOT be `tool_result`
4. Content text must NOT match `^\[Request interrupted` (interrupt
   sentinel — capture as a SEPARATE `Interrupted` event for the
   sentiment loop, since it IS signal)
5. Content text must NOT be wrapped in `<command-name>...</command-name>`
   (slash-command sentinel — already isMeta=true in current versions
   but defense-in-depth)

**Useful fields for the watcher output:**

- `sessionId` — UUID of the Claude Code session
- `uuid` — event UUID (use as dedup key)
- `parentUuid` — links to prior turn (for correction-window mining in Day 15)
- `timestamp` — ISO-8601, set by Claude Code
- `cwd` — current working directory (project anchor)
- `gitBranch` — useful context
- `version` — Claude Code version (currently 2.1.139; sniff for shape drift)
- `message.content` — the actual text

**No new event types vs Day 5** (compared against the
`ecc-source-patterns-2026-05-12.md` taxonomy). `permission-mode`,
`last-prompt`, `queue-operation`, `file-history-snapshot` are all
existing noise. The two 4.6/4.7-era additions worth flagging:
`task_reminder` attachments (151 seen) and `edited_text_file`
attachments (101 seen) — both still pure noise for sentiment.

## D. Watcher state machine

Per-file state:

```text
FileCursor {
    path: PathBuf,
    session_id: String,        // parsed from filename UUID
    inode: u64,                // for rotation detection
    offset: u64,               // bytes read so far
    last_size: u64,            // detect truncation: size < offset
    last_mtime_ms: i64,        // for stale detection
    parse_error_count: u32,    // cap retries
}
```

Event loop (pseudocode):

```text
on notify event for path P:
  cursor = cursors.entry(P).or_insert_with(open_new)
  stat = fs::metadata(P)
  if stat.inode != cursor.inode:           // rotated (rare)
      replay_full(P, cursor); cursor.reset()
  if stat.size < cursor.offset:            // truncated (rare)
      cursor.offset = 0
      replay_full(P, cursor)
  if stat.size == cursor.offset: return    // metadata-only event
  read_from(P, cursor.offset..stat.size)
  for each line:
      parse_json or count_parse_error
      if is_user_turn: emit WatcherEvent::UserTurn
      if is_request_interrupted: emit WatcherEvent::UserInterrupt
  cursor.offset = stat.size
  cursor.last_mtime_ms = stat.mtime_ms

on notify Remove for path P:
  emit WatcherEvent::SessionEnded { session_id }
  cursors.remove(P)
```

**Initial-state policy:** open the directory, list `*.jsonl`, for each
existing file seek to EOF and set `cursor.offset = file_size`. We do
NOT replay history on daemon startup (see Open Questions).

**Partial-line handling:** since Claude Code appends one JSON object per
line and flushes per-write (verified empirically — `jq` parses cleanly
on a live file), the risk of reading a partial line is small but
nonzero. Mitigation: if the final byte read isn't `\n`, treat the trailing
fragment as buffered (don't advance offset past the last newline).

## E. `WatcherEvent` enum proposal

```rust
/// Public output of the watcher module. Consumed by the Day 14
/// sentiment classifier loop.
#[derive(Debug, Clone)]
pub enum WatcherEvent {
    /// A new user-typed turn was appended to a session transcript.
    UserTurn {
        session_id: String,           // UUID from filename / event
        event_uuid: String,           // unique dedup key
        parent_uuid: Option<String>,  // prior turn linkage
        cwd: PathBuf,                 // project anchor
        git_branch: Option<String>,
        timestamp: DateTime<Utc>,     // from event payload, not file mtime
        text: String,                 // plain extracted user text
        cc_version: String,           // e.g. "2.1.139" — for shape drift alerts
    },
    /// User pressed ESC mid-turn. Strong sentiment signal.
    UserInterrupt {
        session_id: String,
        event_uuid: String,
        parent_uuid: Option<String>,
        timestamp: DateTime<Utc>,
        // The assistant text the user interrupted, if recoverable
        interrupted_assistant_text: Option<String>,
    },
    /// New JSONL file appeared in the watched dir (session started).
    SessionStarted {
        session_id: String,
        path: PathBuf,
        started_at: DateTime<Utc>,
    },
    /// JSONL file removed (session deleted; rare).
    SessionEnded {
        session_id: String,
    },
    /// One line failed to parse. Aggregate via parse_error_count;
    /// emit once per N failures to avoid log spam.
    ParseError {
        session_id: String,
        offset: u64,
        raw_line: String,
        error: String,
    },
}
```

Channel type: `tokio::sync::mpsc::UnboundedSender<WatcherEvent>`.
Unbounded is fine — we cap upstream by JSONL append rate (~tens/sec
peak per session).

## F. Performance estimate

From the active session: 25.6 MB / 4 days / 7,630 lines.

- **Per-session bytes/sec at peak burst:** ~5-10 KB/sec when assistant is
  streaming. Idle: 0.
- **Per-session lines/sec at peak:** ~5-10. Idle: 0.
- **User turn rate:** 1,695 user events / 4 days ≈ 18/hour of active use.

**Scaling estimates:**

| Sessions | Watched FDs | RAM (cursor state) | CPU at peak | Notes |
|---|---|---|---|---|
| 1 | 1 dir | < 1 KB | negligible | typical |
| 10 | 10 dirs | ~10 KB | < 1% on M-series | reasonable upper bound |
| 100 | 100 dirs | ~100 KB | 2-5% steady, spike on coordinated activity | extreme; FSEvents kqueue limit far exceeds 100 |

`notify` event delivery on macOS with `latency: 0.0` is realtime
enough — 10-30ms typical. Sentiment classifier budget (target <100ms
end-to-end) has plenty of headroom.

## G. Open questions for the user

1. **Replay vs tail-from-now on daemon startup?** Recommendation:
   tail-from-now (seek to EOF). Replay is expensive (25 MB × N
   sessions) and the sentiment loop is forward-looking. Confirm or
   override.
2. **Per-cwd vs all-of-`~/.claude/projects/`?** Should the watcher
   target only the cwd the user `loop start`'d in, or every project
   dir under `~/.claude/projects/`? Currently TS-side does the latter
   in batch mode. For live watching, watching every dir is fine
   (cheap) but means cross-project signal. Recommend: watch ALL,
   filter downstream by cwd-of-event.
3. **Subagent transcripts** (the `<session>/subagents/agent-*.jsonl`
   tree, 50+ files seen)? Skip entirely? They're not user turns —
   they're internal agent runs. Recommend: skip.
4. **What's the contract for `ParseError`?** Drop silently, log only,
   or emit to a daemon-internal sink? Recommend: log + counter,
   never escalate (Claude Code occasionally writes mid-flush).
5. **Sleep/wake survival.** FSEvents survives macOS sleep via the
   `since_when` event ID, but `notify` initializes with
   `kFSEventStreamEventIdSinceNow` — so events during sleep are
   LOST. Acceptable for sentiment (post-sleep we just tail-from-now
   again, missing nothing important — sleep means no user activity).
   But on every wake, we should re-stat each watched file: if its
   size changed during sleep, we may have a backfill gap. Open Q:
   should we replay that gap, or accept the loss?

## H. Risks not yet covered

1. **`~/.claude/projects/` path sensitivity.** ECC research surfaced a
   "Claude Code sensitive-path guard" that blocks WRITES under
   `~/.claude/`. We only READ + watch, so we're fine — but worth a
   one-liner test (does `notify::Watcher::watch()` succeed on a path
   under `~/.claude/`?) before assuming.
2. **Notify Watcher Drop semantics.** Dropping the `RecommendedWatcher`
   stops the FSEvent stream. The watcher must be `'static`-rooted
   somewhere (held by the daemon's lifecycle struct) — or events
   silently stop.
3. **mpsc channel blocking the notify thread.** `notify`'s callback
   runs on a dedicated thread; if we use `tokio::sync::mpsc::Sender`
   `try_send` or unbounded, we don't block. If we use a *bounded*
   channel and `blocking_send`, we'd stall notify. Stick with
   unbounded OR `try_send` + drop-with-counter.
4. **CC version drift (2.2.x, 2.3.x).** The `version` field on each
   event gives us a tripwire — log a warning if we see an unknown
   shape that doesn't match 2.1.x. Add a stat counter.
5. **Symlinks under `~/.claude/projects/`.** Verified: regular files
   in this user's home, but worth one `lstat()` defense in code.
6. **Daemon and TS MCP server both reading the same JSONLs.** No
   conflict — reads don't need locks, and we never write to these
   files. But if both processes maintain offset cursors and emit
   duplicate events into downstream sinks, that's a Day 14+ concern
   (idempotent sentiment writes, or one canonical reader).
7. **Sidechain events.** Real user transcripts contain
   `"isSidechain": true` events for `Task`-spawned subagents inline in
   the parent JSONL. Currently TS-side filters these implicitly (they
   appear under `tool_result` content). Verify the same skip path
   covers them; add explicit `isSidechain == true` filter for safety.
8. **Encoded-path edge cases.** `cwd` `/Users/slee/projects/loop-daemon`
   encodes to `-Users-slee-projects-loop-daemon`. If a real path
   contains a literal `-`, the encoding is lossy (no escape mechanism
   that I've seen). The daemon should resolve project dir → cwd via
   the `cwd` field IN the JSONL, not by reversing the dir name.
