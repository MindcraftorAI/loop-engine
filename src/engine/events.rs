//! Event source abstraction.
//!
//! Trait shape: `EventSource::run(ctx, shutdown)` is an async factory
//! returning `BoxStream<Result<EngineEvent, EventSourceError>>`. Hosts
//! implement this trait by translating their native event surface
//! (Claude Code JSONL, MCP RPC, HTTP webhooks) into [`EngineEvent`]
//! variants.
//!
//! [`EngineEvent::UserTurn`] field set was locked Day 15 (D1):
//! - `parent_event_uuid: Option<String>` for correction-window mining
//! - `host_version: Option<HostVersion>` for daemon-version tripwire (Day 17)
//! - `project_tag: Option<ProjectTag>` for project-scoped routing (Phase C)
//!
//! No EventSource impl ships yet — `JsonlWatcher::EventSource` lands
//! in Day 16 as part of the orchestrator wiring. Day 15 pure-logic
//! sentiment code (pretrigger / classifier trait / attribution) ships
//! against the locked event shape.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::BoxStream;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::context::{Context, SessionId};

/// Normalized event types the engine consumes. Host adapters translate
/// their native events into one of these variants before emitting.
///
/// `#[non_exhaustive]`: variants are added (sentiment signals from
/// Day 15+, auto-memory candidates later, Haiku classifications) without
/// breaking external consumers.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum EngineEvent {
    /// A user-authored turn in the live conversation.
    UserTurn {
        session_id: SessionId,
        event_uuid: String,
        /// Day 15 D1: parent-event linkage. Used by Day 16 orchestrator
        /// for correction-window mining (the previous turn the user is
        /// responding to). Plain `Option<String>` — not a domain
        /// concept worth a newtype.
        parent_event_uuid: Option<String>,
        text: String,
        timestamp: DateTime<Utc>,
        cwd: Option<PathBuf>,
        /// Day 15 D1: host version string (e.g. Claude Code "2.1.139").
        /// Used by Day 17 solicitor's daemon-version tripwire.
        host_version: Option<HostVersion>,
        /// Day 15 D1: opaque project routing tag. Host adapter derives
        /// from host-specific signals (Claude Code: git_branch or cwd
        /// basename). The engine treats as opaque routing key.
        project_tag: Option<ProjectTag>,
    },

    /// The user interrupted a previous turn (Claude Code's
    /// `[Request interrupted by user]` sentinel).
    UserInterrupt {
        session_id: SessionId,
        event_uuid: String,
        /// Same correction-window mining input as UserTurn.
        parent_event_uuid: Option<String>,
        timestamp: DateTime<Utc>,
    },

    /// A new session began. Hosts that observe sessions out-of-band
    /// (file watcher seeing a new JSONL file) emit this when the session
    /// is first detected.
    SessionStarted {
        session_id: SessionId,
        path: PathBuf,
        started_at: DateTime<Utc>,
    },

    /// The session ended (file deleted, watcher unsubscribed, etc.).
    SessionEnded { session_id: SessionId },
}

/// Host application version (e.g. Claude Code's `"2.1.139"`).
///
/// Day 15 D1 (host-agnostic generalization of `cc_version`): newtype so
/// comparison against a known-good range is typed at the call site.
/// The tripwire impl `is_in_tested_range()` lands in Day 17 as part of
/// solicitor work (Day 15 OQ4 — bare type today).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostVersion(Arc<str>);

impl HostVersion {
    pub fn new(v: impl Into<Arc<str>>) -> Self {
        Self(v.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for HostVersion {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HostVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Opaque project-routing tag.
///
/// Day 15 D1 + OQ5: **derivation lives in the host adapter, not the
/// engine.** Claude Code's adapter uses `git_branch` when present and
/// the `cwd` basename otherwise. HTTP / MCP / Cursor adapters supply
/// whatever signal is meaningful in their world. The engine treats
/// this as an opaque routing key for per-project sentiment thresholds
/// (Day 16 orchestrator) and never parses or interprets the contents.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProjectTag(Arc<str>);

impl ProjectTag {
    pub fn new(t: impl Into<Arc<str>>) -> Self {
        Self(t.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ProjectTag {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProjectTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Errors emitted into an [`EventSource`] stream. Split into transient
/// (skip this event, keep going) and fatal (stream terminates).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EventSourceError {
    /// A single bad event — usually a parse error. Stream continues.
    #[error("transient event-source error: {0}")]
    Transient(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Underlying source is broken — stream terminates. Caller decides
    /// whether to reconnect / retry / abort.
    #[error("fatal event-source error: {0}")]
    Fatal(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl EventSourceError {
    pub fn transient<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Transient(Box::new(err))
    }

    pub fn fatal<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Fatal(Box::new(err))
    }
}

/// Source of engine events. Object-safe via `Arc<dyn EventSource>`.
///
/// Implementations live in `host::*` modules. The engine consumes a
/// `select_all`-merged stream of all wired sources.
#[async_trait]
pub trait EventSource: Send + Sync {
    /// Start emitting events. The returned stream lives until `shutdown`
    /// is cancelled or the source terminates naturally.
    async fn run(
        &self,
        ctx: &Context,
        shutdown: CancellationToken,
    ) -> BoxStream<'static, Result<EngineEvent, EventSourceError>>;

    /// Diagnostic name used in logs and health endpoints.
    fn name(&self) -> &'static str;
}
