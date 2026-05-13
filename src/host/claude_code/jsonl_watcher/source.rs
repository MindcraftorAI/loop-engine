//! `EventSource` impl over the Claude Code JSONL watcher.
//!
//! Locked decisions (`docs/research/day-16a-learn-notes.md` D7+D8):
//! - Wraps the existing `spawn_watcher` (Day 13 — 127-test-validated cursor
//!   logic, A1-A5 fixes) — DOES NOT rewrite the watcher
//! - Bridges the watcher's mpsc to `BoxStream` via
//!   `tokio_stream::wrappers::UnboundedReceiverStream`
//! - Shutdown via spawned task that drops the `WatcherHandle` on
//!   `CancellationToken::cancelled()`
//! - Translates `WatcherEvent` → `EngineEvent` per the D8 mapping; lifts
//!   `WatcherEvent::ParseError` to `EventSourceError::Transient`

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::engine::context::{Context, SessionId};
use crate::engine::events::{EngineEvent, EventSource, EventSourceError, HostVersion, ProjectTag};

use super::events::WatcherEvent;
use super::runner::spawn_watcher;

/// `EventSource` impl over the Claude Code JSONL transcript watcher.
/// Construct with the directory to watch; the engine drives the rest.
#[derive(Debug, Clone)]
pub struct JsonlWatcherSource {
    dir: PathBuf,
}

impl JsonlWatcherSource {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

#[async_trait]
impl EventSource for JsonlWatcherSource {
    async fn run(
        &self,
        _ctx: &Context,
        shutdown: CancellationToken,
    ) -> BoxStream<'static, Result<EngineEvent, EventSourceError>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<WatcherEvent>();
        let dir = self.dir.clone();

        let handle = match spawn_watcher(dir, tx).await {
            Ok(h) => h,
            Err(e) => {
                let display = e.to_string();
                let stream =
                    futures::stream::once(async move {
                        Err(EventSourceError::fatal(std::io::Error::other(format!(
                            "JsonlWatcherSource init: {display}"
                        ))))
                    });
                return Box::pin(stream);
            }
        };

        // Shutdown bridge: when the token fires, drop the handle, which
        // drops the inner notify::Watcher + runner task, which closes
        // `tx`, which terminates the receiver stream below.
        tokio::spawn(async move {
            shutdown.cancelled().await;
            drop(handle);
        });

        let receiver_stream = UnboundedReceiverStream::new(rx);
        let translated =
            receiver_stream.filter_map(|w_evt| async move { Some(translate(w_evt)) });
        Box::pin(translated)
    }

    fn name(&self) -> &'static str {
        "claude_code.jsonl_watcher"
    }
}

/// Translate a host-side `WatcherEvent` into an engine `EngineEvent`.
/// `WatcherEvent::ParseError` lifts to `EventSourceError::Transient`.
fn translate(w: WatcherEvent) -> Result<EngineEvent, EventSourceError> {
    match w {
        WatcherEvent::UserTurn {
            session_id,
            event_uuid,
            parent_uuid,
            cwd,
            git_branch,
            timestamp,
            text,
            cc_version,
        } => {
            let project_tag = derive_project_tag(&git_branch, &cwd);
            Ok(EngineEvent::UserTurn {
                session_id: SessionId::new(session_id),
                event_uuid,
                parent_event_uuid: parent_uuid,
                text,
                timestamp,
                cwd: Some(cwd),
                host_version: Some(HostVersion::new(cc_version)),
                project_tag,
            })
        }
        WatcherEvent::UserInterrupt {
            session_id,
            event_uuid,
            parent_uuid,
            timestamp,
            ..
        } => Ok(EngineEvent::UserInterrupt {
            session_id: SessionId::new(session_id),
            event_uuid,
            parent_event_uuid: parent_uuid,
            timestamp,
        }),
        WatcherEvent::SessionStarted {
            session_id,
            path,
            started_at,
        } => Ok(EngineEvent::SessionStarted {
            session_id: SessionId::new(session_id),
            path,
            started_at,
        }),
        WatcherEvent::SessionEnded { session_id } => Ok(EngineEvent::SessionEnded {
            session_id: SessionId::new(session_id),
        }),
        WatcherEvent::ParseError {
            offset,
            error,
            session_id,
            ..
        } => Err(EventSourceError::transient(std::io::Error::other(format!(
            "session={session_id} offset={offset}: {error}"
        )))),
    }
}

/// Derive a `ProjectTag` from the host-specific fields. Per Day 15 OQ5
/// + Day 16a D8: host adapter derives, engine treats as opaque. Order:
///   1. `git_branch` if non-empty
///   2. `cwd.file_name()` basename if non-empty
///   3. `None` (no derivation possible)
fn derive_project_tag(git_branch: &Option<String>, cwd: &Path) -> Option<ProjectTag> {
    if let Some(branch) = git_branch.as_deref() {
        if !branch.is_empty() {
            return Some(ProjectTag::new(branch.to_string()));
        }
    }
    cwd.file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .map(|s| ProjectTag::new(s.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::path::PathBuf;

    fn make_user_turn() -> WatcherEvent {
        WatcherEvent::UserTurn {
            session_id: "sess-1".into(),
            event_uuid: "evt-1".into(),
            parent_uuid: Some("evt-0".into()),
            cwd: PathBuf::from("/tmp/loop-test"),
            git_branch: Some("main".into()),
            timestamp: Utc::now(),
            text: "thanks".into(),
            cc_version: "2.1.139".into(),
        }
    }

    #[test]
    fn translate_user_turn_maps_all_fields() {
        let w = make_user_turn();
        let e = translate(w).unwrap();
        let EngineEvent::UserTurn {
            session_id,
            event_uuid,
            parent_event_uuid,
            text,
            cwd,
            host_version,
            project_tag,
            ..
        } = e
        else {
            panic!("expected UserTurn");
        };
        assert_eq!(session_id.as_str(), "sess-1");
        assert_eq!(event_uuid, "evt-1");
        assert_eq!(parent_event_uuid.as_deref(), Some("evt-0"));
        assert_eq!(text, "thanks");
        assert_eq!(cwd.unwrap(), PathBuf::from("/tmp/loop-test"));
        assert_eq!(host_version.unwrap().as_str(), "2.1.139");
        assert_eq!(project_tag.unwrap().as_str(), "main");
    }

    #[test]
    fn translate_parse_error_is_transient() {
        let w = WatcherEvent::ParseError {
            session_id: "sess-1".into(),
            offset: 42,
            raw_line: "{bad json".into(),
            error: "unexpected token".into(),
        };
        let result = translate(w);
        assert!(matches!(result, Err(EventSourceError::Transient(_))));
    }

    #[test]
    fn translate_session_started_and_ended_pass_through() {
        let started = WatcherEvent::SessionStarted {
            session_id: "sess-1".into(),
            path: PathBuf::from("/tmp/sess-1.jsonl"),
            started_at: Utc::now(),
        };
        match translate(started).unwrap() {
            EngineEvent::SessionStarted { session_id, .. } => {
                assert_eq!(session_id.as_str(), "sess-1");
            }
            _ => panic!("expected SessionStarted"),
        }
        let ended = WatcherEvent::SessionEnded {
            session_id: "sess-1".into(),
        };
        match translate(ended).unwrap() {
            EngineEvent::SessionEnded { session_id } => {
                assert_eq!(session_id.as_str(), "sess-1");
            }
            _ => panic!("expected SessionEnded"),
        }
    }

    #[test]
    fn derive_project_tag_prefers_git_branch() {
        let tag = derive_project_tag(
            &Some("feature/x".into()),
            Path::new("/tmp/projects/myrepo"),
        )
        .unwrap();
        assert_eq!(tag.as_str(), "feature/x");
    }

    #[test]
    fn derive_project_tag_falls_back_to_cwd_basename() {
        let tag = derive_project_tag(&None, Path::new("/tmp/projects/myrepo")).unwrap();
        assert_eq!(tag.as_str(), "myrepo");
    }

    #[test]
    fn derive_project_tag_empty_branch_falls_back_to_cwd() {
        let tag =
            derive_project_tag(&Some("".into()), Path::new("/tmp/projects/myrepo")).unwrap();
        assert_eq!(tag.as_str(), "myrepo");
    }

    #[test]
    fn derive_project_tag_returns_none_for_root_cwd_no_branch() {
        let tag = derive_project_tag(&None, Path::new("/"));
        assert!(tag.is_none());
    }
}
