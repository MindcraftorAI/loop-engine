//! Team CRUD — Phase F C-F3.

use bytes::Bytes;
use tracing::warn;

use crate::engine::context::Context;
use crate::engine::error::EngineError;
use crate::engine::storage::{Storage, StorageKey};
use crate::engine::teams::{Team, TeamFrontmatter, TeamStatus};
use crate::engine::yaml::{combine_frontmatter, split_frontmatter_normalized};

const TEAM_CAS_MAX_RETRIES: u32 = 5;

fn render_yaml(fm: &TeamFrontmatter, body: &str) -> Result<String, EngineError> {
    let yaml = serde_yml::to_string(fm).map_err(|e| EngineError::Yaml(Box::new(e)))?;
    Ok(combine_frontmatter(yaml.trim(), body))
}

fn parse_file(bytes: &[u8]) -> Result<(TeamFrontmatter, String), EngineError> {
    let content = std::str::from_utf8(bytes)
        .map_err(|e| EngineError::Parse(format!("non-utf8 team bytes: {e}")))?;
    let split = split_frontmatter_normalized(content)
        .map_err(|e| EngineError::Parse(format!("split frontmatter: {e}")))?;
    let fm: TeamFrontmatter = serde_yml::from_str(&split.yaml)
        .map_err(|e| EngineError::Yaml(Box::new(e)))?;
    Ok((fm, split.body))
}

pub async fn insert(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
    frontmatter: TeamFrontmatter,
    body: impl Into<String>,
) -> Result<Team, EngineError> {
    let body = body.into();
    let key = StorageKey::team(ctx, id);
    if storage.get(&key).await?.is_some() {
        return Err(EngineError::Parse(format!("team already exists: {id}")));
    }
    let yaml = render_yaml(&frontmatter, &body)?;
    storage.put(&key, Bytes::from(yaml)).await?;
    Ok(Team::new(frontmatter, body))
}

pub async fn get_by_id(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
) -> Result<Option<Team>, EngineError> {
    let key = StorageKey::team(ctx, id);
    let bytes = match storage.get(&key).await? {
        Some(b) => b,
        None => return Ok(None),
    };
    let (fm, body) = parse_file(&bytes)?;
    Ok(Some(Team::new(fm, body)))
}

pub async fn list(ctx: &Context, storage: &dyn Storage) -> Result<Vec<Team>, EngineError> {
    let prefix = StorageKey::teams_prefix(ctx);
    let keys = storage.list(&prefix).await?;
    let mut out = Vec::new();
    for key in keys {
        if !key.as_str().ends_with("/TEAM.md") {
            continue;
        }
        let bytes = match storage.get(&key).await? {
            Some(b) => b,
            None => continue,
        };
        match parse_file(&bytes) {
            Ok((fm, body)) => out.push(Team::new(fm, body)),
            Err(e) => warn!(key = %key, error = %e, "list_teams: skipping unparseable"),
        }
    }
    Ok(out)
}

pub async fn update<F>(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
    f: F,
) -> Result<Team, EngineError>
where
    F: Fn(&mut TeamFrontmatter, &mut String),
{
    let key = StorageKey::team(ctx, id);
    for _attempt in 0..TEAM_CAS_MAX_RETRIES {
        let Some((bytes, version)) = storage.get_with_version(&key).await? else {
            return Err(EngineError::Parse(format!("team not found: {id}")));
        };
        let (mut fm, mut body) = parse_file(&bytes)?;
        f(&mut fm, &mut body);
        let new_yaml = render_yaml(&fm, &body)?;
        let written = storage
            .put_if_version(&key, Bytes::from(new_yaml), Some(&version))
            .await?;
        if written {
            return Ok(Team::new(fm, body));
        }
    }
    Err(EngineError::CasContended {
        key: key.as_str().to_string(),
        retries: TEAM_CAS_MAX_RETRIES,
    })
}

pub async fn archive(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
    force: bool,
) -> Result<Team, EngineError> {
    let key = StorageKey::team(ctx, id);
    if !force {
        let Some(bytes) = storage.get(&key).await? else {
            return Err(EngineError::Parse(format!("team not found: {id}")));
        };
        let (fm, _body) = parse_file(&bytes)?;
        if fm.authored_by.is_user() {
            return Err(EngineError::UserTeamImmune { id: id.to_string() });
        }
    }
    update(ctx, storage, id, |fm, _body| {
        fm.status = TeamStatus::Archived;
    })
    .await
}

pub async fn delete(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
    force: bool,
) -> Result<(), EngineError> {
    let key = StorageKey::team(ctx, id);
    if !force {
        if let Some(bytes) = storage.get(&key).await? {
            let (fm, _body) = parse_file(&bytes)?;
            if fm.authored_by.is_user() {
                return Err(EngineError::UserTeamImmune { id: id.to_string() });
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
    use crate::engine::teams::TeamMember;
    use crate::engine::yaml::Authorship;
    use std::sync::Arc;

    fn ctx() -> Context {
        Context::single_user_local()
    }

    #[tokio::test]
    async fn insert_and_get_with_members() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let mut fm = TeamFrontmatter::new("tm-engng", "Eng Team", "engineers");
        fm.members = vec![
            TeamMember::Persona("pers-a".into()),
            TeamMember::Skill("skl-b".into()),
        ];
        insert(&ctx(), storage.as_ref(), "tm-engng", fm, "charter body").await.unwrap();
        let t = get_by_id(&ctx(), storage.as_ref(), "tm-engng").await.unwrap().unwrap();
        assert_eq!(t.frontmatter.members.len(), 2);
        assert!(matches!(t.frontmatter.members[0], TeamMember::Persona(_)));
    }

    #[tokio::test]
    async fn archive_blocks_user_authored() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let mut fm = TeamFrontmatter::new("tm-usr00", "User Team", "");
        fm.authored_by = Authorship::User;
        insert(&ctx(), storage.as_ref(), "tm-usr00", fm, "").await.unwrap();
        let r = archive(&ctx(), storage.as_ref(), "tm-usr00", false).await;
        assert!(matches!(r, Err(EngineError::UserTeamImmune { .. })));
    }

    #[tokio::test]
    async fn list_returns_all_teams() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        for id in ["tm-l001", "tm-l002", "tm-l003"] {
            let fm = TeamFrontmatter::new(id, "n", "d");
            insert(&ctx(), storage.as_ref(), id, fm, "b").await.unwrap();
        }
        let r = list(&ctx(), storage.as_ref()).await.unwrap();
        assert_eq!(r.len(), 3);
    }
}
