//! Typed storage key.
//!
//! Slash-delimited path-like, always normalized (no `..`, no leading
//! slash, no empty segments, no backslashes — abstract keys, not OS
//! paths). Constructed via typed builder methods per resource — never
//! from raw user input.
//!
//! Multi-tenant path routing lives HERE, not in `Storage` backends:
//! `tenant_id = "local"` collapses to today's on-disk layout
//! (`lessons/active/<id>.md`); other tenants prefix with
//! `tenants/<id>/users/<id>/...`.

use crate::engine::context::Context;

/// Opaque storage key. Always canonical: slash-separated, no leading
/// slash, no `..` traversals. Treat as a black box — only the
/// `Storage` backend interprets it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StorageKey(String);

impl StorageKey {
    /// Lesson file by status (`active`, `archived`, `superseded`, etc.).
    pub fn lesson(ctx: &Context, status: &str, id: &str) -> Self {
        let suffix = format!("lessons/{status}/{id}.md");
        Self(prefixed(ctx, &suffix))
    }

    /// Daemon PID file. Always under the tenant/user prefix (single
    /// process per user is the invariant).
    pub fn pid_file(ctx: &Context) -> Self {
        Self(prefixed(ctx, "daemon.pid"))
    }

    /// Daemon config file (`~/.loop/config.yaml` in single-user mode).
    pub fn config(ctx: &Context) -> Self {
        Self(prefixed(ctx, "config.yaml"))
    }

    /// Daemon log file.
    pub fn daemon_log(ctx: &Context) -> Self {
        Self(prefixed(ctx, "daemon.log"))
    }

    /// Sentiment signal file — per-(session, event) record. Day 16b
    /// emits one file per emitted signal; Day 17+ aggregates these
    /// into per-lesson signal arrays.
    pub fn sentiment_signal(ctx: &Context, session_id: &str, event_uuid: &str) -> Self {
        let suffix = format!("signals/{session_id}/{event_uuid}.yaml");
        Self(prefixed(ctx, &suffix))
    }

    /// Memory file (Phase E). Single-user: `memories/<id>.md`;
    /// multi-tenant: `tenants/.../memories/<id>.md`. Mirrors lesson
    /// layout but flat — memories don't have a status hierarchy.
    pub fn memory(ctx: &Context, id: &str) -> Self {
        let suffix = format!("memories/{id}.md");
        Self(prefixed(ctx, &suffix))
    }

    /// Prefix key for listing all memories (Phase E). Used by prune
    /// + recompute_citation_counts.
    pub fn memories_prefix(ctx: &Context) -> Self {
        Self(prefixed(ctx, "memories"))
    }

    /// Skill file (Phase F). Directory-per-skill matches Claude
    /// convention + allows future multi-file skills.
    /// Single-user: `skills/<id>/SKILL.md`.
    pub fn skill(ctx: &Context, id: &str) -> Self {
        let suffix = format!("skills/{id}/SKILL.md");
        Self(prefixed(ctx, &suffix))
    }

    /// Skill lesson-history audit sidecar (Phase G D-G6). Append-
    /// only YAML lines logging which lessons promoted into this
    /// skill, when, and by whom. Engine never edits past entries.
    /// `skills/<id>/lesson-history.yaml`.
    pub fn skill_history(ctx: &Context, id: &str) -> Self {
        let suffix = format!("skills/{id}/lesson-history.yaml");
        Self(prefixed(ctx, &suffix))
    }

    /// Prefix key for listing all skills.
    pub fn skills_prefix(ctx: &Context) -> Self {
        Self(prefixed(ctx, "skills"))
    }

    /// Persona file (Phase F). `personas/<id>/PERSONA.md`.
    pub fn persona(ctx: &Context, id: &str) -> Self {
        let suffix = format!("personas/{id}/PERSONA.md");
        Self(prefixed(ctx, &suffix))
    }

    /// Prefix key for listing all personas.
    pub fn personas_prefix(ctx: &Context) -> Self {
        Self(prefixed(ctx, "personas"))
    }

    /// Team file (Phase F). `teams/<id>/TEAM.md`.
    pub fn team(ctx: &Context, id: &str) -> Self {
        let suffix = format!("teams/{id}/TEAM.md");
        Self(prefixed(ctx, &suffix))
    }

    /// Prefix key for listing all teams.
    pub fn teams_prefix(ctx: &Context) -> Self {
        Self(prefixed(ctx, "teams"))
    }

    /// Prefix key for listing all lessons in a given status directory.
    /// Used by Day 17 solicitor for scan-by-status. Single-user:
    /// `lessons/<status>`; multi-tenant: `tenants/.../lessons/<status>`.
    pub fn lesson_status_prefix(ctx: &Context, status: &str) -> Self {
        let suffix = format!("lessons/{status}");
        Self(prefixed(ctx, &suffix))
    }

    /// Construct from a pre-validated path string. **Internal use only**
    /// — accessible to engine modules (e.g. `LocalFsStorage::list`
    /// constructing keys from directory entries). Not part of the
    /// public engine surface.
    ///
    /// Audit Day 14 m8: hard `assert!` not `debug_assert!` — the
    /// invariant check is cheap (three string ops on usually-short
    /// strings) and release builds must not silently propagate
    /// malformed keys.
    pub(crate) fn from_raw(s: String) -> Self {
        assert!(
            !s.starts_with('/') && !s.contains("..") && !s.contains('\\'),
            "invalid StorageKey: {s}"
        );
        Self(s)
    }

    /// View as a slash-delimited string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for StorageKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for StorageKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Compose the tenant/user prefix.
///
/// Single-user (`tenant_id = "local"`): no prefix, keys match today's
/// on-disk layout under `~/.loop/`.
///
/// Multi-tenant: prefixed with `tenants/<tenant>/users/<user>/`.
fn prefixed(ctx: &Context, suffix: &str) -> String {
    if ctx.tenant_id.as_str() == "local" {
        suffix.to_string()
    } else {
        format!(
            "tenants/{}/users/{}/{suffix}",
            ctx.tenant_id, ctx.user_id
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_user_lesson_key_matches_disk_layout() {
        let ctx = Context::single_user_local();
        let key = StorageKey::lesson(&ctx, "active", "les-abc123");
        assert_eq!(key.as_str(), "lessons/active/les-abc123.md");
    }

    #[test]
    fn multi_tenant_lesson_key_prefixes_correctly() {
        let ctx = Context::builder()
            .tenant_id("acme")
            .user_id("alice")
            .session_id("s1")
            .build();
        let key = StorageKey::lesson(&ctx, "active", "les-abc123");
        assert_eq!(
            key.as_str(),
            "tenants/acme/users/alice/lessons/active/les-abc123.md"
        );
    }

    #[test]
    fn pid_file_key_single_user() {
        let ctx = Context::single_user_local();
        assert_eq!(StorageKey::pid_file(&ctx).as_str(), "daemon.pid");
    }

    #[test]
    #[should_panic(expected = "invalid StorageKey")]
    fn from_raw_rejects_leading_slash() {
        let _ = StorageKey::from_raw("/absolute".into());
    }

    #[test]
    #[should_panic(expected = "invalid StorageKey")]
    fn from_raw_rejects_dotdot_traversal() {
        let _ = StorageKey::from_raw("lessons/../etc/passwd".into());
    }

    #[test]
    #[should_panic(expected = "invalid StorageKey")]
    fn from_raw_rejects_backslash() {
        let _ = StorageKey::from_raw("lessons\\active\\foo".into());
    }
}
