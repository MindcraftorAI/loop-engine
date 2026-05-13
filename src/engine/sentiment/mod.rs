//! Sentiment layer — host-agnostic.
//!
//! Three components ship in Day 15:
//! - [`pretrigger`] — fast regex pre-filter; cheap reject for non-signal text
//! - [`classifier`] — sealed async trait `SentimentClassifier`; production
//!   impl lives in a host adapter (Day 16+ — Anthropic Haiku, etc.)
//! - [`attribution`] — pure-function five-pass attribution algorithm
//!
//! Day 16 adds `orchestrator` (per-session state machine + rate limiting),
//! Day 17 adds `solicitor` (engine-level integration tests + tripwires).
//!
//! All types are described in `docs/research/day-15-learn-notes.md` (D1-D15)
//! and `docs/research/day-15-pre-research.md`.

pub mod attribution;
pub mod classifier;
pub mod orchestrator;
pub mod pretrigger;
pub mod signals;
pub mod types;

pub use attribution::{attribute_signal, attribute_signal_with_fallback, Attribution};
pub use classifier::{ClassifierError, SentimentClassifier};
// Audit M6 fix: `SessionState` / `SessionPhase` are internal plumbing
// and not part of the engine's public surface. Only `Orchestrator` +
// `OrchestratorConfig` are exposed.
pub use orchestrator::{Orchestrator, OrchestratorConfig};
pub use pretrigger::Pretrigger;
pub use signals::{
    AbstainReason, LoggingSignalWriter, OrchestratorOutput, SentimentSignal, SignalWriteError,
    SignalWriter,
};
pub use types::{
    AttributionConfidence, AttributionMethod, CalibratedConfidence, ClassificationRequest,
    ClassifierConfidence, Hazard, ItemClassification, LoadedItem, LoadedItemId, LoadedItemKind,
    Polarity, RawClassification, RecentTurn, TurnRole,
};
