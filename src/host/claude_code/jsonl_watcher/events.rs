//! Public output of the watcher module.
//!
//! `WatcherEvent` is the contract consumed by the Day 14+ sentiment loop.
//! Field shape locked per `docs/research/day-13-learn-notes.md`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};

/// A normalized event emitted by the JSONL watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatcherEvent {
    /// A real user-typed turn appended to a session transcript. Filtered
    /// to exclude tool_result content, meta events, sidechain (Task-spawned
    /// subagent) events, and slash-command sentinels.
    UserTurn {
        /// UUID of the Claude Code session (from the JSONL filename or
        /// the event's `sessionId` field — they match).
        session_id: String,
        /// Per-event UUID — useful as a downstream dedup key.
        event_uuid: String,
        /// Prior turn linkage (for correction-window mining in Day 15).
        parent_uuid: Option<String>,
        /// The project's current working directory, from the event payload.
        /// Resolved here so downstream consumers don't have to reverse-encode
        /// the project dir name (lossy on paths containing `-`).
        cwd: PathBuf,
        /// Optional git branch context.
        git_branch: Option<String>,
        /// Event timestamp from the payload, not the file's mtime.
        timestamp: DateTime<Utc>,
        /// The plain extracted user text.
        text: String,
        /// Claude Code version (e.g. "2.1.139") — captured so Day-N+ can
        /// detect shape drift via a tripwire if the value moves outside
        /// the known range.
        cc_version: String,
    },

    /// User pressed ESC mid-turn. The transcript contains a
    /// `[Request interrupted by user]` sentinel which we lift to its
    /// own event variant — it's the strongest auto-extractable sentiment
    /// signal in the entire pipeline (per Day 5 work).
    UserInterrupt {
        session_id: String,
        event_uuid: String,
        parent_uuid: Option<String>,
        timestamp: DateTime<Utc>,
        /// The assistant text the user interrupted, if recoverable from
        /// the immediately-preceding event. Day 13 doesn't track this
        /// (no cross-event lookback yet); leave None. Day 15 fills it
        /// when the orchestrator wires correction-window mining.
        interrupted_assistant_text: Option<String>,
    },

    /// New JSONL file appeared in the watched directory.
    SessionStarted {
        session_id: String,
        path: PathBuf,
        started_at: DateTime<Utc>,
    },

    /// JSONL file removed from the watched directory (rare — usually
    /// only happens if the user manually deletes a transcript).
    SessionEnded { session_id: String },

    /// A line failed JSON parse. Emitted in aggregate (one per
    /// `PARSE_ERROR_REPORT_EVERY` accumulated failures per file) to
    /// avoid spamming downstream consumers on a partial-write window.
    ParseError {
        session_id: String,
        offset: u64,
        raw_line: String,
        error: String,
    },
}

/// Cap on per-file ParseError emission frequency. After this many parse
/// failures since the last emission, the runner emits one aggregated event.
pub const PARSE_ERROR_REPORT_EVERY: u32 = 5;
