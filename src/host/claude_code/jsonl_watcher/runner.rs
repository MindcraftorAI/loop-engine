//! Runner — bridges notify's sync callback to async WatcherEvent stream.
//!
//! Flow:
//!   1. `spawn_watcher` registers a `notify::Watcher` on a directory.
//!   2. notify's callback (sync, on a notify-owned thread) deduplicates
//!      paths and pushes them through a `tokio::sync::mpsc::UnboundedSender`.
//!   3. An async task drains the channel: for each path, classify the
//!      cursor, read appended bytes, parse lines, emit WatcherEvents.
//!   4. The caller holds the returned `Watcher` to keep the FSEvent stream
//!      alive. Dropping it stops watching.
//!
//! Audit checklist from learn note:
//!   - notify callback MUST NOT block (use try_send / unbounded channel)
//!   - partial-line guard correct on append
//!   - rotation detected via inode change
//!   - sidechain filter applied via parser
//!   - encoded-path / cwd resolved from event payload (already in parser)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, warn};

use super::cursor::{CursorAction, FileCursor};
use super::events::{WatcherEvent, PARSE_ERROR_REPORT_EVERY};
use super::parser::{parse_line, ParseOutcome};

/// Maximum bytes to read in a single `read_appended` call. Caps the
/// memory footprint per event in case a session balloons.
const MAX_APPEND_READ: u64 = 1024 * 1024; // 1 MB

/// Public handle to a spawned watcher. Holds the `RecommendedWatcher`
/// so the FSEvent stream stays alive. Drop to stop watching.
pub struct WatcherHandle {
    _watcher: RecommendedWatcher,
    _runner_task: tokio::task::JoinHandle<()>,
}

// `RecommendedWatcher` is a type alias that resolves per-target
// (`FsEventWatcher` on macOS, `INotifyWatcher` on Linux, etc). The
// per-target backends produce different auto-derived UnwindSafe
// impls, which makes the public-api surface diverge by build host.
// `WatcherHandle` is a Drop-only guard with no panic-observable
// state — explicit impls keep the public surface stable across
// build platforms (pre-2026-05-15 the gate baseline was
// platform-locked to whoever generated it last).
impl std::panic::UnwindSafe for WatcherHandle {}
impl std::panic::RefUnwindSafe for WatcherHandle {}

/// Spawn a directory watcher. Emits `WatcherEvent`s on `events_tx`.
///
/// The notify callback is sync, runs on a dedicated thread, and uses
/// `try_send` on an unbounded channel — never blocks. The async runner
/// task drains the path-change channel and does the actual file reads.
pub async fn spawn_watcher(
    dir: PathBuf,
    events_tx: UnboundedSender<WatcherEvent>,
) -> Result<WatcherHandle> {
    // Canonicalize once at entry. macOS FSEvents reports realpaths
    // (`/private/var/...`) while a caller-supplied path may be a
    // symlink (`/var/...`). Mixing the two as `BTreeMap<PathBuf, _>`
    // keys produced duplicate cursors → duplicate `SessionStarted`s.
    // Fall back to the original path if canonicalization fails
    // (e.g. directory not yet created in some pathological test).
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);

    let (path_tx, path_rx) = tokio::sync::mpsc::unbounded_channel::<PathChange>();

    // notify callback runs on its own thread; tx is cloneable + send-safe.
    let cb_tx = path_tx.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else {
            return;
        };
        for p in ev.paths {
            // Skip non-JSONL files — the watched dir may contain other artifacts.
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let kind = classify_event_kind(&ev.kind);
            let _ = cb_tx.send(PathChange { path: p, kind });
        }
    })
    .context("creating notify::Watcher")?;

    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {}", dir.display()))?;

    // Audit Day 13 — A5 fix: emit `SessionStarted` for pre-existing JSONL
    // files visible at watcher startup. Done by synthesizing a Modify
    // PathChange per file; handle_change picks the tail-from-now cursor
    // mode for Modify-on-unknown (A1 fix).
    initial_scan(&dir, &path_tx);

    let runner_task = tokio::spawn(run_loop(path_rx, events_tx));

    Ok(WatcherHandle {
        _watcher: watcher,
        _runner_task: runner_task,
    })
}

/// One-shot directory scan at watcher startup. Closes audit Day 13 A5:
/// pre-existing `.jsonl` files were previously invisible until they
/// next received a write event, so daemons starting against an
/// already-populated `~/.claude/projects/<dir>/` could miss the first
/// turn that triggered their wake-up.
fn initial_scan(dir: &Path, path_tx: &UnboundedSender<PathChange>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(dir = %dir.display(), err = %e, "watcher: initial scan failed; pre-existing files will be missed until next FSEvent");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let _ = path_tx.send(PathChange {
            path,
            kind: PathChangeKind::Modify,
        });
    }
}

#[derive(Debug, Clone)]
struct PathChange {
    path: PathBuf,
    kind: PathChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathChangeKind {
    Create,
    Modify,
    Remove,
    Other,
}

fn classify_event_kind(kind: &EventKind) -> PathChangeKind {
    match kind {
        EventKind::Create(_) => PathChangeKind::Create,
        EventKind::Modify(_) => PathChangeKind::Modify,
        EventKind::Remove(_) => PathChangeKind::Remove,
        _ => PathChangeKind::Other,
    }
}

/// Async drain loop. Reads path-changes, maintains cursors, emits events.
async fn run_loop(
    mut path_rx: UnboundedReceiver<PathChange>,
    events_tx: UnboundedSender<WatcherEvent>,
) {
    let mut cursors: BTreeMap<PathBuf, FileCursor> = BTreeMap::new();
    while let Some(change) = path_rx.recv().await {
        if let Err(e) = handle_change(&change, &mut cursors, &events_tx) {
            warn!(path = %change.path.display(), err = %e, "watcher: failed to handle change");
        }
    }
    debug!("watcher: path_rx channel closed; runner exiting");
}

fn handle_change(
    change: &PathChange,
    cursors: &mut BTreeMap<PathBuf, FileCursor>,
    events_tx: &UnboundedSender<WatcherEvent>,
) -> Result<()> {
    let session_id = FileCursor::session_id_from_path(&change.path);

    if change.kind == PathChangeKind::Remove {
        if cursors.remove(&change.path).is_some() {
            let _ = events_tx.send(WatcherEvent::SessionEnded {
                session_id: session_id.clone(),
            });
        }
        return Ok(());
    }

    // Audit Day 13 A1+A5 fix: cursor mode by event kind.
    //   - `Create` = truly new file (born during this watcher session).
    //     Replay from offset 0 — we want every byte.
    //   - Anything else (`Modify` or `Other`) = pre-existing OR initial-
    //     scan synthesized. Tail-from-now — pre-existing content already
    //     happened and is not part of "live" verification.
    // SessionStarted fires on first sight regardless of kind (A5).
    //
    // Known cross-platform edge case (Day 14 audit M3): on macOS FSEvents
    // can sometimes report `Create` for files that already existed when
    // the watcher attached (historical-event delivery quirk). Combined
    // with the initial-scan synthesis, ordering determines outcome:
    // whichever PathChange arrives in the channel first wins. The
    // canonicalization fix in `spawn_watcher` plus the first-sight rule
    // mean SessionStarted still fires exactly once per file even in this
    // race; cursor offset may be 0 (replay) vs EOF (tail) depending on
    // ordering. Acceptable: low-frequency, non-corrupting, and the
    // replayed bytes are valid JSONL.
    let cursor = match cursors.get_mut(&change.path) {
        Some(c) => {
            debug!(path = %change.path.display(), "watcher: known cursor advancing");
            c
        }
        None => {
            emit_session_started(&change.path, &session_id, events_tx);
            let c = match change.kind {
                PathChangeKind::Create => {
                    FileCursor::new_at_start(change.path.clone(), session_id.clone())?
                }
                _ => FileCursor::new_at_eof(change.path.clone(), session_id.clone())?,
            };
            cursors.insert(change.path.clone(), c);
            cursors.get_mut(&change.path).unwrap()
        }
    };

    process_cursor(cursor, events_tx)?;
    Ok(())
}

fn emit_session_started(path: &Path, session_id: &str, events_tx: &UnboundedSender<WatcherEvent>) {
    let started_at = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.created().ok())
        .and_then(|sys| {
            chrono::DateTime::<chrono::Utc>::from_timestamp(
                sys.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64,
                0,
            )
        })
        .unwrap_or_else(chrono::Utc::now);
    let _ = events_tx.send(WatcherEvent::SessionStarted {
        session_id: session_id.to_string(),
        path: path.to_path_buf(),
        started_at,
    });
}

fn process_cursor(
    cursor: &mut FileCursor,
    events_tx: &UnboundedSender<WatcherEvent>,
) -> Result<()> {
    // Audit Day 13 A2+A3 fix: loop until file is caught up.
    //
    // Prior bug A3: a single FSEvent triggered one read capped at
    // `MAX_APPEND_READ`. If the writer flushed >1MB then stopped, the
    // remaining bytes were stranded until the NEXT FSEvent — which
    // might never arrive.
    //
    // Prior bug A2: offset advance used `count` (the requested read
    // size) instead of `result.actual_read`. If the file shrank between
    // classify and read (or short-read at EOF), the cursor advanced
    // past bytes that weren't actually consumed. Fix: use
    // `result.advance()` = `actual_read - fragment_len`.
    //
    // The MAX_ITER guard caps the loop in case a pathological writer
    // outpaces us; 64 * 1MB = 64MB per FSEvent is generous for a
    // line-oriented append-only transcript.
    const MAX_ITER: u32 = 64;
    for iteration in 0..MAX_ITER {
        let action = cursor.classify()?;
        let (from, count) = match action {
            CursorAction::NoChange => return Ok(()),
            CursorAction::Removed => {
                let _ = events_tx.send(WatcherEvent::SessionEnded {
                    session_id: cursor.session_id.clone(),
                });
                return Ok(());
            }
            CursorAction::Append { read_bytes } => (cursor.offset, read_bytes.min(MAX_APPEND_READ)),
            CursorAction::ReplayFromStart { total_bytes } => (0, total_bytes.min(MAX_APPEND_READ)),
        };

        let result = cursor.read_appended(from, count)?;
        for line in &result.lines {
            process_line(cursor, line, events_tx);
        }
        cursor.offset = from + result.advance();

        // If we didn't hit the read cap, the file is caught up for now.
        if count < MAX_APPEND_READ {
            return Ok(());
        }
        // Audit Day 14 M4 fix: warn if MAX_ITER bound is approached so an
        // operator notices pathological inputs (e.g. a single line >1MB
        // with no terminating `\n` that re-reads the same MAX_APPEND_READ
        // every iteration without advancing the offset).
        if iteration + 1 == MAX_ITER {
            warn!(
                path = %cursor.path.display(),
                offset = cursor.offset,
                "watcher: process_cursor hit MAX_ITER cap; file may have an oversized partial line"
            );
        }
    }
    Ok(())
}

fn process_line(cursor: &mut FileCursor, line: &str, events_tx: &UnboundedSender<WatcherEvent>) {
    let outcome = parse_line(line, &cursor.session_id);
    match outcome {
        ParseOutcome::Event(ev) => {
            cursor.parse_error_count = 0; // reset on success
            let _ = events_tx.send(ev);
        }
        ParseOutcome::Skip(_) => {
            cursor.parse_error_count = 0; // skip is not an error
        }
        ParseOutcome::Error(err_msg) => {
            cursor.parse_error_count = cursor.parse_error_count.saturating_add(1);
            if cursor.parse_error_count >= PARSE_ERROR_REPORT_EVERY {
                let truncated_line: String = line.chars().take(200).collect();
                let _ = events_tx.send(WatcherEvent::ParseError {
                    session_id: cursor.session_id.clone(),
                    offset: cursor.offset,
                    raw_line: truncated_line,
                    error: err_msg,
                });
                cursor.parse_error_count = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::time::Duration;
    use tempfile::TempDir;

    fn write_user_turn(path: &PathBuf, uuid: &str, text: &str) {
        let line = format!(
            r#"{{"type":"user","uuid":"{uuid}","cwd":"/c","timestamp":"2026-05-13T10:00:00.000Z","version":"2.1.139","sessionId":"sess-1","message":{{"role":"user","content":"{text}"}}}}"#
        );
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    #[tokio::test]
    async fn integration_emits_user_turn_on_append() {
        let dir = TempDir::new().unwrap();
        let session_path = dir.path().join("session.jsonl");
        // Pre-create file so the watcher sees it; tail-from-now.
        std::fs::write(&session_path, "").unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WatcherEvent>();
        let _handle = spawn_watcher(dir.path().to_path_buf(), tx).await.unwrap();

        // Small grace for the watcher to attach to FSEvents.
        tokio::time::sleep(Duration::from_millis(150)).await;

        write_user_turn(&session_path, "u1", "hello daemon");

        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("watcher did not emit within timeout")
            .expect("channel closed");

        match event {
            WatcherEvent::UserTurn {
                text, event_uuid, ..
            } => {
                assert_eq!(text, "hello daemon");
                assert_eq!(event_uuid, "u1");
            }
            // Some FSEvent flavors may deliver a SessionStarted first; in
            // that case drain to find the UserTurn.
            WatcherEvent::SessionStarted { .. } => {
                let next = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                    .await
                    .expect("no follow-up event")
                    .expect("channel closed");
                match next {
                    WatcherEvent::UserTurn { text, .. } => {
                        assert_eq!(text, "hello daemon");
                    }
                    other => panic!("expected UserTurn, got {other:?}"),
                }
            }
            other => panic!("expected UserTurn, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn integration_filters_assistant_turns() {
        let dir = TempDir::new().unwrap();
        let session_path = dir.path().join("session.jsonl");
        std::fs::write(&session_path, "").unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WatcherEvent>();
        let _handle = spawn_watcher(dir.path().to_path_buf(), tx).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Append an assistant turn (should be skipped).
        let mut f = OpenOptions::new().append(true).open(&session_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","uuid":"a1","message":{{"role":"assistant","content":"hi"}}}}"#
        )
        .unwrap();

        // Followed by a real user turn (should arrive).
        write_user_turn(&session_path, "u2", "real input");

        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("watcher did not emit user turn within timeout")
                .expect("channel closed");
            match event {
                WatcherEvent::UserTurn { text, .. } => {
                    assert_eq!(text, "real input");
                    break;
                }
                WatcherEvent::SessionStarted { .. } => continue,
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }
}
