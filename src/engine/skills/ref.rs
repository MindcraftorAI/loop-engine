//! `SkillRef` — trimmed manifest-section view of a Skill.
//!
//! Phase F D-F9: mirrors `ActiveLesson` + `MemoryRef` trim pattern.
//! Host gets enough to render + an id to fetch the full Skill on
//! demand.

use serde::{Deserialize, Serialize};

use crate::engine::skills::{ActivationMode, SkillStatus};

/// Trimmed Skill view for `Manifest::active_skills`. `#[non_exhaustive]`
/// — future cycles add fields without SemVer break.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SkillRef {
    pub id: String,
    pub name: String,
    pub description: String,
    pub status: SkillStatus,
    pub activation: ActivationMode,
}

impl SkillRef {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            status: SkillStatus::default(),
            activation: ActivationMode::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_ref_round_trip() {
        let r = SkillRef::new("skl-aaaa", "fmt", "auto-format on save");
        let s = serde_json::to_string(&r).unwrap();
        let back: SkillRef = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }
}
