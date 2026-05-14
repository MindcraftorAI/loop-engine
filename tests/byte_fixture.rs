//! Byte-fixture test: assert the Rust writer produces byte-identical
//! YAML to a hand-crafted reference matching what the TS-side `yaml`
//! library emits under the pinned options. Audit C1.
//!
//! If audit findings A1-A6 ever regress, this test fails immediately.

use loop_engine::yaml::writer::serialize_lesson_frontmatter;
use loop_engine::yaml::{
    CausalNarrative, IngestProvenance, IngestSourceType, LessonFrontmatter, LessonStatus,
};

#[test]
fn matches_ts_load_path_byte_output() {
    let fm = LessonFrontmatter {
        id: "les-aaaaaaaa".into(),
        description: "Always run typecheck before committing".into(),
        status: LessonStatus::Active,
        created_at: "2026-05-13T00:00:00.000Z".into(),
        causal_narrative: Some(CausalNarrative {
            trigger: "commit attempt without typecheck".into(),
            failure_mode: "CI red on next push".into(),
            correction: "run npm run typecheck before commit".into(),
            confidence: loop_engine::yaml::reader::__expose_confidence_inferred(),
            evidence_refs: vec![loop_engine::engine::yaml::EvidenceRef::Quote(
                "\"CI broke again\"".into(),
            )],
            generated_by: loop_engine::yaml::reader::__expose_generated_by_llm(),
            generated_at: "2026-05-13T00:00:00.000Z".into(),
        }),
        target_skill: Some("testing-discipline".into()),
        source_feedback_ids: Some(vec![1, 2]),
        applied_count: 3,
        last_applied_at: Some("2026-05-13T01:00:00.000Z".into()),
        thumbs_up_count: 1,
        thumbs_down_count: 0,
        external_signal_sources: vec!["user_thumbs_up".into(), "sentiment_positive".into()],
        applied_session_ids: vec![],
        promotion_eligible_at: Some("2026-05-14T00:00:00.000Z".into()),
        superseded_by: None,
        superseded_at: None,
        ingest_provenance: Some(IngestProvenance {
            source_type: IngestSourceType::AutoMemory,
            source_path: "/Users/x/.claude/projects/-Users-x-loop/memory/feedback_typecheck.md"
                .into(),
            source_external_id: Some("feedback-typecheck".into()),
            extracted_at: "2026-05-13T00:00:00.000Z".into(),
        }),
        authored_by: Default::default(),
        updated_at: Some("2026-05-13T01:30:00.000Z".into()),
    };

    // Expected output mirrors what TS-side `yaml.stringify` produces
    // under {blockQuote: 'literal', lineWidth: 0, defaultStringType: 'PLAIN',
    // defaultKeyType: 'PLAIN'} given the same input. Field order = TS
    // load-path order from core/src/lessons/loader.ts::tryLoadLessonFile.
    let expected = "\
id: les-aaaaaaaa
description: Always run typecheck before committing
status: active
created_at: 2026-05-13T00:00:00.000Z
causal_narrative:
  trigger: commit attempt without typecheck
  failure_mode: CI red on next push
  correction: run npm run typecheck before commit
  confidence: inferred
  evidence_refs:
    - quote: \"\\\"CI broke again\\\"\"
  generated_by: llm
  generated_at: 2026-05-13T00:00:00.000Z
target_skill: testing-discipline
source_feedback_ids:
  - 1
  - 2
applied_count: 3
last_applied_at: 2026-05-13T01:00:00.000Z
thumbs_up_count: 1
thumbs_down_count: 0
external_signal_sources:
  - user_thumbs_up
  - sentiment_positive
promotion_eligible_at: 2026-05-14T00:00:00.000Z
ingest_provenance:
  source_type: auto_memory
  source_path: /Users/x/.claude/projects/-Users-x-loop/memory/feedback_typecheck.md
  source_external_id: feedback-typecheck
  extracted_at: 2026-05-13T00:00:00.000Z
authored_by: llm
updated_at: 2026-05-13T01:30:00.000Z
";

    let actual = serialize_lesson_frontmatter(&fm);
    assert_eq!(actual, expected, "byte output drift from TS reference");
}
