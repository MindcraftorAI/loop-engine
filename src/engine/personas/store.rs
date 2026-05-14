//! Persona CRUD — Phase F C-F3. Free-function API; matches Skill +
//! Memory precedent. User-authored personas are eviction-immune
//! (`UserPersonaImmune`).

use bytes::Bytes;
use tracing::warn;

use crate::engine::context::Context;
use crate::engine::error::EngineError;
use crate::engine::personas::{Persona, PersonaFrontmatter, PersonaStatus};
use crate::engine::storage::{Storage, StorageKey};
use crate::engine::yaml::{combine_frontmatter, split_frontmatter_normalized};

const PERSONA_CAS_MAX_RETRIES: u32 = 5;

fn render_yaml(fm: &PersonaFrontmatter, body: &str) -> Result<String, EngineError> {
    let yaml = serde_yml::to_string(fm).map_err(|e| EngineError::Yaml(Box::new(e)))?;
    Ok(combine_frontmatter(yaml.trim(), body))
}

fn parse_file(bytes: &[u8]) -> Result<(PersonaFrontmatter, String), EngineError> {
    let content = std::str::from_utf8(bytes)
        .map_err(|e| EngineError::Parse(format!("non-utf8 persona bytes: {e}")))?;
    let split = split_frontmatter_normalized(content)
        .map_err(|e| EngineError::Parse(format!("split frontmatter: {e}")))?;
    let fm: PersonaFrontmatter = serde_yml::from_str(&split.yaml)
        .map_err(|e| EngineError::Yaml(Box::new(e)))?;
    Ok((fm, split.body))
}

pub async fn insert(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
    frontmatter: PersonaFrontmatter,
    body: impl Into<String>,
) -> Result<Persona, EngineError> {
    let body = body.into();
    let key = StorageKey::persona(ctx, id);
    if storage.get(&key).await?.is_some() {
        return Err(EngineError::Parse(format!("persona already exists: {id}")));
    }
    let yaml = render_yaml(&frontmatter, &body)?;
    storage.put(&key, Bytes::from(yaml)).await?;
    Ok(Persona::new(frontmatter, body))
}

pub async fn get_by_id(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
) -> Result<Option<Persona>, EngineError> {
    let key = StorageKey::persona(ctx, id);
    let bytes = match storage.get(&key).await? {
        Some(b) => b,
        None => return Ok(None),
    };
    let (fm, body) = parse_file(&bytes)?;
    Ok(Some(Persona::new(fm, body)))
}

pub async fn list(ctx: &Context, storage: &dyn Storage) -> Result<Vec<Persona>, EngineError> {
    let prefix = StorageKey::personas_prefix(ctx);
    let keys = storage.list(&prefix).await?;
    let mut out = Vec::new();
    for key in keys {
        if !key.as_str().ends_with("/PERSONA.md") {
            continue;
        }
        let bytes = match storage.get(&key).await? {
            Some(b) => b,
            None => continue,
        };
        match parse_file(&bytes) {
            Ok((fm, body)) => out.push(Persona::new(fm, body)),
            Err(e) => warn!(key = %key, error = %e, "list_personas: skipping unparseable"),
        }
    }
    Ok(out)
}

pub async fn update<F>(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
    f: F,
) -> Result<Persona, EngineError>
where
    F: Fn(&mut PersonaFrontmatter, &mut String),
{
    let key = StorageKey::persona(ctx, id);
    for _attempt in 0..PERSONA_CAS_MAX_RETRIES {
        let Some((bytes, version)) = storage.get_with_version(&key).await? else {
            return Err(EngineError::Parse(format!("persona not found: {id}")));
        };
        let (mut fm, mut body) = parse_file(&bytes)?;
        f(&mut fm, &mut body);
        let new_yaml = render_yaml(&fm, &body)?;
        let written = storage
            .put_if_version(&key, Bytes::from(new_yaml), Some(&version))
            .await?;
        if written {
            return Ok(Persona::new(fm, body));
        }
    }
    Err(EngineError::CasContended {
        key: key.as_str().to_string(),
        retries: PERSONA_CAS_MAX_RETRIES,
    })
}

pub async fn archive(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
    force: bool,
) -> Result<Persona, EngineError> {
    let key = StorageKey::persona(ctx, id);
    if !force {
        let Some(bytes) = storage.get(&key).await? else {
            return Err(EngineError::Parse(format!("persona not found: {id}")));
        };
        let (fm, _body) = parse_file(&bytes)?;
        if fm.authored_by.is_user() {
            return Err(EngineError::UserPersonaImmune { id: id.to_string() });
        }
    }
    update(ctx, storage, id, |fm, _body| {
        fm.status = PersonaStatus::Archived;
    })
    .await
}

pub async fn delete(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
    force: bool,
) -> Result<(), EngineError> {
    let key = StorageKey::persona(ctx, id);
    if !force {
        if let Some(bytes) = storage.get(&key).await? {
            let (fm, _body) = parse_file(&bytes)?;
            if fm.authored_by.is_user() {
                return Err(EngineError::UserPersonaImmune { id: id.to_string() });
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
        let fm = PersonaFrontmatter::new("pers-aaaa", "Maya", "patient mentor");
        insert(&ctx(), storage.as_ref(), "pers-aaaa", fm, "voice profile body").await.unwrap();
        let p = get_by_id(&ctx(), storage.as_ref(), "pers-aaaa").await.unwrap().unwrap();
        assert_eq!(p.frontmatter.name, "Maya");
        assert_eq!(p.body.trim(), "voice profile body");
    }

    #[tokio::test]
    async fn archive_blocks_user_authored_without_force() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let mut fm = PersonaFrontmatter::new("pers-usr1", "User", "");
        fm.authored_by = Authorship::User;
        insert(&ctx(), storage.as_ref(), "pers-usr1", fm, "").await.unwrap();
        let r = archive(&ctx(), storage.as_ref(), "pers-usr1", false).await;
        assert!(matches!(r, Err(EngineError::UserPersonaImmune { .. })));
    }

    #[tokio::test]
    async fn archive_force_true_bypasses() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let mut fm = PersonaFrontmatter::new("pers-frc1", "User", "");
        fm.authored_by = Authorship::User;
        insert(&ctx(), storage.as_ref(), "pers-frc1", fm, "").await.unwrap();
        let p = archive(&ctx(), storage.as_ref(), "pers-frc1", true).await.unwrap();
        assert_eq!(p.frontmatter.status, PersonaStatus::Archived);
    }

    #[tokio::test]
    async fn list_returns_all() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        for id in ["pers-l001", "pers-l002"] {
            let fm = PersonaFrontmatter::new(id, "n", "d");
            insert(&ctx(), storage.as_ref(), id, fm, "b").await.unwrap();
        }
        let r = list(&ctx(), storage.as_ref()).await.unwrap();
        assert_eq!(r.len(), 2);
    }
}
