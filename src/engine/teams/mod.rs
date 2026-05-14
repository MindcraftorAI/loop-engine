//! Teams — groupings of personas/skills/users. Phase F D-F6.
//!
//! Engine stores; host activates per-session. Membership is typed
//! via [`TeamMember`] (discriminated by `kind`). TS-parity flat-slug
//! lists deserialize via the `from_string` migrator (treats bare
//! slugs as `TeamMember::User`).

use serde::de::Deserializer;
use serde::{Deserialize, Serialize};

pub mod store;
pub use store::{archive, delete, get_by_id, insert, list, update};

/// Phase F D-F6: typed team member discriminator.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TeamMember {
    Persona(String),
    Skill(String),
    User(String),
}

impl TeamMember {
    pub fn id(&self) -> &str {
        match self {
            Self::Persona(s) | Self::Skill(s) | Self::User(s) => s.as_str(),
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Persona(_) => "persona",
            Self::Skill(_) => "skill",
            Self::User(_) => "user",
        }
    }
}

/// Phase F D-F10: lifecycle status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TeamStatus {
    #[default]
    Draft,
    Active,
    Archived,
}

/// Team frontmatter. Body is free-form markdown describing the
/// team's purpose / charter / norms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TeamFrontmatter {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Typed members. See [`TeamMember`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<TeamMember>,
    #[serde(default)]
    pub status: TeamStatus,
    #[serde(default)]
    pub authored_by: crate::engine::yaml::Authorship,
}

/// Custom Deserialize accepts BOTH the typed form (D-F6) AND the
/// TS-parity flat-slug list form (each bare string deserializes as
/// `TeamMember::User`). Writes always emit the typed form.
impl<'de> Deserialize<'de> for TeamFrontmatter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum MemberShim {
            Typed(TeamMember),
            Slug(String),
        }
        #[derive(Deserialize)]
        struct Raw {
            id: String,
            name: String,
            description: String,
            #[serde(default)]
            members: Vec<MemberShim>,
            #[serde(default)]
            status: TeamStatus,
            #[serde(default)]
            authored_by: crate::engine::yaml::Authorship,
        }
        let raw = Raw::deserialize(deserializer)?;
        let members = raw
            .members
            .into_iter()
            .map(|m| match m {
                MemberShim::Typed(t) => t,
                MemberShim::Slug(s) => TeamMember::User(s),
            })
            .collect();
        Ok(Self {
            id: raw.id,
            name: raw.name,
            description: raw.description,
            members,
            status: raw.status,
            authored_by: raw.authored_by,
        })
    }
}

impl TeamFrontmatter {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            members: Vec::new(),
            status: TeamStatus::default(),
            authored_by: crate::engine::yaml::Authorship::default(),
        }
    }
}

/// In-memory team = frontmatter + body.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Team {
    pub frontmatter: TeamFrontmatter,
    pub body: String,
}

impl Team {
    pub fn new(frontmatter: TeamFrontmatter, body: impl Into<String>) -> Self {
        Self {
            frontmatter,
            body: body.into(),
        }
    }
}

/// Trimmed manifest view.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TeamRef {
    pub id: String,
    pub name: String,
    pub description: String,
    pub status: TeamStatus,
    pub member_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_member_id_and_kind() {
        let m = TeamMember::Persona("pers-aaaa".to_string());
        assert_eq!(m.id(), "pers-aaaa");
        assert_eq!(m.kind(), "persona");
    }

    #[test]
    fn team_frontmatter_round_trips_typed_form() {
        let mut t = TeamFrontmatter::new("tm-aaaa", "Eng Team", "engineers");
        t.members = vec![
            TeamMember::Persona("pers-aaaa".into()),
            TeamMember::Skill("skl-bbbb".into()),
            TeamMember::User("u-cccc".into()),
        ];
        let yaml = serde_yml::to_string(&t).unwrap();
        let back: TeamFrontmatter = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(back.members, t.members);
    }

    #[test]
    fn team_frontmatter_migrates_legacy_bare_slug_members() {
        // TS-parity inbound: members is a flat list of bare strings.
        let yaml = r#"
id: tm-aaaa
name: Legacy Team
description: ts-shaped
members:
  - user-1
  - user-2
"#;
        let t: TeamFrontmatter = serde_yml::from_str(yaml).unwrap();
        assert_eq!(t.members.len(), 2);
        assert!(matches!(t.members[0], TeamMember::User(_)));
        assert_eq!(t.members[0].id(), "user-1");
    }

    #[test]
    fn team_member_serde_tagged_form() {
        let m = TeamMember::Skill("skl-aaaa".to_string());
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"kind\":\"skill\""));
        assert!(json.contains("\"id\":\"skl-aaaa\""));
        let back: TeamMember = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }
}
