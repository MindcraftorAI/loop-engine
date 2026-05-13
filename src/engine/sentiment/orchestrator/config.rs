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
/// Phase A C1 (Day 17 m4 audit fix): semver-aware comparison when all
/// three strings (low/high bounds + incoming version) parse as semver;
/// falls back to lexicographic compare with a tracing warning when any
/// string fails to parse. The fallback preserves pre-Phase-A behavior
/// for non-semver-shaped strings.
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
    /// the policy is off (no range configured).
    pub fn is_out_of_range(&self, version: &str) -> bool {
        let Some(range) = &self.tested_range else {
            return false;
        };
        // Try semver-aware compare first.
        if let (Ok(lo), Ok(hi), Ok(v)) = (
            semver::Version::parse(range.start()),
            semver::Version::parse(range.end()),
            semver::Version::parse(version),
        ) {
            return v < lo || v > hi;
        }
        // Fallback: lex compare with a one-shot warning.
        tracing::warn!(
            version = %version,
            range_lo = %range.start(),
            range_hi = %range.end(),
            "host version tripwire fell back to lexicographic comparison (non-semver strings)"
        );
        !range.contains(&version.to_string())
    }
}

#[cfg(test)]
mod host_version_policy_tests {
    use super::*;

    #[test]
    fn off_policy_never_out_of_range() {
        let p = HostVersionPolicy::off();
        assert!(!p.is_out_of_range("any-string"));
        assert!(!p.is_out_of_range("2.1.139"));
    }

    #[test]
    fn semver_in_range_returns_false() {
        let p = HostVersionPolicy {
            tested_range: Some("2.0.0".to_string()..="2.1.999".to_string()),
            action: HostVersionAction::Abstain,
        };
        assert!(!p.is_out_of_range("2.0.0"));
        assert!(!p.is_out_of_range("2.1.5"));
        assert!(!p.is_out_of_range("2.1.999"));
    }

    /// Phase A C1 regression: the Day 17 m4 gotcha — `"2.1.139" < "2.1.40"`
    /// is TRUE under lex compare (wrong) but FALSE under semver compare
    /// (right). With semver-aware comparison "2.1.139" sorts correctly
    /// inside the [2.1.40, 2.1.200] range.
    #[test]
    fn semver_resolves_dotted_decimal_gotcha() {
        let p = HostVersionPolicy {
            tested_range: Some("2.1.40".to_string()..="2.1.200".to_string()),
            action: HostVersionAction::Abstain,
        };
        assert!(!p.is_out_of_range("2.1.139"));
    }

    #[test]
    fn semver_out_of_range_returns_true() {
        let p = HostVersionPolicy {
            tested_range: Some("2.0.0".to_string()..="2.1.999".to_string()),
            action: HostVersionAction::Abstain,
        };
        assert!(p.is_out_of_range("1.9.9"));
        assert!(p.is_out_of_range("3.0.0"));
    }

    /// Non-semver strings fall back to lex compare.
    #[test]
    fn non_semver_falls_back_to_lex() {
        let p = HostVersionPolicy {
            tested_range: Some("alpha".to_string()..="zulu".to_string()),
            action: HostVersionAction::Abstain,
        };
        assert!(!p.is_out_of_range("delta"));
        assert!(p.is_out_of_range("aaa"));
    }
}
