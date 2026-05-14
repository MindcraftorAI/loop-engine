//! Phase F C-F5 — end-to-end wedge regression for skills.
//!
//! THE cross-cutting wedge for skills mirrors the lesson wedge:
//! when a USER authors a skill that cites a memory, the cited
//! memory MUST become eviction-immune. The skill itself MUST also
//! be eviction-immune from engine-initiated archive/delete.
//!
//! Sequence (the wedge claim, defended top-to-bottom):
//!   1. Insert raw memory M1.
//!   2. Insert a user-authored Skill S with `evidence_refs:
//!      [EvidenceRef::Memory(M1)]`.
//!   3. Assert M1.consumed_by_user_lessons == 1 (skill citation
//!      bumped the immunity counter, same wire-up as lessons).
//!   4. Assert `archive(S, force=false)` returns `UserSkillImmune`.
//!   5. Assert `archive(S, force=true)` succeeds.
//!   6. Assert `delete(S, force=false)` returns `UserSkillImmune`.
//!   7. Assert `memory::delete(M1, force=false)` returns
//!      `UserMemoryImmune` (counter from step 3 is load-bearing).
//!
//! NEGATIVE control: LLM-authored skill citing the same memory
//! must NOT bump the counter — the `authored_by.is_user()` check
//! is load-bearing.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use loop_daemon::engine::context::Context;
use loop_daemon::engine::embedding::MockEmbedder;
use loop_daemon::engine::error::EngineError;
use loop_daemon::engine::memory::{
    self, get_by_id as memory_get_by_id, insert as memory_insert, MemoryId,
};
use loop_daemon::engine::skills::{
    archive as archive_skill, delete as delete_skill, get_by_id as get_skill_by_id,
    insert as insert_skill, SkillFrontmatter,
};
use loop_daemon::engine::storage::{MemoryStorage, Storage};
use loop_daemon::engine::vector::HnswVectorIndex;
use loop_daemon::engine::yaml::{Authorship, EvidenceRef};

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

/// The full wedge: user-authored skill citing M1 → M1 becomes
/// immune; skill itself is also immune; both `force=true` paths
/// succeed.
#[tokio::test]
async fn user_authored_skill_citing_memory_makes_both_immune() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
    let vector_index = HnswVectorIndex::new(4);

    // 1. Insert raw memory M1.
    let m1 = MemoryId::new("mem-sklwdg001");
    let emb = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
    memory_insert(
        &ctx(),
        storage.as_ref(),
        &emb,
        &vector_index,
        m1.clone(),
        "raw memory",
        "body",
        now(),
    )
    .await
    .unwrap();
    let before = memory_get_by_id(&ctx(), storage.as_ref(), &m1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.frontmatter.consumed_by_user_lessons, 0);

    // 2. Insert user-authored skill citing M1.
    let mut fm = SkillFrontmatter::new("formatter", "auto-format on save");
    fm.authored_by = Authorship::User;
    fm.evidence_refs = vec![EvidenceRef::Memory(m1.clone())];
    insert_skill(&ctx(), storage.as_ref(), "skl-wdg00001", fm, "body").await.unwrap();

    // 3. M1.consumed_by_user_lessons MUST have incremented — the
    //    wedge wire-up.
    let after = memory_get_by_id(&ctx(), storage.as_ref(), &m1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.frontmatter.consumed_by_user_lessons, 1,
        "user-authored skill citing memory MUST bump immunity counter"
    );

    // 4. Engine-initiated archive refused.
    let r = archive_skill(&ctx(), storage.as_ref(), "skl-wdg00001", false).await;
    match r {
        Err(EngineError::UserSkillImmune { id }) => assert_eq!(id, "skl-wdg00001"),
        other => panic!("expected UserSkillImmune, got {other:?}"),
    }
    // Skill still present + still Draft (not Archived).
    let s_still = get_skill_by_id(&ctx(), storage.as_ref(), "skl-wdg00001")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        s_still.frontmatter.status,
        loop_daemon::engine::skills::SkillStatus::Draft
    );

    // 5. User-initiated archive (force=true) succeeds.
    let archived = archive_skill(&ctx(), storage.as_ref(), "skl-wdg00001", true)
        .await
        .unwrap();
    assert_eq!(
        archived.frontmatter.status,
        loop_daemon::engine::skills::SkillStatus::Archived
    );

    // 6. Engine-initiated delete refused (insert a fresh user-
    //    authored skill — first one is archived now).
    let mut fm2 = SkillFrontmatter::new("formatter-2", "another user skill");
    fm2.authored_by = Authorship::User;
    fm2.evidence_refs = vec![EvidenceRef::Memory(m1.clone())];
    insert_skill(&ctx(), storage.as_ref(), "skl-wdg00002", fm2, "body").await.unwrap();
    let r = delete_skill(&ctx(), storage.as_ref(), "skl-wdg00002", false).await;
    assert!(matches!(r, Err(EngineError::UserSkillImmune { .. })));

    // 7. M1 is now cited by TWO user-authored skills — memory
    //    immunity counter MUST refuse engine-initiated delete.
    let r = memory::delete(&ctx(), storage.as_ref(), &vector_index, &m1, false).await;
    match r {
        Err(EngineError::UserMemoryImmune { id, cited_by }) => {
            assert_eq!(id, m1.as_str());
            assert!(cited_by >= 1, "cited_by must reflect skill citation(s)");
        }
        other => panic!("expected UserMemoryImmune, got {other:?}"),
    }
}

/// Negative control: LLM-authored skill citing memory must NOT
/// drive the immunity counter. Defends against an `authored_by`
/// check accidentally flipped to "always increment."
#[tokio::test]
async fn llm_authored_skill_does_not_confer_immunity() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
    let vector_index = HnswVectorIndex::new(4);

    let m1 = MemoryId::new("mem-llmskl001");
    let emb = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
    memory_insert(
        &ctx(),
        storage.as_ref(),
        &emb,
        &vector_index,
        m1.clone(),
        "raw",
        "body",
        now(),
    )
    .await
    .unwrap();

    // LLM-authored (default) skill citing M1 via EvidenceRef::Memory.
    let mut fm = SkillFrontmatter::new("llm-skill", "auto-generated");
    fm.evidence_refs = vec![EvidenceRef::Memory(m1.clone())];
    // authored_by = Authorship::Llm (default)
    insert_skill(&ctx(), storage.as_ref(), "skl-llm00001", fm, "body").await.unwrap();

    let after = memory_get_by_id(&ctx(), storage.as_ref(), &m1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.frontmatter.consumed_by_user_lessons, 0,
        "LLM-authored skill MUST NOT drive immunity counter"
    );

    // Engine-initiated archive of the LLM-authored skill MUST succeed.
    let archived = archive_skill(&ctx(), storage.as_ref(), "skl-llm00001", false)
        .await
        .unwrap();
    assert_eq!(
        archived.frontmatter.status,
        loop_daemon::engine::skills::SkillStatus::Archived
    );

    // Engine-initiated delete of M1 MUST succeed (no user-immunity).
    memory::delete(&ctx(), storage.as_ref(), &vector_index, &m1, false)
        .await
        .unwrap();
    assert!(
        memory_get_by_id(&ctx(), storage.as_ref(), &m1)
            .await
            .unwrap()
            .is_none()
    );
}

/// User-authored skill with NO memory citations — skill itself is
/// still immune (authorship-based), but no memory counter is
/// touched. Exercises the authorship-only immunity path.
#[tokio::test]
async fn user_authored_skill_without_citations_still_immune() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());

    let mut fm = SkillFrontmatter::new("user-skill", "no citations");
    fm.authored_by = Authorship::User;
    insert_skill(&ctx(), storage.as_ref(), "skl-noref0001", fm, "body").await.unwrap();

    let r = archive_skill(&ctx(), storage.as_ref(), "skl-noref0001", false).await;
    assert!(matches!(r, Err(EngineError::UserSkillImmune { .. })));
    let r = delete_skill(&ctx(), storage.as_ref(), "skl-noref0001", false).await;
    assert!(matches!(r, Err(EngineError::UserSkillImmune { .. })));
}
