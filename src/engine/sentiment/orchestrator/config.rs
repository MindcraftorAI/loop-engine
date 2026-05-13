//! Tunables for the orchestrator. Module-local per Day 16a OQ-D16a-4.

use std::ops::RangeInclusive;
use std::time::Duration;

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct OrchestratorConfig {
    /// Maximum recent turns retained for attribution + correction-window
    /// mining. Default 6 per design rules (4-6).
    pub recent_turn_capacity: usize,
    /// Minimum gap between sentiment signals for the same (session, lesson).
    /// Default 60s (audit-A2 rate-limit lineage).
    pub per_lesson_cooldown: Duration,
    /// Window inside which a UserInterrupt is considered a correction
    /// of the prior assistant turn. Default 30s (half of `per_lesson_cooldown`
    /// so a real interrupt-then-frustration sequence isn't suppressed).
    pub correction_window: Duration,
    /// Day 17 D4: HostVersion tripwire policy. Default `HostVersionPolicy::off()`
    /// — the tripwire is OFF until the daemon's config sets a tested_range.
    pub host_version_policy: HostVersionPolicy,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            recent_turn_capacity: 6,
            per_lesson_cooldown: Duration::from_secs(60),
            correction_window: Duration::from_secs(30),
            host_version_policy: HostVersionPolicy::off(),
        }
    }
}

/// Day 17 D4: action to take when an incoming `EngineEvent::UserTurn`
/// carries a `host_version` outside the configured tested_range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostVersionAction {
    /// Log a warning but still process the turn.
    Warn,
    /// Skip the entire turn — emit no signals, return
    /// `AbstainReason::UntestedHostVersion`.
    Abstain,
}

/// Day 17 D4: HostVersion tripwire policy. When `tested_range` is None,
/// the tripwire is OFF (default — appropriate for local-dev and any
/// build that hasn't pinned a tested host range).
///
/// String comparison is lexicographic — adequate for the dotted-decimal
/// versions Claude Code emits (e.g. `"2.1.139"` < `"2.1.40"` would be
/// WRONG with lex sort; semver-aware comparison is a Day 18+ refinement
/// when tested_range gets real values).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HostVersionPolicy {
    pub tested_range: Option<RangeInclusive<String>>,
    pub action: HostVersionAction,
}

impl HostVersionPolicy {
    /// Tripwire OFF — the default. No host_version check.
    pub fn off() -> Self {
        Self {
            tested_range: None,
            action: HostVersionAction::Warn,
        }
    }

    /// True when `version` is outside `tested_range`. Returns false if
    /// the policy is off (no range configured) — caller treats that as
    /// "no tripwire".
    pub fn is_out_of_range(&self, version: &str) -> bool {
        match &self.tested_range {
            None => false,
            Some(range) => !range.contains(&version.to_string()),
        }
    }
}
