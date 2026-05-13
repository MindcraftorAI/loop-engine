//! YAML reader for lesson frontmatter.
//!
//! Thin wrapper over `serde_yml` for the parse direction. Parsing is
//! the trustworthy half of the deprecated-serde_yaml situation — the
//! library deserializes correctly on already-shipped versions. Our risk
//! surface is the writer (which we hand-roll).

use anyhow::{Context, Result};

use super::schema::LessonFrontmatter;

// Test helpers — let integration tests build LessonFrontmatter without
// re-importing every enum variant. Underscored to flag as test-support.
#[doc(hidden)]
pub fn __expose_confidence_inferred() -> super::schema::Confidence {
    super::schema::Confidence::Inferred
}

#[doc(hidden)]
pub fn __expose_generated_by_llm() -> super::schema::GeneratedBy {
    super::schema::GeneratedBy::Llm
}

/// Parse a YAML frontmatter block (the text between the `---` delimiters,
/// NOT including the delimiters themselves) into a `LessonFrontmatter`.
pub fn parse_lesson_frontmatter(yaml: &str) -> Result<LessonFrontmatter> {
    serde_yml::from_str::<LessonFrontmatter>(yaml).context("parsing lesson frontmatter")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yaml::schema::{Confidence, GeneratedBy, LessonStatus};

    #[test]
    fn parses_minimum_lesson() {
        let yaml = "\
id: les-aaaaaaaa
description: minimal
status: active
created_at: 2026-05-13T00:00:00.000Z
applied_count: 0
thumbs_up_count: 0
thumbs_down_count: 0
external_signal_sources: []
";
        let fm = parse_lesson_frontmatter(yaml).unwrap();
        assert_eq!(fm.id, "les-aaaaaaaa");
        assert_eq!(fm.description, "minimal");
        assert_eq!(fm.status, LessonStatus::Active);
        assert_eq!(fm.applied_count, 0);
        assert!(fm.external_signal_sources.is_empty());
        assert!(fm.causal_narrative.is_none());
    }

    #[test]
    fn parses_real_world_lesson_with_narrative() {
        // Shape mirrors ~/.loop/lessons/active/les-dfs24ojt.md
        let yaml = "\
id: les-dfs24ojt
description: \"MindCraftor lesson model: every interaction IS data\"
status: active
created_at: 2026-05-12T20:39:55.314Z
applied_count: 0
thumbs_up_count: 0
thumbs_down_count: 0
external_signal_sources: []
causal_narrative:
  trigger: unattested
  failure_mode: unattested
  correction: \"User correction\"
  confidence: speculative
  evidence_refs: []
  generated_by: user
  generated_at: 2026-05-12T20:39:55.314Z
";
        let fm = parse_lesson_frontmatter(yaml).unwrap();
        let cn = fm.causal_narrative.expect("narrative");
        assert_eq!(cn.confidence, Confidence::Speculative);
        assert_eq!(cn.generated_by, GeneratedBy::User);
        assert!(cn.evidence_refs.is_empty());
    }

    #[test]
    fn parses_external_signal_sources_array() {
        let yaml = "\
id: les-bbbbbbbb
description: signals
status: active
created_at: 2026-05-13T00:00:00.000Z
applied_count: 1
thumbs_up_count: 0
thumbs_down_count: 0
external_signal_sources:
  - user_thumbs_up
  - sentiment_positive
";
        let fm = parse_lesson_frontmatter(yaml).unwrap();
        assert_eq!(
            fm.external_signal_sources,
            vec!["user_thumbs_up", "sentiment_positive"]
        );
    }

    #[test]
    fn parses_ingest_provenance() {
        let yaml = "\
id: les-cccccccc
description: ingested
status: active
created_at: 2026-05-13T00:00:00.000Z
applied_count: 0
thumbs_up_count: 0
thumbs_down_count: 0
external_signal_sources: []
ingest_provenance:
  source_type: auto_memory
  source_path: /path/to/memory.md
  source_external_id: ext-1
  extracted_at: 2026-05-13T00:00:00.000Z
";
        let fm = parse_lesson_frontmatter(yaml).unwrap();
        let prov = fm.ingest_provenance.expect("provenance");
        assert_eq!(prov.source_path, "/path/to/memory.md");
        assert_eq!(prov.source_external_id.as_deref(), Some("ext-1"));
    }

    #[test]
    fn rejects_invalid_status() {
        let yaml = "\
id: les-dddddddd
description: bad
status: not-a-real-status
created_at: 2026-05-13T00:00:00.000Z
";
        assert!(parse_lesson_frontmatter(yaml).is_err());
    }

    #[test]
    fn rejects_invalid_confidence() {
        let yaml = "\
id: les-eeeeeeee
description: bad narrative
status: active
created_at: 2026-05-13T00:00:00.000Z
applied_count: 0
thumbs_up_count: 0
thumbs_down_count: 0
external_signal_sources: []
causal_narrative:
  trigger: t
  failure_mode: f
  correction: c
  confidence: definitely-not-valid
  evidence_refs: []
  generated_by: user
  generated_at: 2026-05-13T00:00:00.000Z
";
        assert!(parse_lesson_frontmatter(yaml).is_err());
    }
}
