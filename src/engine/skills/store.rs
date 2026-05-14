//! Skill CRUD — Phase F C-F2.
//!
//! Free-function API matching Phase B/C/D/E precedent. All async.
//! All take `&Context` first. All return `EngineError`.
//!
//! User-authored skills are eviction-immune (D-F10 + the
//! `feedback_user_authored_lessons_immune.md` cascade): engine-
//! initiated `archive_skill(force=false)` / `delete_skill(force=false)`
//! refuses with `UserSkillImmune`. User-initiated paths pass
//! `force=true` to bypass.

use bytes::Bytes;
use tracing::warn;

use crate::engine::context::Context;
use crate::engine::error::EngineError;
use crate::engine::skills::{Skill, SkillFrontmatter, SkillStatus};
use crate::engine::storage::{Storage, StorageKey};
use crate::engine::yaml::{combine_frontmatter, split_frontmatter_normalized};

/// CAS-RMW retry budget for `update_skill` and lifecycle transitions.
const SKILL_CAS_MAX_RETRIES: u32 = 5;

fn render_skill_yaml(fm: &SkillFrontmatter, body: &str) -> Result<String, EngineError> {
    let yaml = serde_yml::to_string(fm).map_err(|e| EngineError::Yaml(Box::new(e)))?;
    Ok(combine_frontmatter(yaml.trim(), body))
}

fn parse_skill_file(bytes: &[u8]) -> Result<(SkillFrontmatter, String), EngineError> {
    let content = std::str::from_utf8(bytes)
        .map_err(|e| EngineError::Parse(format!("non-utf8 skill bytes: {e}")))?;
    let split = split_frontmatter_normalized(content)
        .map_err(|e| EngineError::Parse(format!("split frontmatter: {e}")))?;
    let fm: SkillFrontmatter =
        serde_yml::from_str(&split.yaml).map_err(|e| EngineError::Yaml(Box::new(e)))?;
    Ok((fm, split.body))
}

/// Insert a new skill. Fails if the skill id already exists (use
/// `update_skill` for in-place changes).
///
/// Phase F audit-fix close (C2 fix): when `frontmatter.authored_by ==
/// User` AND `frontmatter.evidence_refs` contains `EvidenceRef::
/// Memory(_)` entries, each cited memory's `consumed_by_user_lessons`
/// counter increments — making it eviction-immune via the wedge
/// invariant. Mirrors the lesson-citation behavior; the cross-cutting
/// wedge for skills.
pub async fn insert(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
    frontmatter: SkillFrontmatter,
    body: impl Into<String>,
) -> Result<Skill, EngineError> {
    let body = body.into();
    let key = StorageKey::skill(ctx, id);
    if storage.get(&key).await?.is_some() {
        return Err(EngineError::Parse(format!("skill already exists: {id}")));
    }
    let yaml = render_skill_yaml(&frontmatter, &body)?;
    storage.put(&key, Bytes::from(yaml)).await?;

    // C-F5 wedge wire-up: when the skill is user-authored, every
    // memory it cites gets its immunity counter bumped. We do this
    // BEST-EFFORT (warn on failure but don't fail the insert) — the
    // skill record is the source of truth; counters can be repaired
    // via `recompute_citation_counts` if they drift.
    if frontmatter.authored_by.is_user() {
        for evr in &frontmatter.evidence_refs {
            if let Some(mid) = evr.as_memory_id() {
                if let Err(e) =
                    crate::engine::memory::increment_citation_count(ctx, storage, mid).await
                {
                    warn!(
                        skill = %id, memory = %mid, error = %e,
                        "insert_skill: failed to increment memory citation counter"
                    );
                }
            }
        }
    }
    Ok(Skill::new(frontmatter, body))
}

/// Load a skill by id. Returns `Ok(None)` if absent.
pub async fn get_by_id(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
) -> Result<Option<Skill>, EngineError> {
    let key = StorageKey::skill(ctx, id);
    let bytes = match storage.get(&key).await? {
        Some(b) => b,
        None => return Ok(None),
    };
    let (fm, body) = parse_skill_file(&bytes)?;
    Ok(Some(Skill::new(fm, body)))
}

/// List all skills. Soft-fails per-entry on malformed frontmatter
/// (warn + skip).
pub async fn list(ctx: &Context, storage: &dyn Storage) -> Result<Vec<Skill>, EngineError> {
    let prefix = StorageKey::skills_prefix(ctx);
    let keys = storage.list(&prefix).await?;
    let mut out = Vec::new();
    for key in keys {
        if !key.as_str().ends_with("/SKILL.md") {
            continue;
        }
        let bytes = match storage.get(&key).await? {
            Some(b) => b,
            None => continue,
        };
        match parse_skill_file(&bytes) {
            Ok((fm, body)) => out.push(Skill::new(fm, body)),
            Err(e) => warn!(key = %key, error = %e, "list_skills: skipping unparseable"),
        }
    }
    Ok(out)
}

/// CAS-RMW update of a skill's frontmatter + body. Closure receives
/// `&mut SkillFrontmatter` + `&mut String body` and mutates in
/// place. 5-retry budget.
pub async fn update<F>(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
    f: F,
) -> Result<Skill, EngineError>
where
    F: Fn(&mut SkillFrontmatter, &mut String),
{
    let key = StorageKey::skill(ctx, id);
    for _attempt in 0..SKILL_CAS_MAX_RETRIES {
        let Some((bytes, version)) = storage.get_with_version(&key).await? else {
            return Err(EngineError::Parse(format!("skill not found: {id}")));
        };
        let (mut fm, mut body) = parse_skill_file(&bytes)?;
        f(&mut fm, &mut body);
        let new_yaml = render_skill_yaml(&fm, &body)?;
        let written = storage
            .put_if_version(&key, Bytes::from(new_yaml), Some(&version))
            .await?;
        if written {
            return Ok(Skill::new(fm, body));
        }
    }
    Err(EngineError::CasContended {
        key: key.as_str().to_string(),
        retries: SKILL_CAS_MAX_RETRIES,
    })
}

/// Archive a skill (lifecycle transition to `SkillStatus::Archived`).
///
/// **User-immunity respected by default** (D-F10): when `force =
/// false`, refuses with `UserSkillImmune` if the skill is
/// user-authored. User-initiated paths pass `force = true` to
/// bypass.
pub async fn archive(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
    force: bool,
) -> Result<Skill, EngineError> {
    let key = StorageKey::skill(ctx, id);
    if !force {
        let Some(bytes) = storage.get(&key).await? else {
            return Err(EngineError::Parse(format!("skill not found: {id}")));
        };
        let (fm, _body) = parse_skill_file(&bytes)?;
        if fm.authored_by.is_user() {
            return Err(EngineError::UserSkillImmune {
                id: id.to_string(),
                // (Phase F audit-fix close M2: `has_user_lessons` field removed)
            });
        }
    }
    update(ctx, storage, id, |fm, _body| {
        fm.status = SkillStatus::Archived;
    })
    .await
}

/// Permanently delete a skill. Same immunity-respect semantics as
/// `archive`.
pub async fn delete(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
    force: bool,
) -> Result<(), EngineError> {
    let key = StorageKey::skill(ctx, id);
    if !force {
        if let Some(bytes) = storage.get(&key).await? {
            let (fm, _body) = parse_skill_file(&bytes)?;
            if fm.authored_by.is_user() {
                return Err(EngineError::UserSkillImmune { id: id.to_string() });
            }
        }
    }
    storage.delete(&key).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::storage::MemoryStorage;
    use crate::engine::yaml::Authorship;
    use std::sync::Arc;

    fn ctx() -> Context {
        Context::single_user_local()
    }

    #[tokio::test]
    async fn insert_then_get_round_trips() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let fm = SkillFrontmatter::new("formatter", "auto-format on save");
        let s = insert(
            &ctx(),
            storage.as_ref(),
            "skl-fmt00001",
            fm.clone(),
            "body content",
        )
        .await
        .unwrap();
        assert_eq!(s.frontmatter.name, "formatter");
        let loaded = get_by_id(&ctx(), storage.as_ref(), "skl-fmt00001")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.frontmatter.name, "formatter");
        assert_eq!(loaded.body.trim(), "body content");
    }

    #[tokio::test]
    async fn insert_rejects_existing_id() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let fm = SkillFrontmatter::new("formatter", "x");
        insert(&ctx(), storage.as_ref(), "skl-dupe0001", fm.clone(), "body")
            .await
            .unwrap();
        let r = insert(&ctx(), storage.as_ref(), "skl-dupe0001", fm, "body again").await;
        assert!(matches!(r, Err(EngineError::Parse(_))));
    }

    #[tokio::test]
    async fn get_by_id_returns_none_for_missing() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let r = get_by_id(&ctx(), storage.as_ref(), "skl-noexist1")
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn list_returns_all_skills() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        for id in ["skl-list0001", "skl-list0002", "skl-list0003"] {
            let fm = SkillFrontmatter::new("name", "desc");
            insert(&ctx(), storage.as_ref(), id, fm, "body")
                .await
                .unwrap();
        }
        let skills = list(&ctx(), storage.as_ref()).await.unwrap();
        assert_eq!(skills.len(), 3);
    }

    #[tokio::test]
    async fn update_mutates_frontmatter() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let fm = SkillFrontmatter::new("formatter", "original desc");
        insert(&ctx(), storage.as_ref(), "skl-upd00001", fm, "body")
            .await
            .unwrap();
        let updated = update(&ctx(), storage.as_ref(), "skl-upd00001", |fm, _| {
            fm.description = "new desc".to_string();
            fm.status = SkillStatus::Active;
        })
        .await
        .unwrap();
        assert_eq!(updated.frontmatter.description, "new desc");
        assert_eq!(updated.frontmatter.status, SkillStatus::Active);
    }

    #[tokio::test]
    async fn archive_force_false_blocks_user_authored() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let mut fm = SkillFrontmatter::new("formatter", "x");
        fm.authored_by = Authorship::User;
        insert(&ctx(), storage.as_ref(), "skl-usr00001", fm, "body")
            .await
            .unwrap();
        let r = archive(&ctx(), storage.as_ref(), "skl-usr00001", false).await;
        match r {
            Err(EngineError::UserSkillImmune { id, .. }) => assert_eq!(id, "skl-usr00001"),
            other => panic!("expected UserSkillImmune, got {other:?}"),
        }
        // Skill remains Draft.
        let s = get_by_id(&ctx(), storage.as_ref(), "skl-usr00001")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(s.frontmatter.status, SkillStatus::Draft);
    }

    #[tokio::test]
    async fn archive_force_true_bypasses_immunity() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let mut fm = SkillFrontmatter::new("formatter", "x");
        fm.authored_by = Authorship::User;
        insert(&ctx(), storage.as_ref(), "skl-frc00001", fm, "body")
            .await
            .unwrap();
        let archived = archive(&ctx(), storage.as_ref(), "skl-frc00001", true)
            .await
            .unwrap();
        assert_eq!(archived.frontmatter.status, SkillStatus::Archived);
    }

    #[tokio::test]
    async fn archive_force_false_succeeds_on_llm_authored() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let fm = SkillFrontmatter::new("formatter", "x"); // default authored_by = Llm
        insert(&ctx(), storage.as_ref(), "skl-llm00001", fm, "body")
            .await
            .unwrap();
        let archived = archive(&ctx(), storage.as_ref(), "skl-llm00001", false)
            .await
            .unwrap();
        assert_eq!(archived.frontmatter.status, SkillStatus::Archived);
    }

    #[tokio::test]
    async fn delete_force_false_blocks_user_authored() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let mut fm = SkillFrontmatter::new("formatter", "x");
        fm.authored_by = Authorship::User;
        insert(&ctx(), storage.as_ref(), "skl-del00001", fm, "body")
            .await
            .unwrap();
        let r = delete(&ctx(), storage.as_ref(), "skl-del00001", false).await;
        assert!(matches!(r, Err(EngineError::UserSkillImmune { .. })));
        // Skill still present.
        assert!(get_by_id(&ctx(), storage.as_ref(), "skl-del00001")
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn delete_force_true_removes_skill() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let mut fm = SkillFrontmatter::new("formatter", "x");
        fm.authored_by = Authorship::User;
        insert(&ctx(), storage.as_ref(), "skl-rmf00001", fm, "body")
            .await
            .unwrap();
        delete(&ctx(), storage.as_ref(), "skl-rmf00001", true)
            .await
            .unwrap();
        assert!(get_by_id(&ctx(), storage.as_ref(), "skl-rmf00001")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn delete_idempotent_for_absent_id() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        // Delete an id that never existed — should be Ok regardless of force.
        delete(&ctx(), storage.as_ref(), "skl-noexist1", false)
            .await
            .unwrap();
        delete(&ctx(), storage.as_ref(), "skl-noexist1", true)
            .await
            .unwrap();
    }
}
