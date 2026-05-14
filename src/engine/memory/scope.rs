//! Phase F D-F8: `MemoryScope` enum + `MemoryScopeFilter`.
//!
//! Scope-tags a memory's reach: User (default), Team, Skill,
//! Project, Global. Phase E shipped single-scope (effectively
//! `User`); Phase F enables team-shared memory + skill-scoped
//! recall + project boundaries.
//!
//! The user-immunity invariant is SCOPE-ORTHOGONAL: a memory cited
//! by a user-authored lesson stays immune regardless of scope.
//! Compression that would orphan citations across scope boundaries
//! is blocked separately via `EngineError::CompressionScopeMismatch`
//! (predecessors with mixed scopes can't be compressed into one
//! memory — privacy boundary violation).

use serde::{Deserialize, Serialize};

/// The reach of a memory record. Default `User`. `#[non_exhaustive]`
/// so future cycles can add scopes (e.g. `Tenant`, `Organization`)
/// without a SemVer break.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MemoryScope {
    /// Default: memory belongs to ONE user, not shared.
    #[default]
    User,
    /// Shared with a team. Inner `String` is the team id (matches
    /// `engine::teams::Team::id` from Phase F C-F3).
    Team(String),
    /// Scoped to one skill — surfaces only when that skill is in
    /// `SessionState::active_skill_ids`.
    Skill(String),
    /// Scoped to a project (workspace / repo). Inner is the
    /// project id.
    Project(String),
    /// Universally available across all sessions, teams, projects.
    /// Rare; reserve for genuinely-broad capability hints.
    Global,
}

impl MemoryScope {
    /// Stringly typed discriminator for the variant — useful for
    /// CLI output + debug logs without exposing the inner id.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Team(_) => "team",
            Self::Skill(_) => "skill",
            Self::Project(_) => "project",
            Self::Global => "global",
        }
    }
}

/// Optional filter on `MemoryQuery` — return only memories matching
/// the filter. `None` (no filter) returns ALL scopes the caller can
/// see.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MemoryScopeFilter {
    /// Match exactly this scope. `Team("X")` matches only memories
    /// scoped to team X.
    Exact(MemoryScope),
    /// Match the variant DISCRIMINATOR only — `KindUser` matches
    /// any `User` memory; `KindTeam` matches any team-scoped memory
    /// regardless of which team.
    Kind(&'static str),
    /// Match any of these scopes.
    AnyOf(Vec<MemoryScope>),
}

impl MemoryScopeFilter {
    /// Predicate: does `scope` satisfy this filter?
    pub fn matches(&self, scope: &MemoryScope) -> bool {
        match self {
            Self::Exact(target) => target == scope,
            Self::Kind(kind) => scope.kind() == *kind,
            Self::AnyOf(scopes) => scopes.iter().any(|s| s == scope),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_user() {
        assert_eq!(MemoryScope::default(), MemoryScope::User);
    }

    #[test]
    fn kind_discriminator() {
        assert_eq!(MemoryScope::User.kind(), "user");
        assert_eq!(MemoryScope::Team("acme".into()).kind(), "team");
        assert_eq!(MemoryScope::Skill("fmt".into()).kind(), "skill");
        assert_eq!(MemoryScope::Project("p1".into()).kind(), "project");
        assert_eq!(MemoryScope::Global.kind(), "global");
    }

    #[test]
    fn serde_round_trip_user() {
        let s = serde_json::to_string(&MemoryScope::User).unwrap();
        assert_eq!(s, "\"user\"");
        let back: MemoryScope = serde_json::from_str(&s).unwrap();
        assert_eq!(back, MemoryScope::User);
    }

    #[test]
    fn serde_round_trip_team() {
        let s = serde_json::to_string(&MemoryScope::Team("acme".into())).unwrap();
        let back: MemoryScope = serde_json::from_str(&s).unwrap();
        assert_eq!(back, MemoryScope::Team("acme".into()));
    }

    #[test]
    fn filter_exact_matches() {
        let f = MemoryScopeFilter::Exact(MemoryScope::Team("a".into()));
        assert!(f.matches(&MemoryScope::Team("a".into())));
        assert!(!f.matches(&MemoryScope::Team("b".into())));
        assert!(!f.matches(&MemoryScope::User));
    }

    #[test]
    fn filter_kind_matches_discriminator_only() {
        let f = MemoryScopeFilter::Kind("team");
        assert!(f.matches(&MemoryScope::Team("a".into())));
        assert!(f.matches(&MemoryScope::Team("b".into())));
        assert!(!f.matches(&MemoryScope::User));
    }

    #[test]
    fn filter_any_of_matches_set() {
        let f = MemoryScopeFilter::AnyOf(vec![
            MemoryScope::Team("a".into()),
            MemoryScope::Global,
        ]);
        assert!(f.matches(&MemoryScope::Team("a".into())));
        assert!(f.matches(&MemoryScope::Global));
        assert!(!f.matches(&MemoryScope::User));
    }
}
