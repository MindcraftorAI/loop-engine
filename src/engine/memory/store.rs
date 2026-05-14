//! Memory store — CRUD + search + prune. Phase E C-E3.
//!
//! Functions, not a struct (D-E5 — matches Phase B/C/D precedent).
//! All async. All take `&Context` first. All return `EngineError`.
//!
//! Wedge invariants:
//!   - `prune(predicate)` enforces the user-lesson immunity guard
//!     internally — host predicates can't bypass even by accident
//!     (D-E9). Skipped memories surface via `PruneStats::skipped_
//!     user_immune`.
//!   - `increment_citation_count` is the WRITE side of the immunity
//!     counter. Called by future Phase G `lessons::transitions::*`
//!     when a user-authored lesson cites the memory. Phase E ships
//!     the function; Phase G wires it in.
//!   - `decrement_citation_count` is reserved for Phase G —
//!     supersession / discard / unauthor paths decrement the counter.
//!
//! Embedding: memory body content is embedded at insert time via the
//! caller-supplied `Embedder` impl. The embedding goes into both the
//! `Memory.embedding: Option<Vec<f32>>` field (for in-memory work)
//! AND the on-disk sidecar `.vec` file (for persistence and
//! reload-friendly recomputes).

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde_yml;
use tracing::warn;

use crate::engine::context::Context;
use crate::engine::embedding::Embedder;
use crate::engine::error::EngineError;
use crate::engine::memory::{
    guarded_predicate, Memory, MemoryFrontmatter, MemoryId, MemoryQuery, MemoryRef,
    PrunePredicate, PruneStats,
};
use crate::engine::storage::{Storage, StorageKey};
use crate::engine::vector::{SearchHit, VectorIndex};
use crate::engine::yaml::{combine_frontmatter, split_frontmatter_normalized};

/// CAS-RMW retry budget for citation-counter updates (Phase A C5
/// pattern). 5 retries absorbs cross-process contention; exhaustion
/// surfaces as `EngineError::CasContended`.
const CITATION_CAS_MAX_RETRIES: u32 = 5;

/// `.vec` sidecar file holding the raw little-endian f32 embedding.
fn vec_key(ctx: &Context, id: &MemoryId) -> StorageKey {
    let suffix = format!("memories/{}.vec", id.as_str());
    if ctx.tenant_id.as_str() == "local" {
        StorageKey::from_raw(suffix)
    } else {
        StorageKey::from_raw(format!(
            "tenants/{}/users/{}/{suffix}",
            ctx.tenant_id, ctx.user_id
        ))
    }
}

/// Encode a Memory (frontmatter + body) into the on-disk YAML+body
/// shape used for the `memories/<id>.md` file.
fn render_memory_yaml(fm: &MemoryFrontmatter, content: &str) -> Result<String, EngineError> {
    let yaml = serde_yml::to_string(fm)
        .map_err(|e| EngineError::Yaml(Box::new(e)))?;
    Ok(combine_frontmatter(yaml.trim(), content))
}

/// Decode a `memories/<id>.md` file body into a `(MemoryFrontmatter,
/// String)`.
fn parse_memory_file(bytes: &[u8]) -> Result<(MemoryFrontmatter, String), EngineError> {
    let content = std::str::from_utf8(bytes)
        .map_err(|e| EngineError::Parse(format!("non-utf8 memory bytes: {e}")))?;
    let split = split_frontmatter_normalized(content)
        .map_err(|e| EngineError::Parse(format!("split frontmatter: {e}")))?;
    let fm: MemoryFrontmatter = serde_yml::from_str(&split.yaml)
        .map_err(|e| EngineError::Yaml(Box::new(e)))?;
    Ok((fm, split.body))
}

/// Convert `Vec<f32>` to little-endian bytes for the `.vec` sidecar.
fn embedding_to_bytes(vec: &[f32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(vec.len() * 4);
    for v in vec {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf
}

/// Reverse of [`embedding_to_bytes`]. Returns `Err` if the buffer
/// length isn't a multiple of 4 OR doesn't match `expected_dims`.
fn bytes_to_embedding(bytes: &[u8], expected_dims: usize) -> Result<Vec<f32>, EngineError> {
    if bytes.len() % 4 != 0 {
        return Err(EngineError::Parse(format!(
            "embedding bytes length {} not a multiple of 4",
            bytes.len()
        )));
    }
    let actual_dims = bytes.len() / 4;
    if actual_dims != expected_dims {
        return Err(EngineError::Parse(format!(
            "embedding dim mismatch: stored={actual_dims} expected={expected_dims}"
        )));
    }
    let mut out = Vec::with_capacity(actual_dims);
    for chunk in bytes.chunks_exact(4) {
        let arr: [u8; 4] = chunk.try_into().unwrap();
        out.push(f32::from_le_bytes(arr));
    }
    Ok(out)
}

/// Insert a new memory. Generates a `MemoryId`, embeds the content
/// via the caller's `Embedder`, persists the frontmatter+body file
/// AND the `.vec` sidecar AND the vector index entry. Returns the
/// fully-populated `Memory` (including the embedding).
///
/// Atomicity note: the three writes (md file, vec file, vector index
/// insert) are NOT transactional. Failure midway can leave a partial
/// state. Phase E ships the simple sequential write; future cycles
/// may add a recovery sweep that prunes orphan vec files / index
/// entries.
#[allow(clippy::too_many_arguments)] // 8 args is fundamental to the operation
pub async fn insert(
    ctx: &Context,
    storage: &dyn Storage,
    embedder: &dyn Embedder,
    vector_index: &dyn VectorIndex,
    id: MemoryId,
    description: impl Into<String>,
    content: impl Into<String>,
    now: DateTime<Utc>,
) -> Result<Memory, EngineError> {
    let description = description.into();
    let content = content.into();
    // 1. Embed.
    let texts = vec![content.clone()];
    let mut embeddings = embedder.embed(ctx, &texts).await?;
    let embedding = embeddings
        .pop()
        .ok_or_else(|| EngineError::Parse("embedder returned zero vectors".into()))?;
    // 2. Build frontmatter + persist the .md file.
    let fm = MemoryFrontmatter::new(id.clone(), description, now);
    let yaml = render_memory_yaml(&fm, &content)?;
    let md_key = StorageKey::memory(ctx, id.as_str());
    storage.put(&md_key, Bytes::from(yaml)).await?;
    // 3. Persist the .vec sidecar.
    let vec_bytes = embedding_to_bytes(&embedding);
    storage
        .put(&vec_key(ctx, &id), Bytes::from(vec_bytes))
        .await?;
    // 4. Insert into the vector index.
    vector_index.insert(ctx, &id, &embedding).await?;
    Ok(Memory::new(fm, content).with_embedding(embedding))
}

/// Load a memory by id. Returns `Ok(None)` if absent. The returned
/// `Memory` has `embedding: None` UNLESS the caller also reads the
/// `.vec` sidecar — use [`get_by_id_with_embedding`] for that path.
pub async fn get_by_id(
    ctx: &Context,
    storage: &dyn Storage,
    id: &MemoryId,
) -> Result<Option<Memory>, EngineError> {
    let key = StorageKey::memory(ctx, id.as_str());
    let bytes = match storage.get(&key).await? {
        Some(b) => b,
        None => return Ok(None),
    };
    let (fm, content) = parse_memory_file(&bytes)?;
    Ok(Some(Memory::new(fm, content)))
}

/// Load a memory by id INCLUDING its embedding (from the `.vec`
/// sidecar). Returns `Ok(None)` if the .md file is absent. Errors
/// if the .md exists but the .vec is missing or malformed.
pub async fn get_by_id_with_embedding(
    ctx: &Context,
    storage: &dyn Storage,
    id: &MemoryId,
    expected_dims: usize,
) -> Result<Option<Memory>, EngineError> {
    let Some(mut mem) = get_by_id(ctx, storage, id).await? else {
        return Ok(None);
    };
    let vec_bytes = storage.get(&vec_key(ctx, id)).await?.ok_or_else(|| {
        EngineError::Parse(format!(
            "memory {id} present but .vec sidecar missing"
        ))
    })?;
    let embedding = bytes_to_embedding(&vec_bytes, expected_dims)?;
    mem.embedding = Some(embedding);
    Ok(Some(mem))
}

/// Semantic search across all memories. Embeds the query (if it's a
/// `Text` variant), runs the vector index search, hydrates the top-k
/// hits into `MemoryRef` shape (id + description + body preview +
/// similarity).
pub async fn search(
    ctx: &Context,
    storage: &dyn Storage,
    embedder: &dyn Embedder,
    vector_index: &dyn VectorIndex,
    query: &MemoryQuery,
    k: usize,
    body_preview_len: usize,
) -> Result<Vec<MemoryRef>, EngineError> {
    let query_vec: Vec<f32> = match query {
        MemoryQuery::Text(s) => {
            let mut v = embedder.embed(ctx, std::slice::from_ref(s)).await?;
            v.pop().ok_or_else(|| {
                EngineError::Parse("embedder returned no vector for query".into())
            })?
        }
        MemoryQuery::Vector(v) => v.clone(),
    };
    let hits: Vec<SearchHit> = vector_index.search(ctx, &query_vec, k).await?;
    let mut out: Vec<MemoryRef> = Vec::with_capacity(hits.len());
    for hit in hits {
        // Load each hit's frontmatter to surface description + body
        // preview. Soft-fail on missing/malformed memories (race
        // between vector index entry and .md file) — log + skip.
        match get_by_id(ctx, storage, &hit.id).await {
            Ok(Some(mem)) => {
                let body_preview = mem
                    .content
                    .chars()
                    .take(body_preview_len)
                    .collect::<String>()
                    .trim()
                    .to_string();
                out.push(MemoryRef {
                    id: hit.id,
                    description: mem.frontmatter.description,
                    body_preview,
                    similarity: hit.similarity,
                });
            }
            Ok(None) => {
                warn!(
                    id = %hit.id,
                    "memory::search: vector index returned id whose .md is missing"
                );
            }
            Err(e) => {
                warn!(
                    id = %hit.id, error = %e,
                    "memory::search: failed to load memory; skipping"
                );
            }
        }
    }
    Ok(out)
}

/// Delete a memory. Removes the .md file, the .vec sidecar, AND
/// tombstones the entry in the vector index. Idempotent — deleting
/// an absent id is `Ok(())`.
///
/// NOTE: `delete` bypasses the user-lesson-immunity guard. It is
/// intended for explicit user-initiated removal (via `loop forget` /
/// MCP tool / etc); auto-prune callers MUST use [`prune`] which
/// enforces immunity.
pub async fn delete(
    ctx: &Context,
    storage: &dyn Storage,
    vector_index: &dyn VectorIndex,
    id: &MemoryId,
) -> Result<(), EngineError> {
    let md_key = StorageKey::memory(ctx, id.as_str());
    let v_key = vec_key(ctx, id);
    storage.delete(&md_key).await?;
    storage.delete(&v_key).await?;
    vector_index.delete(ctx, id).await?;
    Ok(())
}

/// Prune memories matching `predicate`. The engine wraps `predicate`
/// with the user-lesson-immunity guard (D-E9): a memory whose
/// `consumed_by_user_lessons > 0` is ALWAYS skipped, even if the
/// predicate matched. `PruneStats::skipped_user_immune` counts these.
///
/// Predicate runs over `&MemoryFrontmatter` only (not the body or
/// embedding) — cheap to evaluate per memory.
pub async fn prune(
    ctx: &Context,
    storage: &dyn Storage,
    vector_index: &dyn VectorIndex,
    predicate: PrunePredicate,
) -> Result<PruneStats, EngineError> {
    let mut stats = PruneStats {
        examined: 0,
        pruned: 0,
        skipped_user_immune: 0,
    };
    let prefix = StorageKey::memories_prefix(ctx);
    let keys = storage.list(&prefix).await?;
    // Engine-internal: wrap the user predicate with the immunity guard.
    let guarded = guarded_predicate(predicate);

    for key in keys {
        // Skip .vec sidecars; we operate on .md frontmatter files.
        if !key.as_str().ends_with(".md") {
            continue;
        }
        let bytes = match storage.get(&key).await? {
            Some(b) => b,
            None => continue,
        };
        let (fm, _body) = match parse_memory_file(&bytes) {
            Ok(parsed) => parsed,
            Err(e) => {
                warn!(key = %key, error = %e, "prune: skipping unparseable memory");
                continue;
            }
        };
        stats.examined += 1;

        // First: check the unguarded predicate to count what WOULD
        // have matched if not for the user-immunity rule.
        // We need to evaluate the predicate twice (cheap, all
        // in-memory) to distinguish skipped-by-immunity from
        // not-matched.
        //
        // Trick: `guarded` returns true ONLY when predicate AND
        // counter == 0. So:
        //   - guarded(fm) == true: prune.
        //   - guarded(fm) == false AND counter > 0 AND original
        //     predicate would have matched: skipped_user_immune.
        //   - otherwise: not a match.
        //
        // To detect the immunity-skip case without re-running the
        // user predicate, we use the counter directly: if the
        // counter is > 0, the immunity guard is the relevant
        // blocker, so check whether the user predicate WOULD have
        // matched.
        let counter_blocks = fm.consumed_by_user_lessons > 0;
        if guarded(&fm) {
            // Prune: remove md + vec + vector index entry.
            let id = fm.id.clone();
            let md_key = StorageKey::memory(ctx, id.as_str());
            storage.delete(&md_key).await?;
            storage.delete(&vec_key(ctx, &id)).await?;
            vector_index.delete(ctx, &id).await?;
            stats.pruned += 1;
        } else if counter_blocks {
            // The counter is what's blocking. Did the user predicate
            // alone match? We can't tell without re-running it
            // unguarded. Re-derive by constructing an unguarded
            // probe: temporarily zero the counter on a CLONE of the
            // frontmatter and re-evaluate via the guarded predicate.
            let mut probe = fm.clone();
            probe.consumed_by_user_lessons = 0;
            if guarded(&probe) {
                stats.skipped_user_immune += 1;
                warn!(
                    id = %fm.id,
                    cited_by = fm.consumed_by_user_lessons,
                    "prune: skipping user-immune memory"
                );
            }
        }
    }
    Ok(stats)
}

/// Increment `consumed_by_user_lessons` on a memory's frontmatter.
/// Called by future Phase G `lessons::transitions::*` paths when a
/// user-authored lesson cites the memory via
/// `EvidenceRef::Memory(_)`. 5-retry CAS-RMW per Phase A C5 pattern.
///
/// Returns `EngineError::CasContended` on retry exhaustion.
/// Returns `Ok(())` if the memory doesn't exist (best-effort —
/// citation tracking is advisory, not load-bearing for correctness).
pub async fn increment_citation_count(
    ctx: &Context,
    storage: &dyn Storage,
    id: &MemoryId,
) -> Result<(), EngineError> {
    let key = StorageKey::memory(ctx, id.as_str());
    for _attempt in 0..CITATION_CAS_MAX_RETRIES {
        let Some((bytes, version)) = storage.get_with_version(&key).await? else {
            // Memory doesn't exist — citation is best-effort.
            return Ok(());
        };
        let (mut fm, body) = parse_memory_file(&bytes)?;
        fm.consumed_by_user_lessons = fm.consumed_by_user_lessons.saturating_add(1);
        fm.updated_at = Some(now_iso());
        let new_yaml = render_memory_yaml(&fm, &body)?;
        let written = storage
            .put_if_version(&key, Bytes::from(new_yaml), Some(&version))
            .await?;
        if written {
            return Ok(());
        }
    }
    Err(EngineError::CasContended {
        key: key.as_str().to_string(),
        retries: CITATION_CAS_MAX_RETRIES,
    })
}

/// Decrement the citation counter (saturating at 0). Reserved for
/// Phase G `transitions::discard` / `transitions::supersede`. Same
/// CAS pattern as [`increment_citation_count`].
#[allow(dead_code)] // Phase G consumes
pub(crate) async fn decrement_citation_count(
    ctx: &Context,
    storage: &dyn Storage,
    id: &MemoryId,
) -> Result<(), EngineError> {
    let key = StorageKey::memory(ctx, id.as_str());
    for _attempt in 0..CITATION_CAS_MAX_RETRIES {
        let Some((bytes, version)) = storage.get_with_version(&key).await? else {
            return Ok(());
        };
        let (mut fm, body) = parse_memory_file(&bytes)?;
        fm.consumed_by_user_lessons = fm.consumed_by_user_lessons.saturating_sub(1);
        fm.updated_at = Some(now_iso());
        let new_yaml = render_memory_yaml(&fm, &body)?;
        let written = storage
            .put_if_version(&key, Bytes::from(new_yaml), Some(&version))
            .await?;
        if written {
            return Ok(());
        }
    }
    Err(EngineError::CasContended {
        key: key.as_str().to_string(),
        retries: CITATION_CAS_MAX_RETRIES,
    })
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::Context;
    use crate::engine::embedding::MockEmbedder;
    use crate::engine::storage::MemoryStorage;
    use crate::engine::vector::HnswVectorIndex;
    use std::sync::Arc;

    fn ctx() -> Context {
        Context::single_user_local()
    }

    fn unit_vec(dim: usize, axis: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; dim];
        v[axis % dim] = 1.0;
        v
    }

    async fn fresh_setup() -> (
        Arc<dyn Storage>,
        MockEmbedder,
        HnswVectorIndex,
        DateTime<Utc>,
    ) {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let embedder = MockEmbedder::new(4);
        let vector_index = HnswVectorIndex::new(4);
        let now = "2026-05-14T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        (storage, embedder, vector_index, now)
    }

    #[tokio::test]
    async fn insert_persists_md_vec_and_index() {
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-aaaaaaaa");
        let mem = insert(
            &ctx(),
            storage.as_ref(),
            &embedder,
            &vector_index,
            id.clone(),
            "test memory",
            "body content",
            now,
        )
        .await
        .unwrap();
        assert_eq!(mem.frontmatter.id, id);
        assert!(mem.embedding.is_some());

        // .md file present.
        let md_key = StorageKey::memory(&ctx(), id.as_str());
        assert!(storage.get(&md_key).await.unwrap().is_some());

        // .vec file present.
        let v_key = vec_key(&ctx(), &id);
        let vec_bytes = storage.get(&v_key).await.unwrap().unwrap();
        let dims = bytes_to_embedding(&vec_bytes, 4).unwrap();
        assert_eq!(dims, unit_vec(4, 0));
    }

    #[tokio::test]
    async fn get_by_id_round_trips_after_insert() {
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-aaaaaaaa");
        insert(
            &ctx(),
            storage.as_ref(),
            &embedder,
            &vector_index,
            id.clone(),
            "desc",
            "body",
            now,
        )
        .await
        .unwrap();
        let loaded = get_by_id(&ctx(), storage.as_ref(), &id).await.unwrap().unwrap();
        assert_eq!(loaded.frontmatter.id, id);
        assert_eq!(loaded.frontmatter.description, "desc");
        assert_eq!(loaded.content.trim(), "body");
        // get_by_id doesn't load the embedding.
        assert!(loaded.embedding.is_none());
    }

    #[tokio::test]
    async fn get_by_id_with_embedding_includes_vec() {
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-aaaaaaaa");
        insert(
            &ctx(),
            storage.as_ref(),
            &embedder,
            &vector_index,
            id.clone(),
            "desc",
            "body",
            now,
        )
        .await
        .unwrap();
        let loaded = get_by_id_with_embedding(&ctx(), storage.as_ref(), &id, 4)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.embedding.as_deref(), Some(&unit_vec(4, 0)[..]));
    }

    #[tokio::test]
    async fn get_by_id_returns_none_for_missing() {
        let (storage, _, _, _) = fresh_setup().await;
        let r = get_by_id(&ctx(), storage.as_ref(), &MemoryId::new("mem-noexist1"))
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn search_returns_top_k_with_hydrated_refs() {
        let (storage, _, vector_index, now) = fresh_setup().await;
        // Insert 3 memories along different axes.
        let embedder_a = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
        let embedder_b = MockEmbedder::new(4).with_response(vec![unit_vec(4, 1)]);
        let embedder_c = MockEmbedder::new(4).with_response(vec![unit_vec(4, 2)]);
        let a = MemoryId::new("mem-aaaaaaaa");
        let b = MemoryId::new("mem-bbbbbbbb");
        let c = MemoryId::new("mem-cccccccc");
        insert(&ctx(), storage.as_ref(), &embedder_a, &vector_index, a.clone(), "axis 0", "body a", now).await.unwrap();
        insert(&ctx(), storage.as_ref(), &embedder_b, &vector_index, b.clone(), "axis 1", "body b", now).await.unwrap();
        insert(&ctx(), storage.as_ref(), &embedder_c, &vector_index, c.clone(), "axis 2", "body c", now).await.unwrap();

        // Search with a pre-computed query vector aligned with axis 0.
        let q = MemoryQuery::Vector(unit_vec(4, 0));
        let embedder_search = MockEmbedder::new(4); // unused for Vector queries
        let hits = search(&ctx(), storage.as_ref(), &embedder_search, &vector_index, &q, 1, 50).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, a);
        assert_eq!(hits[0].description, "axis 0");
        assert_eq!(hits[0].body_preview, "body a");
        assert!(hits[0].similarity > 0.9);
    }

    #[tokio::test]
    async fn delete_removes_md_vec_and_tombstones_index() {
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-aaaaaaaa");
        insert(&ctx(), storage.as_ref(), &embedder, &vector_index, id.clone(), "x", "y", now).await.unwrap();
        delete(&ctx(), storage.as_ref(), &vector_index, &id).await.unwrap();

        // .md gone.
        assert!(storage.get(&StorageKey::memory(&ctx(), id.as_str())).await.unwrap().is_none());
        // .vec gone.
        assert!(storage.get(&vec_key(&ctx(), &id)).await.unwrap().is_none());
        // Vector index search no longer returns it.
        let hits = vector_index.search(&ctx(), &unit_vec(4, 0), 5).await.unwrap();
        assert!(!hits.iter().any(|h| h.id == id));
    }

    #[tokio::test]
    async fn prune_evicts_matching_memory() {
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder_a = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-prune001");
        insert(&ctx(), storage.as_ref(), &embedder_a, &vector_index, id.clone(), "p", "p body", now).await.unwrap();

        // Predicate that matches everything.
        let pred: PrunePredicate = Box::new(|_fm| true);
        let stats = prune(&ctx(), storage.as_ref(), &vector_index, pred).await.unwrap();
        assert_eq!(stats.examined, 1);
        assert_eq!(stats.pruned, 1);
        assert_eq!(stats.skipped_user_immune, 0);
        assert!(storage.get(&StorageKey::memory(&ctx(), id.as_str())).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn prune_skips_user_immune_memory_and_counts_it() {
        // THE wedge-trust invariant at the memory layer.
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-immune01");
        insert(&ctx(), storage.as_ref(), &embedder, &vector_index, id.clone(), "user-cited", "body", now).await.unwrap();
        // Simulate a user-authored lesson citing this memory.
        increment_citation_count(&ctx(), storage.as_ref(), &id).await.unwrap();
        let loaded = get_by_id(&ctx(), storage.as_ref(), &id).await.unwrap().unwrap();
        assert_eq!(loaded.frontmatter.consumed_by_user_lessons, 1);

        // Predicate that WOULD match everything.
        let pred: PrunePredicate = Box::new(|_fm| true);
        let stats = prune(&ctx(), storage.as_ref(), &vector_index, pred).await.unwrap();
        assert_eq!(stats.examined, 1);
        assert_eq!(stats.pruned, 0, "user-immune memory must NOT be pruned");
        assert_eq!(stats.skipped_user_immune, 1, "skip MUST be counted");
        // .md still present.
        assert!(storage.get(&StorageKey::memory(&ctx(), id.as_str())).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn increment_citation_count_is_idempotent_via_cas() {
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-counter1");
        insert(&ctx(), storage.as_ref(), &embedder, &vector_index, id.clone(), "x", "y", now).await.unwrap();
        // 3 increments should land 3.
        for _ in 0..3 {
            increment_citation_count(&ctx(), storage.as_ref(), &id).await.unwrap();
        }
        let loaded = get_by_id(&ctx(), storage.as_ref(), &id).await.unwrap().unwrap();
        assert_eq!(loaded.frontmatter.consumed_by_user_lessons, 3);
    }

    #[tokio::test]
    async fn increment_citation_count_on_missing_memory_is_noop() {
        let (storage, _, _, _) = fresh_setup().await;
        // No insert — just increment on a non-existent id.
        let r = increment_citation_count(
            &ctx(),
            storage.as_ref(),
            &MemoryId::new("mem-noexist1"),
        )
        .await;
        assert!(r.is_ok(), "increment on missing must be a no-op");
    }

    #[tokio::test]
    async fn decrement_citation_count_saturates_at_zero() {
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-decrmnt1");
        insert(&ctx(), storage.as_ref(), &embedder, &vector_index, id.clone(), "x", "y", now).await.unwrap();
        // Increment twice, decrement five times.
        for _ in 0..2 {
            increment_citation_count(&ctx(), storage.as_ref(), &id).await.unwrap();
        }
        for _ in 0..5 {
            decrement_citation_count(&ctx(), storage.as_ref(), &id).await.unwrap();
        }
        let loaded = get_by_id(&ctx(), storage.as_ref(), &id).await.unwrap().unwrap();
        assert_eq!(loaded.frontmatter.consumed_by_user_lessons, 0, "saturate at 0");
    }

    #[tokio::test]
    async fn embedding_to_bytes_round_trip() {
        let v = vec![0.1_f32, -0.5, 1.0, 0.0];
        let bytes = embedding_to_bytes(&v);
        assert_eq!(bytes.len(), 16);
        let back = bytes_to_embedding(&bytes, 4).unwrap();
        assert_eq!(back, v);
    }

    #[tokio::test]
    async fn bytes_to_embedding_rejects_misaligned_length() {
        let bytes = vec![0u8; 7];
        let r = bytes_to_embedding(&bytes, 1);
        assert!(matches!(r, Err(EngineError::Parse(_))));
    }

    #[tokio::test]
    async fn bytes_to_embedding_rejects_wrong_dimension() {
        let bytes = vec![0u8; 16]; // 4 floats
        let r = bytes_to_embedding(&bytes, 8); // expected 8
        assert!(matches!(r, Err(EngineError::Parse(_))));
    }
}
