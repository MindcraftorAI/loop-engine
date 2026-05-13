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

/// Spawn a directory watcher. Emits `WatcherEvent`s on `events_tx`.
///
/// The notify callback is sync, runs on a dedicated thread, and uses
/// `try_send` on an unbounded channel — never blocks. The async runner
/// task drains the path-change channel and does the actual file reads.
pub async fn spawn_watcher(
    dir: PathBuf,
    events_tx: UnboundedSender<WatcherEvent>,
) -> Result<WatcherHandle> {
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

    let runner_task = tokio::spawn(run_loop(path_rx, events_tx));

    Ok(WatcherHandle {
        _watcher: watcher,
        _runner_task: runner_task,
    })
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

    // For Create events, spin up a cursor at offset 0 (we want the first
    // events of a new session). For Modify on a known file, the cursor
    // already exists; for Modify on an unknown file (we missed the Create
    // somehow), treat as new — start at 0 so we don't miss content.
    let is_new = !cursors.contains_key(&change.path);
    let cursor = match cursors.get_mut(&change.path) {
        Some(c) => c,
        None => {
            // Synthesize SessionStarted on first sight, if it's a Create.
            if change.kind == PathChangeKind::Create {
                emit_session_started(&change.path, &session_id, events_tx);
            }
            let c = FileCursor::new_at_start(change.path.clone(), session_id.clone())?;
            cursors.insert(change.path.clone(), c);
            cursors.get_mut(&change.path).unwrap()
        }
    };
    if !is_new {
        debug!(path = %change.path.display(), "watcher: known cursor advancing");
    }

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

    let (lines, fragment_len) = cursor.read_appended(from, count)?;
    for line in &lines {
        process_line(cursor, line, events_tx);
    }
    // Advance offset past complete lines only; fragment stays buffered
    // (the next classify will re-read it once the writer flushes the \n).
    cursor.offset = from + count - fragment_len;
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
