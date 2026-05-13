//! Tunables for the orchestrator. Module-local per Day 16a OQ-D16a-4.

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
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            recent_turn_capacity: 6,
            per_lesson_cooldown: Duration::from_secs(60),
            correction_window: Duration::from_secs(30),
        }
    }
}
