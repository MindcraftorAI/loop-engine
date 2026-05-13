//! Event source abstraction.
//!
//! Trait shape: `EventSource::run(ctx, shutdown)` is an async factory
//! returning `BoxStream<Result<EngineEvent, EventSourceError>>`. Hosts
//! implement this trait by translating their native event surface
//! (Claude Code JSONL, MCP RPC, HTTP webhooks) into [`EngineEvent`]
//! variants.
//!
//! **Phase 3b status:** trait + types defined. The first impl
//! (`JsonlWatcher::EventSource`) lands in Phase 3c alongside the Day 13
//! audit fixes A1-A5. Engine consumers (orchestrator etc.) consume
//! these starting Day 15.

use std::path::PathBuf;

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
        text: String,
        timestamp: DateTime<Utc>,
        cwd: Option<PathBuf>,
    },

    /// The user interrupted a previous turn (Claude Code's
    /// `[Request interrupted by user]` sentinel).
    UserInterrupt {
        session_id: SessionId,
        event_uuid: String,
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
