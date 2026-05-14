//! Phase E2 Cx3 — end-to-end wedge regression for compression.
//!
//! THE cross-cutting wedge invariant for the compression layer:
//!   1. Insert a raw memory M1.
//!   2. Simulate a user-authored lesson citing M1
//!      (`increment_citation_count`).
//!   3. Compress M1 into Mc. Mc.consumed_by_user_lessons = 1
//!      (transferred from M1).
//!   4. Force-delete M1 (host's post-compression sweep).
//!   5. The chase-helper for M1 STILL resolves — it walks forward
//!      to Mc. The citation chain is unbroken.
//!   6. `recompute_citation_counts` (with the actual user-lesson
//!      present) credits Mc, not M1, after the chase.
//!
//! Plus recursive compression: M1 → Mc → Mcc. Citation chains
//! through multiple compression generations resolve correctly.

use std::sync::Arc;

use bytes::Bytes;
use chrono::{DateTime, Utc};

use loop_engine::engine::context::Context;
use loop_engine::engine::embedding::MockEmbedder;
use loop_engine::engine::llm::{Generation, MockLlmClient};
use loop_engine::engine::memory::{
    compress, delete, get_by_id, get_by_id_chasing_derived_from, increment_citation_count, insert,
    recompute_citation_counts, CompressionConfig, CompressionWindow, MemoryId,
};
use loop_engine::engine::storage::{MemoryStorage, Storage, StorageKey};
use loop_engine::engine::vector::HnswVectorIndex;
use loop_engine::engine::yaml::{
    combine_frontmatter, writer::serialize_lesson_frontmatter, Authorship, CausalNarrative,
    Confidence, EvidenceRef, GeneratedBy, LessonFrontmatter, LessonStatus,
};

fn ctx() -> Context {
    Context::single_user_local()
}

fn now() -> DateTime<Utc> {
    "2026-05-14T12:00:00Z".parse().unwrap()
}

fn unit_vec(dim: usize, axis: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; dim];
    v[axis % dim] = 1.0;
    v
}

fn success_generation(json_str: &str) -> Generation {
    Generation::new(json_str).with_parsed(serde_json::from_str(json_str).unwrap())
}

/// Write a user-authored lesson citing `mid` via `EvidenceRef::Memory`.
async fn write_user_lesson_citing_memory(storage: &dyn Storage, lesson_id: &str, mid: MemoryId) {
    let fm = LessonFrontmatter {
        id: lesson_id.into(),
        description: "user-authored test lesson".into(),
        status: LessonStatus::Active,
        created_at: "2026-05-14T00:00:00Z".into(),
        causal_narrative: Some(CausalNarrative {
            trigger: "t".into(),
            failure_mode: "f".into(),
            correction: "c".into(),
            confidence: Confidence::Inferred,
            evidence_refs: vec![EvidenceRef::Memory(mid)],
            generated_by: GeneratedBy::User,
            generated_at: "2026-05-14T00:00:00Z".into(),
        }),
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
        authored_by: Authorship::User, // load-bearing
        updated_at: None,
    };
    let yaml = serialize_lesson_frontmatter(&fm);
    let content = combine_frontmatter(&yaml, "lesson body\n");
    let key = StorageKey::lesson(&ctx(), "active", lesson_id);
    storage.put(&key, Bytes::from(content)).await.unwrap();
}

#[tokio::test]
async fn wedge_citation_chain_survives_compression_and_predecessor_force_delete() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
    let vector_index = HnswVectorIndex::new(4);

    // 1. Insert raw memory M1.
    let emb1 = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
    let m1 = MemoryId::new("mem-wdg00001");
    insert(
        &ctx(),
        storage.as_ref(),
        &emb1,
        &vector_index,
        m1.clone(),
        "raw memory description",
        "raw body content",
        now(),
    )
    .await
    .unwrap();

    // 2. Simulate user-authored lesson citing M1.
    increment_citation_count(&ctx(), storage.as_ref(), &m1)
        .await
        .unwrap();
    write_user_lesson_citing_memory(storage.as_ref(), "les-wedge001", m1.clone()).await;

    // 3. Compress M1 into Mc. Mc inherits the citation count.
    let llm = MockLlmClient::default().with_response(success_generation(
        r#"{"description":"summary","content":"compressed body"}"#,
    ));
    let emb_c = MockEmbedder::new(4).with_response(vec![unit_vec(4, 1)]);
    let mc = compress(
        &ctx(),
        storage.as_ref(),
        &llm,
        &emb_c,
        &vector_index,
        CompressionWindow::Ids(vec![m1.clone()]),
        &CompressionConfig::default(),
        now(),
    )
    .await
    .unwrap();
    assert_eq!(
        mc.frontmatter.consumed_by_user_lessons, 1,
        "Mc must inherit M1's citation count"
    );

    // 4. Force-delete M1 (host's post-compression sweep).
    delete(&ctx(), storage.as_ref(), &vector_index, &m1, true)
        .await
        .unwrap();
    assert!(
        get_by_id(&ctx(), storage.as_ref(), &m1)
            .await
            .unwrap()
            .is_none(),
        "M1 should be gone"
    );

    // 5. THE WEDGE: chase-helper for M1 still resolves — lands on Mc.
    let chased = get_by_id_chasing_derived_from(&ctx(), storage.as_ref(), &m1)
        .await
        .unwrap();
    assert!(chased.is_some(), "chain MUST resolve through compression");
    assert_eq!(
        chased.unwrap().frontmatter.id,
        mc.frontmatter.id,
        "chase must land on Mc"
    );

    // 6. recompute_citation_counts walks the chain forward — the
    //    user-authored lesson's EvidenceRef::Memory(M1) resolves to
    //    Mc, so Mc keeps its citation count of 1. M1 isn't a record
    //    anymore so it doesn't get counted (already deleted; the
    //    chase-helper supplies Mc as the canonical id).
    let stats = recompute_citation_counts(&ctx(), storage.as_ref())
        .await
        .unwrap();
    assert_eq!(stats.lessons_scanned, 1);
    let mc_after = get_by_id(&ctx(), storage.as_ref(), &mc.frontmatter.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        mc_after.frontmatter.consumed_by_user_lessons, 1,
        "Mc must retain its 1 citation after recompute"
    );
}

#[tokio::test]
async fn recursive_compression_preserves_citation_chain() {
    // M1 + M2 → Mc (level 1 compression)
    // Mc + M3 → Mcc (level 2 compression — recursive)
    // Total citations: M1 had 1, M2 had 2, M3 had 4. Mc inherits 3,
    // Mcc inherits 3 (from Mc) + 4 (from M3) = 7.
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
    let vector_index = HnswVectorIndex::new(4);

    for (id_str, citations) in [
        ("mem-rec00001", 1),
        ("mem-rec00002", 2),
        ("mem-rec00003", 4),
    ] {
        let emb = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new(id_str);
        insert(
            &ctx(),
            storage.as_ref(),
            &emb,
            &vector_index,
            id.clone(),
            id_str,
            "body",
            now(),
        )
        .await
        .unwrap();
        for _ in 0..citations {
            increment_citation_count(&ctx(), storage.as_ref(), &id)
                .await
                .unwrap();
        }
    }

    // Level 1: M1 + M2 → Mc.
    let llm1 = MockLlmClient::default().with_response(success_generation(
        r#"{"description":"level1","content":"c1"}"#,
    ));
    let emb_c1 = MockEmbedder::new(4).with_response(vec![unit_vec(4, 1)]);
    let mc = compress(
        &ctx(),
        storage.as_ref(),
        &llm1,
        &emb_c1,
        &vector_index,
        CompressionWindow::Ids(vec![
            MemoryId::new("mem-rec00001"),
            MemoryId::new("mem-rec00002"),
        ]),
        &CompressionConfig::default(),
        now(),
    )
    .await
    .unwrap();
    assert_eq!(mc.frontmatter.consumed_by_user_lessons, 3, "Mc: 1+2=3");

    // Level 2: Mc + M3 → Mcc.
    let llm2 = MockLlmClient::default().with_response(success_generation(
        r#"{"description":"level2","content":"c2"}"#,
    ));
    let emb_c2 = MockEmbedder::new(4).with_response(vec![unit_vec(4, 2)]);
    let mcc = compress(
        &ctx(),
        storage.as_ref(),
        &llm2,
        &emb_c2,
        &vector_index,
        CompressionWindow::Ids(vec![
            mc.frontmatter.id.clone(),
            MemoryId::new("mem-rec00003"),
        ]),
        &CompressionConfig::default(),
        now(),
    )
    .await
    .unwrap();
    assert_eq!(mcc.frontmatter.consumed_by_user_lessons, 7, "Mcc: 3+4=7");
    assert_eq!(mcc.frontmatter.derived_from.len(), 2);
    assert!(mcc.is_compressed());

    // Chase from M1 (still alive) — should land on Mcc as the
    // most-recent successor in the chain (M1 → Mc → Mcc).
    let chased =
        get_by_id_chasing_derived_from(&ctx(), storage.as_ref(), &MemoryId::new("mem-rec00001"))
            .await
            .unwrap();
    assert!(
        chased.is_some(),
        "chase must traverse two compression levels"
    );
    assert_eq!(
        chased.unwrap().frontmatter.id,
        mcc.frontmatter.id,
        "chase ends at the leaf compressor"
    );
}

#[tokio::test]
async fn compress_with_predecessor_window_yields_correct_derived_from() {
    // Sanity check that compress + chase compose: insert 3 raw
    // memories matching a predicate, compress via Predicate window,
    // assert Mc.derived_from contains all 3.
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
    let vector_index = HnswVectorIndex::new(4);
    for id_str in ["mem-pdw00001", "mem-pdw00002", "mem-pdw00003"] {
        let emb = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new(id_str);
        insert(
            &ctx(),
            storage.as_ref(),
            &emb,
            &vector_index,
            id,
            "matchable",
            "body",
            now(),
        )
        .await
        .unwrap();
    }
    let predicate: loop_engine::engine::memory::PrunePredicate =
        Box::new(|fm| fm.description == "matchable");
    let llm = MockLlmClient::default().with_response(success_generation(
        r#"{"description":"summary","content":"compressed"}"#,
    ));
    let emb_c = MockEmbedder::new(4).with_response(vec![unit_vec(4, 1)]);
    let mc = compress(
        &ctx(),
        storage.as_ref(),
        &llm,
        &emb_c,
        &vector_index,
        CompressionWindow::Predicate(predicate),
        &CompressionConfig::default(),
        now(),
    )
    .await
    .unwrap();
    assert_eq!(mc.frontmatter.derived_from.len(), 3);
}

/// Phase E2 audit M1 regression: duplicate ids in
/// `CompressionWindow::Ids` must be deduped — `derived_from` should
/// reflect unique predecessors, and the citation transfer must not
/// double-count.
#[tokio::test]
async fn compress_dedupes_duplicate_predecessor_ids() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
    let vector_index = HnswVectorIndex::new(4);
    let m1 = MemoryId::new("mem-dedup0001");
    let emb = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
    insert(
        &ctx(),
        storage.as_ref(),
        &emb,
        &vector_index,
        m1.clone(),
        "x",
        "y",
        now(),
    )
    .await
    .unwrap();
    increment_citation_count(&ctx(), storage.as_ref(), &m1)
        .await
        .unwrap();
    // Window with M1 listed 3 times.
    let llm = MockLlmClient::default()
        .with_response(success_generation(r#"{"description":"s","content":"c"}"#));
    let emb_c = MockEmbedder::new(4).with_response(vec![unit_vec(4, 1)]);
    let mc = compress(
        &ctx(),
        storage.as_ref(),
        &llm,
        &emb_c,
        &vector_index,
        CompressionWindow::Ids(vec![m1.clone(), m1.clone(), m1.clone()]),
        &CompressionConfig::default(),
        now(),
    )
    .await
    .unwrap();
    assert_eq!(
        mc.frontmatter.derived_from.len(),
        1,
        "duplicates must be deduped"
    );
    assert_eq!(
        mc.frontmatter.consumed_by_user_lessons, 1,
        "citation transfer must not double-count"
    );
}

/// Phase E2 audit M3 fix: the wedge regression must actually
/// EXERCISE the chase repair path. We force Mc's counter to 0
/// (simulating drift / partial-write corruption), then run
/// `recompute_citation_counts` and assert it RESTORES the count.
/// The original C-x3 test passed equally if recompute were a no-op.
#[tokio::test]
async fn wedge_recompute_actually_restores_drift_through_compression_chain() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
    let vector_index = HnswVectorIndex::new(4);
    let emb = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
    let m1 = MemoryId::new("mem-drift1234");
    insert(
        &ctx(),
        storage.as_ref(),
        &emb,
        &vector_index,
        m1.clone(),
        "x",
        "y",
        now(),
    )
    .await
    .unwrap();
    increment_citation_count(&ctx(), storage.as_ref(), &m1)
        .await
        .unwrap();
    write_user_lesson_citing_memory(storage.as_ref(), "les-drift0001", m1.clone()).await;
    let llm = MockLlmClient::default()
        .with_response(success_generation(r#"{"description":"s","content":"c"}"#));
    let emb_c = MockEmbedder::new(4).with_response(vec![unit_vec(4, 1)]);
    let mc = compress(
        &ctx(),
        storage.as_ref(),
        &llm,
        &emb_c,
        &vector_index,
        CompressionWindow::Ids(vec![m1.clone()]),
        &CompressionConfig::default(),
        now(),
    )
    .await
    .unwrap();
    delete(&ctx(), storage.as_ref(), &vector_index, &m1, true)
        .await
        .unwrap();
    assert_eq!(mc.frontmatter.consumed_by_user_lessons, 1);

    // Inject drift: force Mc.counter to 0 by direct write.
    // Construct a minimal corrupted YAML matching the frontmatter
    // structure with `consumed_by_user_lessons: 0`.
    let mc_key = StorageKey::memory(&ctx(), mc.frontmatter.id.as_str());
    let drifted = format!(
        "---\n\
         id: {}\n\
         description: {}\n\
         created_at: \"{}\"\n\
         consumed_by_user_lessons: 0\n\
         derived_from:\n  - {}\n\
         ---\n\
         compressed body\n",
        mc.frontmatter.id.as_str(),
        mc.frontmatter.description,
        mc.frontmatter.created_at,
        m1.as_str(),
    );
    storage.put(&mc_key, Bytes::from(drifted)).await.unwrap();

    // Verify drift.
    let pre = get_by_id(&ctx(), storage.as_ref(), &mc.frontmatter.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pre.frontmatter.consumed_by_user_lessons, 0);

    // Recompute: chase walks M1 → Mc, lesson citation credits Mc,
    // counter rewritten back to 1.
    let stats = recompute_citation_counts(&ctx(), storage.as_ref())
        .await
        .unwrap();
    assert_eq!(
        stats.counters_repaired, 1,
        "recompute must REPAIR the drift"
    );
    assert_eq!(stats.orphan_citations, 0, "no orphan: chase resolved Mc");
    let post = get_by_id(&ctx(), storage.as_ref(), &mc.frontmatter.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        post.frontmatter.consumed_by_user_lessons, 1,
        "recompute must RESTORE the citation through the chain"
    );
}

/// Phase E2 audit M4 fix: NEGATIVE control. An LLM-authored lesson
/// must NOT confer immunity through compression. The user-immunity
/// invariant is specifically about USER-authored lessons. If a
/// flipped `authored_by.is_user()` check landed (treating LLM as
/// user), this test would catch it.
#[tokio::test]
async fn llm_authored_lesson_does_not_confer_immunity_through_compression() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
    let vector_index = HnswVectorIndex::new(4);
    let emb = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
    let m1 = MemoryId::new("mem-llmlsn001");
    insert(
        &ctx(),
        storage.as_ref(),
        &emb,
        &vector_index,
        m1.clone(),
        "x",
        "y",
        now(),
    )
    .await
    .unwrap();

    // Write an LLM-authored lesson citing M1 (the only difference
    // from the user-authored wedge test is `authored_by:
    // Authorship::Llm`).
    let fm = LessonFrontmatter {
        id: "les-llm00001".into(),
        description: "llm-authored lesson".into(),
        status: LessonStatus::Active,
        created_at: "2026-05-14T00:00:00Z".into(),
        causal_narrative: Some(CausalNarrative {
            trigger: "t".into(),
            failure_mode: "f".into(),
            correction: "c".into(),
            confidence: Confidence::Inferred,
            evidence_refs: vec![EvidenceRef::Memory(m1.clone())],
            generated_by: GeneratedBy::Llm,
            generated_at: "2026-05-14T00:00:00Z".into(),
        }),
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
        authored_by: Authorship::Llm, // NOT User — this is the load-bearing distinction
        updated_at: None,
    };
    let yaml = serialize_lesson_frontmatter(&fm);
    let content = combine_frontmatter(&yaml, "body\n");
    let key = StorageKey::lesson(&ctx(), "active", "les-llm00001");
    storage
        .put(&key, bytes::Bytes::from(content))
        .await
        .unwrap();

    // No `increment_citation_count` — only user-authored lessons
    // should drive immunity, and the recompute walk is what drives
    // that counter from on-disk lesson data.

    // Recompute. Should NOT credit M1 (lesson is Llm-authored).
    let stats = recompute_citation_counts(&ctx(), storage.as_ref())
        .await
        .unwrap();
    assert_eq!(stats.lessons_scanned, 1);
    let m1_after = get_by_id(&ctx(), storage.as_ref(), &m1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        m1_after.frontmatter.consumed_by_user_lessons, 0,
        "LLM-authored citations MUST NOT drive the immunity counter"
    );
}

/// Phase E2 audit C2: orphan citation surfaces in RecomputeStats.
/// User-authored lesson cites a memory; memory is force-deleted;
/// no compressor for it. The chase returns None → recompute should
/// increment `orphan_citations` (audit signal, not error).
#[tokio::test]
async fn orphan_citation_surfaces_in_recompute_stats() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
    let vector_index = HnswVectorIndex::new(4);
    let emb = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
    let m1 = MemoryId::new("mem-orph00001");
    insert(
        &ctx(),
        storage.as_ref(),
        &emb,
        &vector_index,
        m1.clone(),
        "x",
        "y",
        now(),
    )
    .await
    .unwrap();
    increment_citation_count(&ctx(), storage.as_ref(), &m1)
        .await
        .unwrap();
    write_user_lesson_citing_memory(storage.as_ref(), "les-orph00001", m1.clone()).await;
    // Force-delete M1 WITHOUT compressing it first. Citation chain
    // is now orphaned — the lesson still cites M1, but neither M1
    // nor any successor exists.
    delete(&ctx(), storage.as_ref(), &vector_index, &m1, true)
        .await
        .unwrap();

    let stats = recompute_citation_counts(&ctx(), storage.as_ref())
        .await
        .unwrap();
    assert_eq!(
        stats.orphan_citations, 1,
        "audit C2 — orphan must be counted"
    );
}
