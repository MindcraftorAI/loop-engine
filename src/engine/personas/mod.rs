//! Personas — identity descriptors. Phase F D-F5.
//!
//! A persona carries identity attributes (display name, voice/style
//! hints, default LLM model, system prompt fragment). Distinct from
//! skills (capability packages with hooks) and teams (groupings).
//!
//! Engine stores; host activates per-session via `SessionState`.

use serde::{Deserialize, Serialize};

pub mod store;
pub use store::{archive, delete, get_by_id, insert, list, update};

/// Phase F D-F5: lifecycle status — matches skill / lesson patterns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PersonaStatus {
    #[default]
    Draft,
    Active,
    Archived,
}

/// Persona YAML frontmatter. Body carries the voice/style/system-
/// prompt content as markdown sections.
///
/// **Not `#[non_exhaustive]`** — serialized YAML shape (same trade-
/// off as `MemoryFrontmatter` per D-E1). Growth via `#[serde(default)]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonaFrontmatter {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub status: PersonaStatus,
    #[serde(default)]
    pub authored_by: crate::engine::yaml::Authorship,
}

impl PersonaFrontmatter {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            status: PersonaStatus::default(),
            authored_by: crate::engine::yaml::Authorship::default(),
        }
    }
}

/// In-memory persona = frontmatter + body.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Persona {
    pub frontmatter: PersonaFrontmatter,
    pub body: String,
}

impl Persona {
    pub fn new(frontmatter: PersonaFrontmatter, body: impl Into<String>) -> Self {
        Self {
            frontmatter,
            body: body.into(),
        }
    }
}

/// Trimmed manifest view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PersonaRef {
    pub id: String,
    pub name: String,
    pub description: String,
    pub status: PersonaStatus,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::yaml::Authorship;

    #[test]
    fn persona_frontmatter_new_populates_defaults() {
        let p = PersonaFrontmatter::new("pers-aaaa", "Maya", "patient mentor");
        assert_eq!(p.id, "pers-aaaa");
        assert_eq!(p.status, PersonaStatus::Draft);
        assert_eq!(p.authored_by, Authorship::Llm);
    }

    #[test]
    fn persona_serde_round_trip() {
        let p = PersonaFrontmatter::new("pers-aaaa", "Maya", "patient mentor");
        let yaml = serde_yml::to_string(&p).unwrap();
        let back: PersonaFrontmatter = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn persona_status_default_is_draft() {
        assert_eq!(PersonaStatus::default(), PersonaStatus::Draft);
    }
}
