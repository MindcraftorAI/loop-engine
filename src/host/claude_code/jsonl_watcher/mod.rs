//! JSONL watcher module — observes Claude Code session transcripts and
//! emits normalized `WatcherEvent`s to the Day 14+ sentiment loop.
//!
//! Design + decisions: `docs/research/day-13-learn-notes.md`.
//! Pre-research deliverable: `docs/research/day-13-pre-research.md`.
//!
//! Stack: `notify` (CC0-1.0, real-time FSEvents on macOS) + manual per-
//! file offset cursor (no linemux; no debouncer; raw real-time delivery
//! for the <100ms sentiment-loop latency budget).

mod cursor;
pub mod events;
pub mod parser;
mod runner;
pub mod source;

pub use cursor::{CursorAction, FileCursor};
pub use events::{WatcherEvent, PARSE_ERROR_REPORT_EVERY};
pub use parser::{parse_line, ParseOutcome, SkipReason};
pub use runner::{spawn_watcher, WatcherHandle};
pub use source::JsonlWatcherSource;
