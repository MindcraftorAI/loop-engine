//! Lesson layer — file-canonical readers/writers.
//!
//! Mirrors the TS side `core/src/lessons/loader.ts` + `signals.ts` for the
//! daemon's needs:
//!   - `getLessonById` → `get_lesson_by_id`
//!   - `recordLessonSentimentSignal` → `record_sentiment_signal`
//!
//! Status-as-directory per ADR-0010: a lesson's status is determined by
//! the parent directory name, not by the frontmatter `status` field. The
//! frontmatter field is portability metadata; the directory is truth.
//!
//! Cross-process coordination: writes take an advisory flock (`fd-lock`)
//! on the lesson file before read-modify-write. The TS side currently
//! uses an in-process mutex only; if both processes adopt flock, full
//! safety. For now, daemon-side flock prevents two daemon mutations from
//! racing, and the atomic-rename of write_lesson means the TS-side worst
//! case is a lost update (not a corrupted file).

pub mod loader;
pub mod lock;
pub mod signals;

// Canonical async API (Phase A C4 + C5):
pub use loader::{get_by_id, LessonFullContent, LoadedLesson};
pub use signals::{record_signal, SignalPolarity};

// Deprecated sync wrappers — retained for backward compat through Phase
// A. Retire in Phase F or G when the daemon binary's wiring is fully
// async. The re-exports themselves carry the deprecation note.
#[allow(deprecated)]
pub use loader::get_lesson_by_id;
#[allow(deprecated)]
pub use signals::record_sentiment_signal;
