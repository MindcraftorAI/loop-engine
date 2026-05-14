//! YAML writer for lesson frontmatter.
//!
//! Hand-rolled to match the TS-side `yaml` library's output under the
//! pinned options `{blockQuote: 'literal', lineWidth: 0,
//! defaultStringType: 'PLAIN', defaultKeyType: 'PLAIN'}`. Scalar style
//! decisions live in `scalar.rs`; this file owns field ordering and
//! the per-section emit logic.
//!
//! Field emission order MUST match TS's load-path order in
//! `core/src/lessons/loader.ts::tryLoadLessonFile`. Mismatched order
//! causes git-diff churn on every cross-process mutation.

use super::scalar::render_scalar;
use super::schema::{CausalNarrative, EvidenceRef, IngestProvenance, LessonFrontmatter};

/// Render a `LessonFrontmatter` to YAML text that goes between the
/// `---` delimiters. Caller wraps in delimiters via `combine_frontmatter`.
pub fn serialize_lesson_frontmatter(fm: &LessonFrontmatter) -> String {
    let mut out = String::with_capacity(1024);

    // Always-present core (in TS load-path order)
    emit_plain(&mut out, "id", &fm.id);
    emit_string(&mut out, "description", &fm.description);
    emit_plain(&mut out, "status", fm.status.as_str());
    emit_plain(&mut out, "created_at", &fm.created_at);

    // Conditional block 1: narrative, skill, feedback (TS load-path order)
    if let Some(cn) = &fm.causal_narrative {
        emit_causal_narrative(&mut out, cn);
    }
    if let Some(v) = &fm.target_skill {
        emit_string(&mut out, "target_skill", v);
    }
    if let Some(ids) = &fm.source_feedback_ids {
        emit_i64_array(&mut out, "source_feedback_ids", ids);
    }

    // Counters + external signal sources
    emit_u64(&mut out, "applied_count", fm.applied_count);
    if let Some(v) = &fm.last_applied_at {
        emit_plain(&mut out, "last_applied_at", v);
    }
    emit_u64(&mut out, "thumbs_up_count", fm.thumbs_up_count);
    emit_u64(&mut out, "thumbs_down_count", fm.thumbs_down_count);
    emit_string_array(
        &mut out,
        "external_signal_sources",
        &fm.external_signal_sources,
    );
    // Phase G D-G3 (v0.4): only emit when populated, so v0.3.x lessons
    // round-trip without acquiring an empty array on first rewrite.
    if !fm.applied_session_ids.is_empty() {
        emit_string_array(&mut out, "applied_session_ids", &fm.applied_session_ids);
    }

    // Promotion + supersession
    if let Some(v) = &fm.promotion_eligible_at {
        emit_plain(&mut out, "promotion_eligible_at", v);
    }
    if let Some(v) = &fm.superseded_by {
        emit_string(&mut out, "superseded_by", v);
    }
    if let Some(v) = &fm.superseded_at {
        emit_plain(&mut out, "superseded_at", v);
    }

    // Ingest provenance (Day 2)
    if let Some(p) = &fm.ingest_provenance {
        emit_ingest_provenance(&mut out, p);
    }

    // Phase E D-E11: authored_by (emit unconditionally so lessons
    // converge to the new shape on first write).
    emit_plain(&mut out, "authored_by", fm.authored_by.as_str());

    // Always last
    if let Some(v) = &fm.updated_at {
        emit_plain(&mut out, "updated_at", v);
    }

    out
}

// ── Emit helpers ────────────────────────────────────────────────────────

fn emit_u64(out: &mut String, key: &str, value: u64) {
    out.push_str(key);
    out.push_str(": ");
    out.push_str(&value.to_string());
    out.push('\n');
}

/// Emit a plain unquoted value. Use only for values guaranteed safe in
/// plain style (enums, numbers-as-strings, ISO timestamps). For arbitrary
/// strings, use `emit_string` which routes through scalar-style detection.
fn emit_plain(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(": ");
    out.push_str(value);
    out.push('\n');
}

fn emit_string(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(": ");
    out.push_str(&render_scalar(value, 2));
    out.push('\n');
}

fn emit_string_array(out: &mut String, key: &str, items: &[String]) {
    if items.is_empty() {
        out.push_str(key);
        out.push_str(": []\n");
        return;
    }
    out.push_str(key);
    out.push_str(":\n");
    for item in items {
        out.push_str("  - ");
        out.push_str(&render_scalar(item, 4));
        out.push('\n');
    }
}

fn emit_i64_array(out: &mut String, key: &str, items: &[i64]) {
    if items.is_empty() {
        out.push_str(key);
        out.push_str(": []\n");
        return;
    }
    out.push_str(key);
    out.push_str(":\n");
    for item in items {
        out.push_str("  - ");
        out.push_str(&item.to_string());
        out.push('\n');
    }
}

fn emit_causal_narrative(out: &mut String, cn: &CausalNarrative) {
    out.push_str("causal_narrative:\n");
    emit_nested_string(out, "trigger", &cn.trigger);
    emit_nested_string(out, "failure_mode", &cn.failure_mode);
    emit_nested_string(out, "correction", &cn.correction);
    out.push_str("  confidence: ");
    out.push_str(cn.confidence.as_str());
    out.push('\n');
    if cn.evidence_refs.is_empty() {
        out.push_str("  evidence_refs: []\n");
    } else {
        out.push_str("  evidence_refs:\n");
        for r in &cn.evidence_refs {
            // Phase E D-E10: emit typed form. Each ref becomes a
            // one-key map: `- quote: "..."` or `- memory: mem-...`.
            // Reads accept BOTH this form AND the legacy plain-string
            // form (per the custom `Deserialize` on `EvidenceRef`).
            match r {
                EvidenceRef::Quote(s) => {
                    out.push_str("    - quote: ");
                    out.push_str(&render_scalar(s, 8));
                    out.push('\n');
                }
                EvidenceRef::Memory(id) => {
                    out.push_str("    - memory: ");
                    out.push_str(id.as_str());
                    out.push('\n');
                }
            }
        }
    }
    out.push_str("  generated_by: ");
    out.push_str(cn.generated_by.as_str());
    out.push('\n');
    out.push_str("  generated_at: ");
    out.push_str(&cn.generated_at);
    out.push('\n');
}

fn emit_ingest_provenance(out: &mut String, p: &IngestProvenance) {
    out.push_str("ingest_provenance:\n");
    out.push_str("  source_type: ");
    out.push_str(p.source_type.as_str());
    out.push('\n');
    emit_nested_string(out, "source_path", &p.source_path);
    if let Some(eid) = &p.source_external_id {
        emit_nested_string(out, "source_external_id", eid);
    }
    out.push_str("  extracted_at: ");
    out.push_str(&p.extracted_at);
    out.push('\n');
}

fn emit_nested_string(out: &mut String, key: &str, value: &str) {
    out.push_str("  ");
    out.push_str(key);
    out.push_str(": ");
    out.push_str(&render_scalar(value, 4));
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::yaml::reader::parse_lesson_frontmatter;
    use crate::engine::yaml::schema::{Confidence, GeneratedBy, IngestSourceType, LessonStatus};

    fn minimum_fm() -> LessonFrontmatter {
        LessonFrontmatter {
            id: "les-aaaaaaaa".into(),
            description: "minimal".into(),
            status: LessonStatus::Active,
            created_at: "2026-05-13T00:00:00.000Z".into(),
            causal_narrative: None,
            target_skill: None,
            source_feedback_ids: None,
            applied_count: 0,
            last_applied_at: None,
            thumbs_up_count: 0,
            thumbs_down_count: 0,
            external_signal_sources: vec![],
            applied_session_ids: vec![],
            promotion_eligible_at: None,
            superseded_by: None,
            superseded_at: None,
            ingest_provenance: None,
            authored_by: Default::default(),
            updated_at: None,
        }
    }

    #[test]
    fn round_trips_minimum_lesson() {
        let fm = minimum_fm();
        let yaml = serialize_lesson_frontmatter(&fm);
        let parsed = parse_lesson_frontmatter(&yaml).unwrap();
        assert_eq!(parsed, fm);
    }

    /// Audit A1: field order matches TS load-path. This test fails fast
    /// if a future contributor reorders fields without realigning the
    /// emit sequence.
    #[test]
    fn emits_fields_in_ts_load_path_order() {
        let mut fm = minimum_fm();
        fm.causal_narrative = Some(CausalNarrative {
            trigger: "t".into(),
            failure_mode: "f".into(),
            correction: "c".into(),
            confidence: Confidence::Inferred,
            evidence_refs: vec![],
            generated_by: GeneratedBy::Llm,
            generated_at: "2026-05-13T00:00:00.000Z".into(),
        });
        fm.target_skill = Some("skill-x".into());
        fm.source_feedback_ids = Some(vec![1]);
        fm.last_applied_at = Some("2026-05-13T01:00:00.000Z".into());
        fm.external_signal_sources = vec!["user_thumbs_up".into()];
        fm.promotion_eligible_at = Some("2026-05-14T00:00:00.000Z".into());
        fm.superseded_by = Some("les-bbbbbbbb".into());
        fm.superseded_at = Some("2026-05-14T01:00:00.000Z".into());
        fm.ingest_provenance = Some(IngestProvenance {
            source_type: IngestSourceType::AutoMemory,
            source_path: "/p".into(),
            source_external_id: None,
            extracted_at: "2026-05-13T00:00:00.000Z".into(),
        });
        fm.updated_at = Some("2026-05-14T02:00:00.000Z".into());

        let yaml = serialize_lesson_frontmatter(&fm);
        let top_level_keys: Vec<&str> = yaml
            .lines()
            .filter_map(|line| {
                if line.starts_with(' ') || line.starts_with('-') {
                    return None;
                }
                line.split_once(':').map(|(k, _)| k)
            })
            .collect();

        let expected = vec![
            "id",
            "description",
            "status",
            "created_at",
            "causal_narrative",
            "target_skill",
            "source_feedback_ids",
            "applied_count",
            "last_applied_at",
            "thumbs_up_count",
            "thumbs_down_count",
            "external_signal_sources",
            "promotion_eligible_at",
            "superseded_by",
            "superseded_at",
            "ingest_provenance",
            "authored_by", // Phase E D-E11 addition
            "updated_at",
        ];
        assert_eq!(top_level_keys, expected);
    }

    #[test]
    fn quoted_when_value_contains_colon_space() {
        let mut fm = minimum_fm();
        fm.description = "Always apply X: do not Y".into();
        let yaml = serialize_lesson_frontmatter(&fm);
        assert!(yaml.contains("description: \"Always apply X: do not Y\"\n"));
    }

    #[test]
    fn quoted_when_value_starts_with_dash() {
        let mut fm = minimum_fm();
        fm.description = "-foo".into();
        let yaml = serialize_lesson_frontmatter(&fm);
        assert!(yaml.contains("description: \"-foo\"\n"));
    }

    #[test]
    fn plain_when_value_simple() {
        let mut fm = minimum_fm();
        fm.description = "simple_description_no_specials".into();
        let yaml = serialize_lesson_frontmatter(&fm);
        assert!(yaml.contains("description: simple_description_no_specials\n"));
    }

    #[test]
    fn quoted_when_value_parses_as_number() {
        let mut fm = minimum_fm();
        fm.description = "42".into();
        let yaml = serialize_lesson_frontmatter(&fm);
        assert!(yaml.contains("description: \"42\"\n"));
    }

    /// Audit A3: YAML extra numerics (.inf / 0x10 / +42) must quote.
    #[test]
    fn quoted_for_yaml_extra_numerics() {
        for v in [".inf", ".nan", "0x10", "0o7", "+42"] {
            let mut fm = minimum_fm();
            fm.description = v.into();
            let yaml = serialize_lesson_frontmatter(&fm);
            assert!(
                yaml.contains(&format!("description: \"{v}\"\n")),
                "expected {v} to be quoted in: {yaml}"
            );
        }
    }

    #[test]
    fn quoted_when_value_is_yaml_keyword() {
        let mut fm = minimum_fm();
        fm.description = "true".into();
        let yaml = serialize_lesson_frontmatter(&fm);
        assert!(yaml.contains("description: \"true\"\n"));
    }

    /// Audit A4: yes/no/on/off are NOT YAML 1.2 keywords; emit plain.
    #[test]
    fn plain_for_yaml_1_1_obsoleted_keywords() {
        for v in ["yes", "no", "on", "off"] {
            let mut fm = minimum_fm();
            fm.description = v.into();
            let yaml = serialize_lesson_frontmatter(&fm);
            assert!(
                yaml.contains(&format!("description: {v}\n")),
                "expected {v} plain in: {yaml}"
            );
        }
    }

    /// Audit A4: embedded quote chars are spec-legal in plain.
    #[test]
    fn plain_for_embedded_quotes() {
        let mut fm = minimum_fm();
        fm.description = "she said \"hi\"".into();
        let yaml = serialize_lesson_frontmatter(&fm);
        assert!(yaml.contains("description: she said \"hi\"\n"));
    }

    /// Audit A2: multi-line strings emit literal `|-` blocks.
    #[test]
    fn multiline_string_uses_literal_block() {
        let mut fm = minimum_fm();
        fm.description = "line one\nline two".into();
        let yaml = serialize_lesson_frontmatter(&fm);
        assert!(yaml.contains("description: |-\n  line one\n  line two\n"));
    }

    #[test]
    fn multiline_with_trailing_newline_uses_pipe_block() {
        let mut fm = minimum_fm();
        fm.description = "line one\nline two\n".into();
        let yaml = serialize_lesson_frontmatter(&fm);
        assert!(yaml.contains("description: |\n  line one\n  line two\n"));
    }

    #[test]
    fn empty_array_inline() {
        let fm = minimum_fm();
        let yaml = serialize_lesson_frontmatter(&fm);
        assert!(yaml.contains("external_signal_sources: []\n"));
    }

    #[test]
    fn nonempty_array_block_style() {
        let mut fm = minimum_fm();
        fm.external_signal_sources = vec!["user_thumbs_up".into(), "sentiment_positive".into()];
        let yaml = serialize_lesson_frontmatter(&fm);
        assert!(
            yaml.contains("external_signal_sources:\n  - user_thumbs_up\n  - sentiment_positive\n")
        );
    }

    #[test]
    fn nested_causal_narrative_2_space_indent() {
        let mut fm = minimum_fm();
        fm.causal_narrative = Some(CausalNarrative {
            trigger: "unattested".into(),
            failure_mode: "unattested".into(),
            correction: "do X".into(),
            confidence: Confidence::Speculative,
            evidence_refs: vec![],
            generated_by: GeneratedBy::User,
            generated_at: "2026-05-13T00:00:00.000Z".into(),
        });
        let yaml = serialize_lesson_frontmatter(&fm);
        assert!(yaml.contains("causal_narrative:\n"));
        assert!(yaml.contains("  trigger: unattested\n"));
        assert!(yaml.contains("  confidence: speculative\n"));
        assert!(yaml.contains("  evidence_refs: []\n"));
        assert!(yaml.contains("  generated_by: user\n"));
    }

    #[test]
    fn round_trips_with_narrative() {
        let mut fm = minimum_fm();
        fm.causal_narrative = Some(CausalNarrative {
            trigger: "commit attempt without typecheck".into(),
            failure_mode: "CI red".into(),
            correction: "run typecheck before commit".into(),
            confidence: Confidence::Inferred,
            evidence_refs: vec![EvidenceRef::Quote("\"some evidence quote\"".into())],
            generated_by: GeneratedBy::Llm,
            generated_at: "2026-05-13T00:00:00.000Z".into(),
        });
        let yaml = serialize_lesson_frontmatter(&fm);
        let parsed = parse_lesson_frontmatter(&yaml).unwrap();
        assert_eq!(parsed, fm);
    }

    #[test]
    fn round_trips_with_ingest_provenance() {
        let mut fm = minimum_fm();
        fm.ingest_provenance = Some(IngestProvenance {
            source_type: IngestSourceType::AutoMemory,
            source_path: "/path/to/memory.md".into(),
            source_external_id: Some("ext-1".into()),
            extracted_at: "2026-05-13T10:00:00.000Z".into(),
        });
        let yaml = serialize_lesson_frontmatter(&fm);
        let parsed = parse_lesson_frontmatter(&yaml).unwrap();
        assert_eq!(parsed, fm);
    }

    #[test]
    fn round_trips_with_all_optional_fields_set() {
        let mut fm = minimum_fm();
        fm.last_applied_at = Some("2026-05-13T11:00:00.000Z".into());
        fm.promotion_eligible_at = Some("2026-05-14T00:00:00.000Z".into());
        fm.target_skill = Some("testing-discipline".into());
        fm.source_feedback_ids = Some(vec![1, 2, 3]);
        fm.superseded_by = Some("les-bbbbbbbb".into());
        fm.superseded_at = Some("2026-05-14T01:00:00.000Z".into());
        fm.updated_at = Some("2026-05-14T02:00:00.000Z".into());
        fm.applied_count = 5;
        fm.thumbs_up_count = 1;
        fm.external_signal_sources = vec!["user_thumbs_up".into()];

        let yaml = serialize_lesson_frontmatter(&fm);
        let parsed = parse_lesson_frontmatter(&yaml).unwrap();
        assert_eq!(parsed, fm);
    }
}
