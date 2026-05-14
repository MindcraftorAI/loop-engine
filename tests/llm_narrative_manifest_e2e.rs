//! End-to-end integration: LlmClient → narrative → persist → manifest.
//!
//! Phase D C-D3: smoke test that the full "candidate signal →
//! narrative generation → lesson with narrative → manifest with
//! gate" pipeline works across the engine's trait surface. Uses
//! `MockLlmClient` (no network) + `MemoryStorage` (no filesystem)
//! for hermetic execution.
//!
//! NOT a regression matrix — those live in module-internal tests
//! (`engine::lessons::narrative::tests`, `engine::manifest::tests`,
//! `engine::lessons::gate::tests`). This file proves the integration
//! points compose without surprises.

use std::sync::Arc;

use bytes::Bytes;
use chrono::{DateTime, Utc};

use loop_engine::engine::context::Context;
use loop_engine::engine::lessons::{
    get_by_id, narrative::generate as generate_narrative, NarrativeConfig, NarrativeContext,
};
use loop_engine::engine::llm::{Generation, LlmClient, MockLlmClient};
use loop_engine::engine::manifest::{assemble, AssembleConfig};
use loop_engine::engine::storage::{MemoryStorage, Storage, StorageKey};

fn now() -> DateTime<Utc> {
    "2026-05-13T12:00:00Z".parse().unwrap()
}

fn success_generation(json_str: &str) -> Generation {
    Generation::new(json_str).with_parsed(serde_json::from_str(json_str).unwrap())
}

/// Seed a lesson with frontmatter that PASSES every Phase B gate rule
/// EXCEPT the causal_narrative requirement — so once we generate +
/// persist a narrative the manifest's gate annotation should flip
/// from Block(MissingCausalNarrative + ...) to Promote.
async fn seed_narrative_pending_lesson(storage: &dyn Storage, ctx: &Context, id: &str) {
    // created_at well before now() so TimeFloor passes. applied_count
    // above default min (3). thumbs_up source present. NO narrative —
    // this is the field we'll populate via the narrative pipeline.
    let backdated = "2026-05-11T00:00:00Z";
    let yaml = format!(
        "---\n\
         id: {id}\n\
         description: \"Always run formatter before committing\"\n\
         status: active\n\
         created_at: \"{backdated}\"\n\
         applied_count: 5\n\
         thumbs_up_count: 2\n\
         thumbs_down_count: 0\n\
         external_signal_sources:\n  - thumbs_up\n\
         ---\n\
         lesson body\n"
    );
    let key = StorageKey::lesson(ctx, "active", id);
    storage.put(&key, Bytes::from(yaml)).await.unwrap();
}

#[tokio::test]
async fn narrative_generation_produces_struct_consumed_by_manifest_gate() {
    let ctx = Context::single_user_local();
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
    let id = "les-e2etest1";

    // 1. Seed a lesson missing only causal_narrative.
    seed_narrative_pending_lesson(storage.as_ref(), &ctx, id).await;

    // 2. Confirm the gate currently BLOCKS on missing narrative.
    let mut config = AssembleConfig::default();
    config.record_applied = false;
    let m_before = assemble(&ctx, storage.as_ref(), None, None, None, &config, now())
        .await
        .unwrap();
    assert_eq!(m_before.active_lessons.len(), 1);
    let gate_before = m_before.active_lessons[0].gate.as_ref().expect("gate");
    use loop_engine::engine::lessons::{BlockReason, GateDecision};
    match gate_before {
        GateDecision::Block { reasons } => {
            assert!(reasons
                .iter()
                .any(|r| matches!(r, BlockReason::MissingCausalNarrative)));
        }
        other => panic!("expected pre-narrative block, got {other:?}"),
    }

    // 3. Generate a narrative via MockLlmClient.
    let json = r#"{
        "trigger": "user kept committing unformatted code",
        "failure_mode": "CI lint rejected three PRs in a row",
        "correction": "cargo fmt before git commit, no exceptions",
        "confidence": "inferred",
        "evidence_refs": ["\"you forgot to format again\""]
    }"#;
    let mock_llm: Arc<dyn LlmClient> =
        Arc::new(MockLlmClient::default().with_response(success_generation(json)));
    let narrative_ctx = NarrativeContext::new("Always run formatter before committing")
        .with_source_feedback("you forgot to format again");
    let narrative = generate_narrative(
        &ctx,
        mock_llm.as_ref(),
        &narrative_ctx,
        &NarrativeConfig::default(),
        now(),
    )
    .await
    .unwrap();
    assert_eq!(narrative.trigger, "user kept committing unformatted code");

    // 4. Persist the narrative by rewriting the lesson with it.
    use loop_engine::engine::yaml::{combine_frontmatter, writer::serialize_lesson_frontmatter};
    let loaded = get_by_id(&ctx, storage.as_ref(), id)
        .await
        .unwrap()
        .unwrap();
    let mut fm = loaded.frontmatter;
    fm.causal_narrative = Some(narrative);
    let new_yaml = serialize_lesson_frontmatter(&fm);
    let new_content = combine_frontmatter(&new_yaml, &loaded.body);
    let key = StorageKey::lesson(&ctx, "active", id);
    storage.put(&key, Bytes::from(new_content)).await.unwrap();

    // 5. Re-assemble — gate must transition from
    //    `Block(MissingCausalNarrative + ...)` to a state where the
    //    narrative-related reasons are gone. The seeded fixture
    //    deliberately has `created_at: 2026-05-11` (2 days before
    //    `now()`) while MemoryStorage stamps birthtime at wall-clock
    //    write time — so `TamperedAge` will fire and gate stays
    //    `Block`. The audit (A-M1) requires we assert this EXACT
    //    shape, not just "Promote or Block".
    let mut config_after = AssembleConfig::default();
    config_after.record_applied = false;
    let m_after = assemble(
        &ctx,
        storage.as_ref(),
        None,
        None,
        None,
        &config_after,
        now(),
    )
    .await
    .unwrap();
    let gate_after = m_after.active_lessons[0]
        .gate
        .as_ref()
        .expect("gate annotation present");
    match gate_after {
        GateDecision::Block { reasons } => {
            // Narrative-related blocks MUST be gone (the pipeline
            // delivered + persisted a valid narrative).
            assert!(
                !reasons
                    .iter()
                    .any(|r| matches!(r, BlockReason::MissingCausalNarrative)),
                "MissingCausalNarrative should be GONE after persisting a narrative; \
                 reasons={reasons:?}"
            );
            assert!(
                !reasons
                    .iter()
                    .any(|r| matches!(r, BlockReason::SpeculativeNarrative)),
                "SpeculativeNarrative should not fire on Inferred narrative; \
                 reasons={reasons:?}"
            );
            // The remaining block IS TamperedAge — birthtime mismatch
            // vs the seeded backdated created_at. Asserting this proves
            // the wedge end-to-end through the manifest layer.
            assert!(
                reasons
                    .iter()
                    .any(|r| matches!(r, BlockReason::TamperedAge { .. })),
                "expected TamperedAge to remain (birthtime > frontmatter created_at); \
                 reasons={reasons:?}"
            );
        }
        other => {
            panic!("expected Block (TamperedAge remains after narrative persist), got {other:?}")
        }
    }
}

#[tokio::test]
async fn narrative_validation_failure_does_not_persist() {
    // Defense-in-depth check: when narrative::generate's parse-time
    // validator rejects an LLM response (the wedge invariant), the
    // caller gets `EngineError::Llm(_)` and the lesson on storage is
    // unchanged. Re-assembling the manifest after the rejected
    // generation must STILL surface `MissingCausalNarrative` —
    // audit A-M3 fix: the original test only checked
    // `narrative.is_none()` on storage, which is tautological because
    // `narrative::generate` has no storage arg. The re-assemble is
    // the actual end-to-end claim.
    use loop_engine::engine::lessons::{BlockReason, GateDecision};
    use loop_engine::engine::manifest::assemble;
    use loop_engine::engine::manifest::AssembleConfig as Cfg;

    let ctx = Context::single_user_local();
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
    let id = "les-e2etest2";
    seed_narrative_pending_lesson(storage.as_ref(), &ctx, id).await;

    // LLM returns observed + empty evidence_refs — invalid per D-D10.
    let bad_json = r#"{
        "trigger": "t",
        "failure_mode": "f",
        "correction": "c",
        "confidence": "observed",
        "evidence_refs": []
    }"#;
    let mock_llm = MockLlmClient::default().with_response(success_generation(bad_json));
    let narrative_ctx = NarrativeContext::new("x");
    let result = generate_narrative(
        &ctx,
        &mock_llm,
        &narrative_ctx,
        &NarrativeConfig::default(),
        now(),
    )
    .await;
    assert!(result.is_err(), "validation should have failed");

    // Storage-level invariant: lesson body unchanged.
    let loaded = get_by_id(&ctx, storage.as_ref(), id)
        .await
        .unwrap()
        .unwrap();
    assert!(loaded.frontmatter.causal_narrative.is_none());

    // End-to-end invariant: manifest assembly still surfaces
    // MissingCausalNarrative — the wedge stays blocked because the
    // rejected narrative was never persisted.
    let mut config = Cfg::default();
    config.record_applied = false;
    let m = assemble(&ctx, storage.as_ref(), None, None, None, &config, now())
        .await
        .unwrap();
    let gate = m.active_lessons[0].gate.as_ref().expect("gate");
    match gate {
        GateDecision::Block { reasons } => {
            assert!(
                reasons
                    .iter()
                    .any(|r| matches!(r, BlockReason::MissingCausalNarrative)),
                "manifest should still surface MissingCausalNarrative; reasons={reasons:?}"
            );
        }
        other => panic!("expected Block (no narrative persisted), got {other:?}"),
    }
}

#[tokio::test]
async fn narrative_refusal_distinguished_from_validation_failure() {
    use loop_engine::engine::error::EngineError;
    let ctx = Context::single_user_local();
    let refusal_json = r#"{"error": "insufficient_context"}"#;
    let mock_llm = MockLlmClient::default().with_response(success_generation(refusal_json));
    let result = generate_narrative(
        &ctx,
        &mock_llm,
        &NarrativeContext::new("too generic to ground anything"),
        &NarrativeConfig::default(),
        now(),
    )
    .await;
    assert!(matches!(
        result,
        Err(EngineError::NarrativeInsufficientContext)
    ));
}
