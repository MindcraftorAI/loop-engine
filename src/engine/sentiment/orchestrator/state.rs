//! Per-session orchestrator state.
//!
//! `pub(crate)` — internal plumbing. External callers go through
//! [`super::Orchestrator`]'s API.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use crate::engine::sentiment::types::{LoadedItem, LoadedItemId, RecentTurn, TurnRole};

#[derive(Debug)]
#[non_exhaustive]
pub(crate) struct SessionState {
    pub recent_turns: VecDeque<RecentTurn>,
    pub rate_limit: HashMap<LoadedItemId, Instant>,
    pub phase: SessionPhase,
    pub turn_count: u64,
    /// Wall-clock of the most-recent observed assistant turn — used by
    /// correction-window mining.
    pub last_assistant_turn_at: Option<Instant>,
    /// Manifest items active for this session. Populated by callers via
    /// [`super::Orchestrator::update_manifest`]; consumed when building
    /// `ClassificationRequest`s + by attribution.
    ///
    /// Day 16a: this is the surface the manifest-assembly layer
    /// (Day 16b+) writes to. The orchestrator itself never derives or
    /// invents manifest content.
    pub loaded_items: Vec<LoadedItem>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            recent_turns: VecDeque::new(),
            rate_limit: HashMap::new(),
            phase: SessionPhase::Idle,
            turn_count: 0,
            last_assistant_turn_at: None,
            loaded_items: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum SessionPhase {
    Idle,
    AwaitingClassifier {
        utterance: String,
        started_at: Instant,
    },
}

/// Append a turn to the bounded recent-turns ring buffer. Updates
/// `last_assistant_turn_at` when the turn role is Assistant.
pub(crate) fn push_turn(state: &mut SessionState, capacity: usize, turn: RecentTurn) {
    if turn.role == TurnRole::Assistant {
        state.last_assistant_turn_at = Some(Instant::now());
    }
    state.recent_turns.push_back(turn);
    while state.recent_turns.len() > capacity {
        state.recent_turns.pop_front();
    }
}
