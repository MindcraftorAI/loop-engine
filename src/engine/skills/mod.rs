//! Skills — scoped capability packages per Anthropic's Claude Skills
//! model. Phase F D-F1.
//!
//! The engine STORES skill records (frontmatter + body + hooks
//! metadata). The HOST adapter (future monolith) activates + executes
//! hooks. Engine boundary: serialize/deserialize the skill schema,
//! provide CRUD, surface skills in the manifest's `active_skills`
//! section when the host has marked them active via `SessionState`.
//!
//! Hooks are stored as data (`HashMap<HookEvent, Vec<HookMatcherGroup>>`),
//! NOT executed by the engine. The 5 known handler types map to a
//! tagged enum [`HookHandler`]; unknown event names accept open-
//! ended via the [`HookEvent`] newtype so the schema forward-compats
//! with Anthropic's growing event list (27+ as of 2026-05-14).
//!
//! See `phase-f-pre-research.md` §2 for the Claude Skills
//! investigation (sources cited).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub mod hooks;
pub mod r#ref;
pub mod store;

pub use hooks::{HookEvent, HookHandler, HookMatcherGroup};
pub use r#ref::SkillRef;
pub use store::{archive, delete, get_by_id, insert, list, update};

/// Phase F D-F4: how a skill becomes active in a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ActivationMode {
    /// Always-on once loaded (host marks active at session start).
    Auto,
    /// Engine matches one of the path globs against the active
    /// context; host opts in when a match fires.
    PathTriggered(Vec<String>),
    /// Host explicitly activates; no engine-side trigger logic.
    #[default]
    UserTriggered,
}

/// TS-parity skill type — categorical hint for host UI grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SkillType {
    Generative,
    Analytical,
}

/// Claude-parity effort level. `Other(String)` keeps forward-compat
/// with new levels Anthropic might add.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EffortLevel {
    Low,
    Medium,
    High,
    Other(String),
}

/// Claude-parity context mode for skill execution. `Fork` runs the
/// skill in a forked subagent context (see `Skill::agent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContextMode {
    Inherit,
    Fork,
}

/// Phase F D-F10: skill lifecycle status — matches lesson
/// lifecycle pattern.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SkillStatus {
    #[default]
    Draft,
    Active,
    Archived,
}

/// Skill frontmatter — Claude-parity + Loop additions.
///
/// **Not `#[non_exhaustive]`** — this IS the serialized YAML shape
/// (same trade-off as `MemoryFrontmatter` per D-E1). Growth happens
/// via `#[serde(default)]` on additive fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillFrontmatter {
    /// Display name. Lowercase + hyphens + numbers, max 64 chars
    /// (Claude validation rule; engine enforces on insert).
    pub name: String,
    /// What the skill does + when to use it. Claude truncates at
    /// 1024 chars; engine warns on insert if longer.
    pub description: String,
    /// TS-parity categorical hint.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub skill_type: Option<SkillType>,
    /// Free-form version string. Host decides semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Claude-parity: when to invoke.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    /// Claude-parity: argument hint for autocomplete.
    #[serde(default, rename = "argument-hint", skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    /// Claude-parity: named positional args.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<String>,
    /// Claude-parity: tools usable without permission prompts.
    #[serde(default, rename = "allowed-tools", skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
    /// Claude-parity: model override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Claude-parity: effort hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortLevel>,
    /// Claude-parity: forked-context flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextMode>,
    /// Claude-parity: subagent type when `context = fork`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Claude-parity: per-event hook map. Phase F D-F2 — the
    /// load-bearing field. Typed enum over 5 known handler types;
    /// open-ended event-name newtype for forward-compat.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub hooks: HashMap<HookEvent, Vec<HookMatcherGroup>>,
    /// Claude-parity: activation path globs (used when activation
    /// mode is PathTriggered).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Loop-specific: how the skill activates.
    #[serde(default)]
    pub activation: ActivationMode,
    /// Loop-specific: lifecycle status.
    #[serde(default)]
    pub status: SkillStatus,
    /// Loop-specific: authorship. Drives the eviction-immunity
    /// invariant (user-authored skills can't be auto-archived).
    #[serde(default)]
    pub authored_by: crate::engine::yaml::Authorship,
}

impl SkillFrontmatter {
    /// Construct with required fields; optionals default.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            skill_type: None,
            version: None,
            when_to_use: None,
            argument_hint: None,
            arguments: Vec::new(),
            allowed_tools: Vec::new(),
            model: None,
            effort: None,
            context: None,
            agent: None,
            hooks: HashMap::new(),
            paths: Vec::new(),
            activation: ActivationMode::default(),
            status: SkillStatus::default(),
            authored_by: crate::engine::yaml::Authorship::default(),
        }
    }
}

/// In-memory view of a Skill: frontmatter + body (markdown
/// instructions). Body is opaque to the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Skill {
    pub frontmatter: SkillFrontmatter,
    pub body: String,
}

impl Skill {
    pub fn new(frontmatter: SkillFrontmatter, body: impl Into<String>) -> Self {
        Self {
            frontmatter,
            body: body.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::yaml::Authorship;

    #[test]
    fn skill_frontmatter_new_populates_defaults() {
        let s = SkillFrontmatter::new("formatter", "auto-format on save");
        assert_eq!(s.name, "formatter");
        assert_eq!(s.description, "auto-format on save");
        assert!(s.hooks.is_empty());
        assert_eq!(s.status, SkillStatus::Draft);
        assert_eq!(s.authored_by, Authorship::Llm);
        assert!(matches!(s.activation, ActivationMode::UserTriggered));
    }

    #[test]
    fn skill_status_default_is_draft() {
        assert_eq!(SkillStatus::default(), SkillStatus::Draft);
    }

    #[test]
    fn activation_mode_default_is_user_triggered() {
        assert!(matches!(ActivationMode::default(), ActivationMode::UserTriggered));
    }

    #[test]
    fn skill_serde_round_trip_minimal() {
        let s = SkillFrontmatter::new("test", "desc");
        let yaml = serde_yml::to_string(&s).unwrap();
        let back: SkillFrontmatter = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn skill_type_serde_snake_case() {
        let s = serde_json::to_string(&SkillType::Generative).unwrap();
        assert_eq!(s, "\"generative\"");
        let back: SkillType = serde_json::from_str(&s).unwrap();
        assert_eq!(back, SkillType::Generative);
    }

    #[test]
    fn effort_level_other_variant_round_trip() {
        let e = EffortLevel::Other("ultra".to_string());
        let s = serde_json::to_string(&e).unwrap();
        let back: EffortLevel = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
    }
}
