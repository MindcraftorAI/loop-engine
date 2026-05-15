//! Phase G — lesson lifecycle transitions.
//!
//! Status state machine: `pending → active → promoted`, plus
//! terminal `discarded` and `superseded`. Status-as-directory per
//! ADR-0010: the directory is truth, the frontmatter `status` field
//! is portability metadata.
//!
//! All transitions move the lesson file from one status dir to
//! another. Storage has no `rename` primitive, so the move is
//! decomposed into:
//!   1. `put_if_version(new_key, bytes, None)` — create-only.
//!   2. `delete(old_key)` — best-effort cleanup.
//!
//! Half-applied transition (crash between 1 and 2) leaves the
//! lesson in both dirs. `get_by_id` scans the canonical order
//! (`pending, active, promoted, discarded, superseded`) and returns
//! the first hit — meaning a half-applied transition reads as
//! "still in the OLD state" until the delete completes. This is
//! the SAFER bias.
//!
//! User-authored lessons are eviction-immune from engine-initiated
//! `discard` / `supersede` (D-G8). `force=true` bypasses; only
//! user-driven paths supply that.

use std::fmt;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use tracing::warn;

use crate::engine::context::Context;
use crate::engine::error::EngineError;
use crate::engine::lessons::gate::{check_promotion_gate, GateDecision, PromotionConfig};
use crate::engine::lessons::loader::{get_by_id, LoadedLesson};
use crate::engine::memory::decrement_citation_count;
use crate::engine::storage::{Storage, StorageKey};
use crate::engine::yaml::reader::parse_lesson_frontmatter;
use crate::engine::yaml::writer::serialize_lesson_frontmatter;
use crate::engine::yaml::{
    combine_frontmatter, split_frontmatter_normalized, EvidenceRef, LessonFrontmatter, LessonStatus,
};

/// CAS-RMW retry budget for transitions. Matches Phase A/F precedent.
const TRANSITION_CAS_MAX_RETRIES: u32 = 5;

/// Cycle-detection depth cap when walking `superseded_by` forward.
/// Matches the Phase E2 compression cycle cap.
const SUPERSEDE_CYCLE_DEPTH_CAP: usize = 16;

/// Phase G D-G4: feedback polarity for `capture_feedback`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FeedbackPolarity {
    ThumbsUp,
    ThumbsDown,
}

impl FeedbackPolarity {
    fn signal_source(self) -> &'static str {
        match self {
            Self::ThumbsUp => "user_thumbs_up",
            Self::ThumbsDown => "user_thumbs_down",
        }
    }
}

/// Phase G D-G7: structured rejection reason for `supersede_lesson`.
/// Mirrors `gate::BlockReason` precedent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SupersedeBlockReason {
    /// `supersede_lesson(id, id)` — no-op rejected.
    SelfReference { id: String },
    /// `new_id` doesn't resolve to an on-disk lesson.
    ReplacementNotFound { replacement_id: String },
    /// Walking `superseded_by` forward from `new_id` lands back on
    /// `old_id` (or hits the depth cap).
    CycleDetected { chain: Vec<String> },
}

impl fmt::Display for SupersedeBlockReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SelfReference { id } => write!(f, "self-reference (id={id})"),
            Self::ReplacementNotFound { replacement_id } => {
                write!(f, "replacement-not-found ({replacement_id})")
            }
            Self::CycleDetected { chain } => write!(f, "cycle-detected (chain={chain:?})"),
        }
    }
}

fn now_iso(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// D-G5: idempotent move helper. `put_if_version(new, bytes, None)`
/// then `delete(old)`. Returns `Ok(())` on success OR on idempotent
/// re-application (`new_key` already contains a lesson with the same
/// id). Returns `EngineError::Parse` if `new_key` contains a
/// DIFFERENT lesson (malformed half-state requiring inspection).
async fn move_lesson_file(
    storage: &dyn Storage,
    old_key: &StorageKey,
    new_key: &StorageKey,
    new_bytes: Bytes,
    expected_id: &str,
) -> Result<(), EngineError> {
    let written = storage.put_if_version(new_key, new_bytes, None).await?;
    if !written {
        // Already exists at new_key — re-read + verify.
        let existing = storage.get(new_key).await?.ok_or_else(|| {
            EngineError::Parse(format!(
                "lesson move: put_if_version returned Ok(false) but get returned None at {new_key}"
            ))
        })?;
        let content = std::str::from_utf8(&existing)
            .map_err(|e| EngineError::Parse(format!("non-utf8 lesson bytes at {new_key}: {e}")))?;
        let split = split_frontmatter_normalized(content)
            .map_err(|e| EngineError::Parse(format!("split frontmatter {new_key}: {e}")))?;
        let existing_fm =
            parse_lesson_frontmatter(&split.yaml).map_err(|e| EngineError::Yaml(e.into()))?;
        if existing_fm.id != expected_id {
            return Err(EngineError::Parse(format!(
                "lesson move: collision at {new_key} (expected id {expected_id}, found {})",
                existing_fm.id
            )));
        }
        // Idempotent: someone (likely us, on a prior partial attempt)
        // already wrote this lesson here. Fall through to cleanup.
    }
    if let Err(e) = storage.delete(old_key).await {
        warn!(
            old = %old_key,
            new = %new_key,
            error = %e,
            "lesson move: delete(old_key) failed; lesson may appear in both dirs until cleaned up"
        );
    }
    Ok(())
}

/// D-G1 + D-G2: decrement memory citation counters for every
/// `EvidenceRef::Memory(_)` in an immune lesson's causal narrative
/// (user-authored OR pack-authored, both confer immunity). Best-effort:
/// warn-log each failure but do NOT fail the parent transition.
async fn decrement_user_lesson_citations(
    ctx: &Context,
    storage: &dyn Storage,
    fm: &LessonFrontmatter,
) {
    if !fm.authored_by.is_immune() {
        return;
    }
    let Some(cn) = &fm.causal_narrative else {
        return;
    };
    for evr in &cn.evidence_refs {
        if let EvidenceRef::Memory(mid) = evr {
            if let Err(e) = decrement_citation_count(ctx, storage, mid).await {
                warn!(
                    lesson = %fm.id, memory = %mid, error = %e,
                    "transition: failed to decrement memory citation counter"
                );
            }
        }
    }
}

/// M-G4 helper: CAS-RMW the OLD key through the gate check + move.
/// Sentinel-writes the same bytes back to detect concurrent mutation.
/// If the version changed (someone bumped thumbs_down etc), re-loop;
/// re-run the gate with the new bytes; bounded by
/// `TRANSITION_CAS_MAX_RETRIES`.
async fn promote_cas_loop(
    storage: &dyn Storage,
    id: &str,
    old_key: &StorageKey,
    new_key: &StorageKey,
    config: &PromotionConfig,
    now: DateTime<Utc>,
) -> Result<(LessonFrontmatter, String), EngineError> {
    for _attempt in 0..TRANSITION_CAS_MAX_RETRIES {
        let Some((bytes, version)) = storage.get_with_version(old_key).await? else {
            return Err(EngineError::LessonNotFound { id: id.to_string() });
        };
        let content = std::str::from_utf8(&bytes)
            .map_err(|e| EngineError::Parse(format!("non-utf8 lesson bytes for {old_key}: {e}")))?;
        let split = split_frontmatter_normalized(content)
            .map_err(|e| EngineError::Parse(format!("split frontmatter {old_key}: {e}")))?;
        let live_fm =
            parse_lesson_frontmatter(&split.yaml).map_err(|e| EngineError::Yaml(e.into()))?;
        let metadata = storage
            .metadata(old_key)
            .await?
            .ok_or_else(|| EngineError::LessonNotFound { id: id.to_string() })?;
        match check_promotion_gate(&live_fm, &metadata, config, now) {
            GateDecision::Block { reasons } => {
                return Err(EngineError::PromotionBlocked { reasons });
            }
            GateDecision::Promote { .. } => {}
        }
        // Sentinel-CAS the OLD key to its current value — if someone
        // raced us we lose, re-loop, re-run the gate. If we win, the
        // move helper's create-only CAS at new_key is the second guard.
        let sentinel_written = storage
            .put_if_version(old_key, Bytes::from(bytes.to_vec()), Some(&version))
            .await?;
        if !sentinel_written {
            continue;
        }
        let mut fm = live_fm.clone();
        if fm.promotion_eligible_at.is_none() {
            fm.promotion_eligible_at = Some(now_iso(now));
        }
        fm.status = LessonStatus::Promoted;
        fm.updated_at = Some(now_iso(now));
        let new_yaml = serialize_lesson_frontmatter(&fm);
        let body = split.body.trim_start_matches('\n').to_string();
        let new_bytes = Bytes::from(combine_frontmatter(&new_yaml, &body));
        move_lesson_file(storage, old_key, new_key, new_bytes, id).await?;
        return Ok((fm, body));
    }
    Err(EngineError::CasContended {
        key: old_key.as_str().to_string(),
        retries: TRANSITION_CAS_MAX_RETRIES,
    })
}

/// D-G3: `promote_lesson` enforces the gate internally and stamps
/// `promotion_eligible_at` if not already set. On `Block`, returns
/// `EngineError::PromotionBlocked`. On `Promote`, moves the lesson
/// file `active/ → promoted/`.
pub async fn promote(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
    config: &PromotionConfig,
    now: DateTime<Utc>,
) -> Result<LoadedLesson, EngineError> {
    // M-G4 fix: CAS-RMW loop — gate re-runs on every iteration so a
    // concurrent thumbs_down between the gate check and the move can't
    // smuggle a now-blocked lesson through.
    let initial = get_by_id(ctx, storage, id)
        .await?
        .ok_or_else(|| EngineError::LessonNotFound { id: id.to_string() })?;
    let old_key = StorageKey::lesson(ctx, &initial.status_dir, id);
    let new_key = StorageKey::lesson(ctx, "promoted", id);
    let (fm, body) = promote_cas_loop(storage, id, &old_key, &new_key, config, now).await?;
    // D-G6: best-effort skill lesson-history append (only when the
    // lesson has both a target_skill AND a causal_narrative).
    if let (Some(skill_id), Some(cn)) = (&fm.target_skill, &fm.causal_narrative) {
        let entry = format!(
            "- {{ lesson_id: {}, promoted_at: \"{}\", narrative_summary: {}, authored_by: {} }}\n",
            id,
            now_iso(now),
            yaml_inline_scalar(&cn.trigger),
            fm.authored_by.as_str(),
        );
        // M-G3 fix: D-G6 sub-clause — skip if the skill record doesn't
        // exist. Orphan history is not worth keeping; the audit trail
        // assumes a real skill at the corresponding directory.
        let skill_key = StorageKey::skill(ctx, skill_id);
        let skill_exists = storage.get(&skill_key).await.ok().flatten().is_some();
        if !skill_exists {
            warn!(
                lesson = %id, skill = %skill_id,
                "promote: target_skill points at non-existent skill; skipping audit append"
            );
        } else {
            let history_key = StorageKey::skill_history(ctx, skill_id);
            let existing = storage.get(&history_key).await.ok().flatten();
            let new_contents = match existing {
                Some(b) => {
                    let mut s = String::from_utf8_lossy(&b).into_owned();
                    if !s.ends_with('\n') {
                        s.push('\n');
                    }
                    s.push_str(&entry);
                    s
                }
                None => entry,
            };
            if let Err(e) = storage.put(&history_key, Bytes::from(new_contents)).await {
                warn!(
                    lesson = %id, skill = %skill_id, error = %e,
                    "promote: best-effort lesson-history append failed"
                );
            }
        }
    }
    Ok(LoadedLesson {
        path: std::path::PathBuf::from(new_key.as_str()),
        status_dir: "promoted".to_string(),
        frontmatter: fm,
        body,
    })
}

/// Render `s` as a YAML single-line scalar suitable for inline-map
/// values. Escapes double quotes and backslashes; falls back to
/// a literal `""` if the string contains characters that would
/// require multi-line formatting (newline / CR).
fn yaml_inline_scalar(s: &str) -> String {
    if s.contains('\n') || s.contains('\r') {
        return "\"\"".to_string();
    }
    let escaped: String = s
        .chars()
        .fold(String::with_capacity(s.len() + 2), |mut acc, c| {
            match c {
                '"' => acc.push_str("\\\""),
                '\\' => acc.push_str("\\\\"),
                other => acc.push(other),
            }
            acc
        });
    format!("\"{escaped}\"")
}

/// D-G8: terminate the lesson (move `active/ → discarded/`).
/// User-authored lessons immune unless `force=true`. D-G1: on a
/// user-authored discard, decrement cited memories' immunity
/// counters (best-effort).
pub async fn discard(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
    reason: Option<String>,
    force: bool,
    now: DateTime<Utc>,
) -> Result<LoadedLesson, EngineError> {
    let loaded = get_by_id(ctx, storage, id)
        .await?
        .ok_or_else(|| EngineError::LessonNotFound { id: id.to_string() })?;
    if !force && loaded.frontmatter.authored_by.is_immune() {
        return Err(EngineError::UserLessonImmune { id: id.to_string() });
    }
    let old_key = StorageKey::lesson(ctx, &loaded.status_dir, id);
    let mut fm = loaded.frontmatter.clone();
    fm.status = LessonStatus::Discarded;
    fm.updated_at = Some(now_iso(now));
    let new_yaml = serialize_lesson_frontmatter(&fm);
    let mut body = loaded.body.trim_start_matches('\n').to_string();
    if let Some(r) = reason {
        body.push_str(&format!(
            "\n<!-- discard reason: {r} at {} -->\n",
            now_iso(now)
        ));
    }
    let new_bytes = Bytes::from(combine_frontmatter(&new_yaml, &body));
    let new_key = StorageKey::lesson(ctx, "discarded", id);
    move_lesson_file(storage, &old_key, &new_key, new_bytes, id).await?;
    // D-G1: user-authored lesson discard decrements cited memories'
    // immunity counters. Best-effort.
    decrement_user_lesson_citations(ctx, storage, &fm).await;
    Ok(LoadedLesson {
        path: std::path::PathBuf::from(new_key.as_str()),
        status_dir: "discarded".to_string(),
        frontmatter: fm,
        body,
    })
}

/// D-G7: move `old_id` to `superseded/` and stamp
/// `superseded_by = new_id` + `superseded_at`. Rejects:
/// self-reference, missing replacement, cycle.
pub async fn supersede(
    ctx: &Context,
    storage: &dyn Storage,
    old_id: &str,
    new_id: &str,
    force: bool,
    now: DateTime<Utc>,
) -> Result<LoadedLesson, EngineError> {
    if old_id == new_id {
        return Err(EngineError::LessonSupersedeInvalid {
            id: old_id.to_string(),
            reason: SupersedeBlockReason::SelfReference {
                id: old_id.to_string(),
            },
        });
    }
    // Replacement must exist.
    let replacement = get_by_id(ctx, storage, new_id).await?;
    if replacement.is_none() {
        return Err(EngineError::LessonSupersedeInvalid {
            id: old_id.to_string(),
            reason: SupersedeBlockReason::ReplacementNotFound {
                replacement_id: new_id.to_string(),
            },
        });
    }
    // Cycle check: walking superseded_by forward from new_id must
    // not lead back to old_id within the depth cap.
    let mut chain = vec![new_id.to_string()];
    let mut cursor = new_id.to_string();
    let mut walked_to_end = false;
    for _ in 0..SUPERSEDE_CYCLE_DEPTH_CAP {
        let next = match get_by_id(ctx, storage, &cursor).await? {
            Some(l) => l.frontmatter.superseded_by.clone(),
            None => {
                walked_to_end = true;
                break;
            }
        };
        let Some(next_id) = next else {
            walked_to_end = true;
            break;
        };
        if next_id == old_id {
            chain.push(next_id);
            return Err(EngineError::LessonSupersedeInvalid {
                id: old_id.to_string(),
                reason: SupersedeBlockReason::CycleDetected { chain },
            });
        }
        chain.push(next_id.clone());
        cursor = next_id;
    }
    // M-G1 fix: depth-cap exhaustion without reaching chain end is
    // itself a cycle signal — refuse rather than silently proceed.
    // Matches Phase E2 compression-cycle precedent.
    if !walked_to_end {
        return Err(EngineError::LessonSupersedeInvalid {
            id: old_id.to_string(),
            reason: SupersedeBlockReason::CycleDetected { chain },
        });
    }
    // Load the old lesson + apply immunity guard.
    let loaded =
        get_by_id(ctx, storage, old_id)
            .await?
            .ok_or_else(|| EngineError::LessonNotFound {
                id: old_id.to_string(),
            })?;
    if !force && loaded.frontmatter.authored_by.is_immune() {
        return Err(EngineError::UserLessonImmune {
            id: old_id.to_string(),
        });
    }
    let old_key = StorageKey::lesson(ctx, &loaded.status_dir, old_id);
    let mut fm = loaded.frontmatter.clone();
    fm.status = LessonStatus::Superseded;
    fm.superseded_by = Some(new_id.to_string());
    fm.superseded_at = Some(now_iso(now));
    fm.updated_at = Some(now_iso(now));
    let new_yaml = serialize_lesson_frontmatter(&fm);
    let body = loaded.body.trim_start_matches('\n').to_string();
    let new_bytes = Bytes::from(combine_frontmatter(&new_yaml, &body));
    let new_key = StorageKey::lesson(ctx, "superseded", old_id);
    move_lesson_file(storage, &old_key, &new_key, new_bytes, old_id).await?;
    decrement_user_lesson_citations(ctx, storage, &fm).await;
    Ok(LoadedLesson {
        path: std::path::PathBuf::from(new_key.as_str()),
        status_dir: "superseded".to_string(),
        frontmatter: fm,
        body,
    })
}

/// D-G4: mutate `thumbs_up_count` / `thumbs_down_count` + add
/// signal source + optionally append a body audit-line. CAS-RMW.
pub async fn capture_feedback(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
    polarity: FeedbackPolarity,
    source_signal_id: Option<String>,
    now: DateTime<Utc>,
) -> Result<LoadedLesson, EngineError> {
    let initial = get_by_id(ctx, storage, id)
        .await?
        .ok_or_else(|| EngineError::LessonNotFound { id: id.to_string() })?;
    let status_dir = initial.status_dir.clone();
    let key = StorageKey::lesson(ctx, &status_dir, id);
    for _attempt in 0..TRANSITION_CAS_MAX_RETRIES {
        let Some((bytes, version)) = storage.get_with_version(&key).await? else {
            return Err(EngineError::LessonNotFound { id: id.to_string() });
        };
        let content = std::str::from_utf8(&bytes)
            .map_err(|e| EngineError::Parse(format!("non-utf8 lesson bytes for {key}: {e}")))?;
        let split = split_frontmatter_normalized(content)
            .map_err(|e| EngineError::Parse(format!("split frontmatter {key}: {e}")))?;
        let mut fm =
            parse_lesson_frontmatter(&split.yaml).map_err(|e| EngineError::Yaml(e.into()))?;
        match polarity {
            FeedbackPolarity::ThumbsUp => fm.thumbs_up_count = fm.thumbs_up_count.saturating_add(1),
            FeedbackPolarity::ThumbsDown => {
                fm.thumbs_down_count = fm.thumbs_down_count.saturating_add(1)
            }
        }
        // Idempotent set-add to external_signal_sources.
        let source = polarity.signal_source();
        if !fm.external_signal_sources.iter().any(|s| s == source) {
            fm.external_signal_sources.push(source.to_string());
        }
        fm.updated_at = Some(now_iso(now));
        let new_yaml = serialize_lesson_frontmatter(&fm);
        let mut body = split.body.trim_start_matches('\n').to_string();
        if let Some(sig_id) = &source_signal_id {
            body.push_str(&format!(
                "\n<!-- feedback: {} by {} at {} -->\n",
                source,
                sig_id,
                now_iso(now)
            ));
        }
        let new_contents = combine_frontmatter(&new_yaml, &body);
        let written = storage
            .put_if_version(&key, Bytes::from(new_contents), Some(&version))
            .await?;
        if written {
            return Ok(LoadedLesson {
                path: std::path::PathBuf::from(key.as_str()),
                status_dir,
                frontmatter: fm,
                body,
            });
        }
    }
    Err(EngineError::CasContended {
        key: key.as_str().to_string(),
        retries: TRANSITION_CAS_MAX_RETRIES,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::embedding::MockEmbedder;
    use crate::engine::memory::{insert as insert_memory, MemoryId};
    use crate::engine::storage::MemoryStorage;
    use crate::engine::test_support::TestHarness;
    use crate::engine::vector::HnswVectorIndex;
    use crate::engine::yaml::{Authorship, CausalNarrative, Confidence, EvidenceRef, GeneratedBy};
    use std::sync::Arc;

    fn now() -> DateTime<Utc> {
        "2026-05-14T12:00:00Z".parse().unwrap()
    }

    fn unit_vec(dim: usize, axis: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; dim];
        v[axis % dim] = 1.0;
        v
    }

    async fn seed_lesson_full(
        h: &TestHarness,
        status: &str,
        id: &str,
        authored_by: Authorship,
        memory_refs: Vec<MemoryId>,
    ) -> StorageKey {
        let fm = LessonFrontmatter {
            id: id.into(),
            description: "test lesson".into(),
            status: match status {
                "active" => LessonStatus::Active,
                "pending" => LessonStatus::Pending,
                _ => LessonStatus::Active,
            },
            created_at: "2026-05-13T00:00:00Z".into(),
            updated_at: None,
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
            authored_by,
            pack_id: None,
            external_id: None,
            causal_narrative: if memory_refs.is_empty() {
                None
            } else {
                Some(CausalNarrative {
                    trigger: "t".into(),
                    failure_mode: "f".into(),
                    correction: "c".into(),
                    confidence: Confidence::Inferred,
                    evidence_refs: memory_refs.into_iter().map(EvidenceRef::Memory).collect(),
                    generated_by: GeneratedBy::User,
                    generated_at: "2026-05-14T00:00:00Z".into(),
                })
            },
        };
        let yaml = serialize_lesson_frontmatter(&fm);
        let content = combine_frontmatter(&yaml, "body\n");
        let key = StorageKey::lesson(&h.ctx, status, id);
        h.storage.put(&key, Bytes::from(content)).await.unwrap();
        key
    }

    #[tokio::test]
    async fn discard_moves_file_to_discarded_dir() {
        let h = TestHarness::in_memory();
        seed_lesson_full(&h, "active", "les-disc00001", Authorship::Llm, vec![]).await;
        let result = discard(
            &h.ctx,
            h.storage.as_ref(),
            "les-disc00001",
            None,
            false,
            now(),
        )
        .await
        .unwrap();
        assert_eq!(result.status_dir, "discarded");
        assert_eq!(result.frontmatter.status, LessonStatus::Discarded);
        // File moved.
        let old_key = StorageKey::lesson(&h.ctx, "active", "les-disc00001");
        let new_key = StorageKey::lesson(&h.ctx, "discarded", "les-disc00001");
        assert!(h.storage.get(&old_key).await.unwrap().is_none());
        assert!(h.storage.get(&new_key).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn discard_user_authored_without_force_refuses() {
        let h = TestHarness::in_memory();
        seed_lesson_full(&h, "active", "les-usrl00001", Authorship::User, vec![]).await;
        let r = discard(
            &h.ctx,
            h.storage.as_ref(),
            "les-usrl00001",
            None,
            false,
            now(),
        )
        .await;
        assert!(matches!(r, Err(EngineError::UserLessonImmune { .. })));
        // Lesson still active.
        let key = StorageKey::lesson(&h.ctx, "active", "les-usrl00001");
        assert!(h.storage.get(&key).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn discard_user_authored_with_force_decrements_memory_citations() {
        let h = TestHarness::in_memory();
        let storage: Arc<dyn Storage> = h.storage.clone();
        let vidx = HnswVectorIndex::new(4);
        // Insert a memory + bump its counter (simulating earlier
        // user-lesson citation).
        let mid = MemoryId::new("mem-disc0001");
        let emb = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
        insert_memory(
            &h.ctx,
            storage.as_ref(),
            &emb,
            &vidx,
            mid.clone(),
            "x",
            "y",
            now(),
        )
        .await
        .unwrap();
        crate::engine::memory::increment_citation_count(&h.ctx, storage.as_ref(), &mid)
            .await
            .unwrap();
        let pre = crate::engine::memory::get_by_id(&h.ctx, storage.as_ref(), &mid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pre.frontmatter.consumed_by_user_lessons, 1);
        // Seed + discard user-authored lesson citing this memory.
        seed_lesson_full(
            &h,
            "active",
            "les-dec00001",
            Authorship::User,
            vec![mid.clone()],
        )
        .await;
        discard(
            &h.ctx,
            h.storage.as_ref(),
            "les-dec00001",
            Some("user-asked".into()),
            true,
            now(),
        )
        .await
        .unwrap();
        let post = crate::engine::memory::get_by_id(&h.ctx, storage.as_ref(), &mid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            post.frontmatter.consumed_by_user_lessons, 0,
            "user-authored lesson discard must decrement cited memories"
        );
    }

    #[tokio::test]
    async fn discard_llm_authored_does_not_decrement_citations() {
        let h = TestHarness::in_memory();
        let storage: Arc<dyn Storage> = h.storage.clone();
        let vidx = HnswVectorIndex::new(4);
        let mid = MemoryId::new("mem-llm00001");
        let emb = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
        insert_memory(
            &h.ctx,
            storage.as_ref(),
            &emb,
            &vidx,
            mid.clone(),
            "x",
            "y",
            now(),
        )
        .await
        .unwrap();
        crate::engine::memory::increment_citation_count(&h.ctx, storage.as_ref(), &mid)
            .await
            .unwrap();
        seed_lesson_full(
            &h,
            "active",
            "les-llmd00001",
            Authorship::Llm,
            vec![mid.clone()],
        )
        .await;
        discard(
            &h.ctx,
            h.storage.as_ref(),
            "les-llmd00001",
            None,
            false,
            now(),
        )
        .await
        .unwrap();
        let post = crate::engine::memory::get_by_id(&h.ctx, storage.as_ref(), &mid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            post.frontmatter.consumed_by_user_lessons, 1,
            "LLM-authored lesson discard MUST NOT decrement"
        );
    }

    #[tokio::test]
    async fn discard_idempotent_on_replay() {
        // Second discard of a lesson already at discarded/ should
        // succeed (move helper hits the "already exists" path).
        let h = TestHarness::in_memory();
        seed_lesson_full(&h, "active", "les-idem00001", Authorship::Llm, vec![]).await;
        discard(
            &h.ctx,
            h.storage.as_ref(),
            "les-idem00001",
            None,
            false,
            now(),
        )
        .await
        .unwrap();
        // Re-seed at "active" (simulating a half-applied transition leftover).
        seed_lesson_full(&h, "active", "les-idem00001", Authorship::Llm, vec![]).await;
        // Now discard again — should succeed via idempotent path.
        let r = discard(
            &h.ctx,
            h.storage.as_ref(),
            "les-idem00001",
            None,
            false,
            now(),
        )
        .await;
        assert!(r.is_ok(), "got {r:?}");
    }

    #[tokio::test]
    async fn supersede_self_reference_rejected() {
        let h = TestHarness::in_memory();
        seed_lesson_full(&h, "active", "les-self00001", Authorship::Llm, vec![]).await;
        let r = supersede(
            &h.ctx,
            h.storage.as_ref(),
            "les-self00001",
            "les-self00001",
            false,
            now(),
        )
        .await;
        match r {
            Err(EngineError::LessonSupersedeInvalid {
                reason: SupersedeBlockReason::SelfReference { .. },
                ..
            }) => {}
            other => panic!("expected SelfReference, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn supersede_missing_replacement_rejected() {
        let h = TestHarness::in_memory();
        seed_lesson_full(&h, "active", "les-orig00001", Authorship::Llm, vec![]).await;
        let r = supersede(
            &h.ctx,
            h.storage.as_ref(),
            "les-orig00001",
            "les-nope00001",
            false,
            now(),
        )
        .await;
        match r {
            Err(EngineError::LessonSupersedeInvalid {
                reason: SupersedeBlockReason::ReplacementNotFound { .. },
                ..
            }) => {}
            other => panic!("expected ReplacementNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn supersede_cycle_detected() {
        // B → A (already superseded by A). Then attempt supersede(A → B):
        // walking superseded_by from B yields A, which is the old_id → cycle.
        let h = TestHarness::in_memory();
        seed_lesson_full(&h, "active", "les-A0000001", Authorship::Llm, vec![]).await;
        // Pre-make B with superseded_by=A.
        let fm = LessonFrontmatter {
            id: "les-B0000001".into(),
            description: "B".into(),
            status: LessonStatus::Superseded,
            created_at: "2026-05-13T00:00:00Z".into(),
            updated_at: None,
            target_skill: None,
            source_feedback_ids: None,
            applied_count: 0,
            last_applied_at: None,
            thumbs_up_count: 0,
            thumbs_down_count: 0,
            external_signal_sources: vec![],
            applied_session_ids: vec![],
            promotion_eligible_at: None,
            superseded_by: Some("les-A0000001".into()),
            superseded_at: Some("2026-05-13T00:00:00Z".into()),
            ingest_provenance: None,
            authored_by: Authorship::Llm,
            pack_id: None,
            external_id: None,
            causal_narrative: None,
        };
        let yaml = serialize_lesson_frontmatter(&fm);
        let key = StorageKey::lesson(&h.ctx, "superseded", "les-B0000001");
        h.storage
            .put(&key, Bytes::from(combine_frontmatter(&yaml, "body\n")))
            .await
            .unwrap();
        let r = supersede(
            &h.ctx,
            h.storage.as_ref(),
            "les-A0000001",
            "les-B0000001",
            false,
            now(),
        )
        .await;
        match r {
            Err(EngineError::LessonSupersedeInvalid {
                reason: SupersedeBlockReason::CycleDetected { .. },
                ..
            }) => {}
            other => panic!("expected CycleDetected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn supersede_user_authored_without_force_refuses() {
        let h = TestHarness::in_memory();
        seed_lesson_full(&h, "active", "les-suusr0001", Authorship::User, vec![]).await;
        seed_lesson_full(&h, "active", "les-sunew0001", Authorship::Llm, vec![]).await;
        let r = supersede(
            &h.ctx,
            h.storage.as_ref(),
            "les-suusr0001",
            "les-sunew0001",
            false,
            now(),
        )
        .await;
        assert!(matches!(r, Err(EngineError::UserLessonImmune { .. })));
    }

    #[tokio::test]
    async fn supersede_user_authored_with_force_decrements_memory_citations() {
        // M-G2 fix: the wedge invariant on supersede is symmetric to
        // discard — user-authored supersede must decrement cited
        // memories' immunity counters.
        let h = TestHarness::in_memory();
        let storage: Arc<dyn Storage> = h.storage.clone();
        let vidx = HnswVectorIndex::new(4);
        let mid = MemoryId::new("mem-sudec0001");
        let emb = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
        insert_memory(
            &h.ctx,
            storage.as_ref(),
            &emb,
            &vidx,
            mid.clone(),
            "x",
            "y",
            now(),
        )
        .await
        .unwrap();
        crate::engine::memory::increment_citation_count(&h.ctx, storage.as_ref(), &mid)
            .await
            .unwrap();
        let pre = crate::engine::memory::get_by_id(&h.ctx, storage.as_ref(), &mid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pre.frontmatter.consumed_by_user_lessons, 1);
        seed_lesson_full(
            &h,
            "active",
            "les-suold0001",
            Authorship::User,
            vec![mid.clone()],
        )
        .await;
        seed_lesson_full(&h, "active", "les-sunew0002", Authorship::Llm, vec![]).await;
        supersede(
            &h.ctx,
            h.storage.as_ref(),
            "les-suold0001",
            "les-sunew0002",
            true,
            now(),
        )
        .await
        .unwrap();
        let post = crate::engine::memory::get_by_id(&h.ctx, storage.as_ref(), &mid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            post.frontmatter.consumed_by_user_lessons, 0,
            "user-authored lesson supersede must decrement cited memories (wedge invariant)"
        );
    }

    #[tokio::test]
    async fn supersede_depth_cap_treated_as_cycle() {
        // M-G1 fix: walking superseded_by past depth cap without
        // reaching chain end must refuse.
        let h = TestHarness::in_memory();
        for i in 0..20 {
            let next_id = if i < 19 {
                Some(format!("les-d{:03}", i + 1))
            } else {
                Some("les-d000".into())
            };
            let fm = LessonFrontmatter {
                id: format!("les-d{:03}", i),
                description: "chain".into(),
                status: LessonStatus::Superseded,
                created_at: "2026-05-13T00:00:00Z".into(),
                updated_at: None,
                target_skill: None,
                source_feedback_ids: None,
                applied_count: 0,
                last_applied_at: None,
                thumbs_up_count: 0,
                thumbs_down_count: 0,
                external_signal_sources: vec![],
                applied_session_ids: vec![],
                promotion_eligible_at: None,
                superseded_by: next_id,
                superseded_at: Some("2026-05-13T00:00:00Z".into()),
                ingest_provenance: None,
                authored_by: Authorship::Llm,
                pack_id: None,
                external_id: None,
                causal_narrative: None,
            };
            let yaml = serialize_lesson_frontmatter(&fm);
            let key = StorageKey::lesson(&h.ctx, "superseded", &format!("les-d{:03}", i));
            h.storage
                .put(&key, Bytes::from(combine_frontmatter(&yaml, "body\n")))
                .await
                .unwrap();
        }
        seed_lesson_full(&h, "active", "les-orig00002", Authorship::Llm, vec![]).await;
        let r = supersede(
            &h.ctx,
            h.storage.as_ref(),
            "les-orig00002",
            "les-d000",
            false,
            now(),
        )
        .await;
        match r {
            Err(EngineError::LessonSupersedeInvalid {
                reason: SupersedeBlockReason::CycleDetected { .. },
                ..
            }) => {}
            other => panic!("expected CycleDetected at depth cap, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn supersede_happy_path_moves_and_stamps_fields() {
        let h = TestHarness::in_memory();
        seed_lesson_full(&h, "active", "les-old00001", Authorship::Llm, vec![]).await;
        seed_lesson_full(&h, "active", "les-new00001", Authorship::Llm, vec![]).await;
        let r = supersede(
            &h.ctx,
            h.storage.as_ref(),
            "les-old00001",
            "les-new00001",
            false,
            now(),
        )
        .await
        .unwrap();
        assert_eq!(r.status_dir, "superseded");
        assert_eq!(r.frontmatter.superseded_by.as_deref(), Some("les-new00001"));
        assert!(r.frontmatter.superseded_at.is_some());
    }

    #[tokio::test]
    async fn capture_feedback_thumbs_up_increments_and_adds_signal() {
        let h = TestHarness::in_memory();
        seed_lesson_full(&h, "active", "les-fb000001", Authorship::Llm, vec![]).await;
        let r = capture_feedback(
            &h.ctx,
            h.storage.as_ref(),
            "les-fb000001",
            FeedbackPolarity::ThumbsUp,
            Some("sig-12345".to_string()),
            now(),
        )
        .await
        .unwrap();
        assert_eq!(r.frontmatter.thumbs_up_count, 1);
        assert_eq!(r.frontmatter.thumbs_down_count, 0);
        assert!(r
            .frontmatter
            .external_signal_sources
            .iter()
            .any(|s| s == "user_thumbs_up"));
        assert!(r
            .body
            .contains("<!-- feedback: user_thumbs_up by sig-12345"));
    }

    #[tokio::test]
    async fn capture_feedback_idempotent_signal_set_add() {
        let h = TestHarness::in_memory();
        seed_lesson_full(&h, "active", "les-fb000002", Authorship::Llm, vec![]).await;
        // Call thumbs_up twice — counter goes to 2 but signal list
        // still has exactly one entry.
        capture_feedback(
            &h.ctx,
            h.storage.as_ref(),
            "les-fb000002",
            FeedbackPolarity::ThumbsUp,
            None,
            now(),
        )
        .await
        .unwrap();
        let r = capture_feedback(
            &h.ctx,
            h.storage.as_ref(),
            "les-fb000002",
            FeedbackPolarity::ThumbsUp,
            None,
            now(),
        )
        .await
        .unwrap();
        assert_eq!(r.frontmatter.thumbs_up_count, 2);
        let up_count = r
            .frontmatter
            .external_signal_sources
            .iter()
            .filter(|s| *s == "user_thumbs_up")
            .count();
        assert_eq!(up_count, 1);
    }

    #[tokio::test]
    async fn promote_blocked_surfaces_typed_error() {
        // A fresh lesson without causal_narrative fails the gate
        // (missing-causal-narrative is one of the BlockReasons).
        let h = TestHarness::in_memory();
        seed_lesson_full(&h, "active", "les-prom00001", Authorship::Llm, vec![]).await;
        let r = promote(
            &h.ctx,
            h.storage.as_ref(),
            "les-prom00001",
            &PromotionConfig::default(),
            now(),
        )
        .await;
        assert!(
            matches!(r, Err(EngineError::PromotionBlocked { .. })),
            "got {r:?}"
        );
    }

    #[tokio::test]
    async fn move_lesson_file_collision_with_different_id_errors() {
        // Set up a "different lesson at new_key" scenario.
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let ctx = crate::engine::context::Context::single_user_local();
        let old_key = StorageKey::lesson(&ctx, "active", "les-mv0000001");
        let new_key = StorageKey::lesson(&ctx, "discarded", "les-mv0000001");
        // Pre-seed a DIFFERENT lesson at new_key.
        let other_fm = LessonFrontmatter {
            id: "les-OTHERXXX".into(),
            description: "other".into(),
            status: LessonStatus::Discarded,
            created_at: "2026-05-13T00:00:00Z".into(),
            updated_at: None,
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
            authored_by: Authorship::Llm,
            pack_id: None,
            external_id: None,
            causal_narrative: None,
        };
        let yaml = serialize_lesson_frontmatter(&other_fm);
        storage
            .put(&new_key, Bytes::from(combine_frontmatter(&yaml, "body")))
            .await
            .unwrap();
        // Now try the helper expecting id "les-mv0000001".
        let r = move_lesson_file(
            storage.as_ref(),
            &old_key,
            &new_key,
            Bytes::from("any"),
            "les-mv0000001",
        )
        .await;
        assert!(matches!(r, Err(EngineError::Parse(_))), "got {r:?}");
    }
}
