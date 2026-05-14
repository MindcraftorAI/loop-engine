//! Per-session active-set state + the manifest section that
//! materializes it. Phase F C-F4 — extracted to its own module
//! per audit-fix close finding B-M1 (manifest/mod.rs exceeded the
//! 500-LOC file-size cap).
//!
//! Two things live here:
//!   * [`SessionState`] — descriptor the host sets before calling
//!     `assemble`. Engine reads ids; does NOT mutate.
//!   * [`populate_active_session_sections`] — engine-side resolver
//!     that turns id lists into trimmed [`SkillRef`] /
//!     [`PersonaRef`] / [`TeamRef`] views by reading the on-disk
//!     records. Stale ids (no on-disk record) increment
//!     `stats.session_section_skips`.

use tracing::warn;

use crate::engine::context::Context;
use crate::engine::error::EngineError;
use crate::engine::manifest::AssemblyStats;
use crate::engine::personas::PersonaRef;
use crate::engine::skills::SkillRef;
use crate::engine::storage::Storage;
use crate::engine::teams::TeamRef;

/// Phase F C-F4: per-session active-set descriptor. Host sets
/// before calling `assemble`; engine populates
/// `Manifest::active_skills/personas/teams` from the listed ids.
/// `None` skill/persona/team ids fields = no active set (manifest
/// section stays empty).
///
/// `#[non_exhaustive]` so future cycles add `active_user_id`,
/// session metadata, etc additively.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct SessionState {
    pub active_skill_ids: Vec<String>,
    pub active_persona_ids: Vec<String>,
    pub active_team_ids: Vec<String>,
}

impl SessionState {
    /// Empty state (no active skills/personas/teams). Same as
    /// `Self::default()`.
    pub fn empty() -> Self {
        Self::default()
    }
}

/// Populate the Phase F manifest sections. Returns (skills, personas,
/// teams). Bumps `stats.session_section_skips` for ids that don't
/// resolve.
pub(crate) async fn populate_active_session_sections(
    ctx: &Context,
    storage: &dyn Storage,
    state: &SessionState,
    stats: &mut AssemblyStats,
) -> Result<(Vec<SkillRef>, Vec<PersonaRef>, Vec<TeamRef>), EngineError> {
    let mut skills: Vec<SkillRef> = Vec::new();
    for id in &state.active_skill_ids {
        match crate::engine::skills::get_by_id(ctx, storage, id).await? {
            Some(s) => {
                let mut sref = SkillRef::new(
                    id.clone(),
                    s.frontmatter.name.clone(),
                    s.frontmatter.description.clone(),
                );
                sref.status = s.frontmatter.status;
                sref.activation = s.frontmatter.activation.clone();
                skills.push(sref);
            }
            None => {
                warn!(id = %id, "manifest: active skill id has no on-disk record; skipping");
                stats.session_section_skips += 1;
            }
        }
    }

    let mut personas: Vec<PersonaRef> = Vec::new();
    for id in &state.active_persona_ids {
        match crate::engine::personas::get_by_id(ctx, storage, id).await? {
            Some(p) => personas.push(PersonaRef {
                id: id.clone(),
                name: p.frontmatter.name,
                description: p.frontmatter.description,
                status: p.frontmatter.status,
            }),
            None => {
                warn!(id = %id, "manifest: active persona id has no on-disk record; skipping");
                stats.session_section_skips += 1;
            }
        }
    }

    let mut teams: Vec<TeamRef> = Vec::new();
    for id in &state.active_team_ids {
        match crate::engine::teams::get_by_id(ctx, storage, id).await? {
            Some(t) => teams.push(TeamRef {
                id: id.clone(),
                name: t.frontmatter.name,
                description: t.frontmatter.description,
                status: t.frontmatter.status,
                member_count: t.frontmatter.members.len(),
            }),
            None => {
                warn!(id = %id, "manifest: active team id has no on-disk record; skipping");
                stats.session_section_skips += 1;
            }
        }
    }

    Ok((skills, personas, teams))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_state_empty_is_default() {
        assert_eq!(
            SessionState::empty().active_skill_ids,
            SessionState::default().active_skill_ids,
        );
    }

    #[test]
    fn session_state_default_has_no_active_ids() {
        let s = SessionState::default();
        assert!(s.active_skill_ids.is_empty());
        assert!(s.active_persona_ids.is_empty());
        assert!(s.active_team_ids.is_empty());
    }
}
