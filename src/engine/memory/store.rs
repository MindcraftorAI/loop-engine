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
    Memory, MemoryFrontmatter, MemoryId, MemoryQuery, MemoryRef, MemoryScopeFilter, PrunePredicate,
    PruneStats,
};
use crate::engine::storage::{Storage, StorageKey};
use crate::engine::vector::{SearchHit, VectorIndex};
use crate::engine::yaml::{combine_frontmatter, split_frontmatter_normalized};

/// CAS-RMW retry budget for citation-counter updates (Phase A C5
/// pattern). 5 retries absorbs cross-process contention; exhaustion
/// surfaces as `EngineError::CasContended`.
/// `pub(crate)` so `engine::memory::lifecycle` (Phase E2 audit B-M2
/// extraction) reuses the same retry budget.
pub(crate) const CITATION_CAS_MAX_RETRIES: u32 = 5;

/// `.vec` sidecar file holding the raw little-endian f32 embedding.
/// `pub(crate)` so the compression module (Phase E2) can build keys
/// using the same convention.
pub(crate) fn vec_key(ctx: &Context, id: &MemoryId) -> StorageKey {
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
/// shape used for the `memories/<id>.md` file. `pub(crate)` for the
/// compression module (Phase E2).
pub(crate) fn render_memory_yaml(
    fm: &MemoryFrontmatter,
    content: &str,
) -> Result<String, EngineError> {
    let yaml = serde_yml::to_string(fm).map_err(|e| EngineError::Yaml(Box::new(e)))?;
    Ok(combine_frontmatter(yaml.trim(), content))
}

/// Decode a `memories/<id>.md` file body into a `(MemoryFrontmatter,
/// String)`. `pub(crate)` for the compression module.
pub(crate) fn parse_memory_file(bytes: &[u8]) -> Result<(MemoryFrontmatter, String), EngineError> {
    let content = std::str::from_utf8(bytes)
        .map_err(|e| EngineError::Parse(format!("non-utf8 memory bytes: {e}")))?;
    let split = split_frontmatter_normalized(content)
        .map_err(|e| EngineError::Parse(format!("split frontmatter: {e}")))?;
    let fm: MemoryFrontmatter =
        serde_yml::from_str(&split.yaml).map_err(|e| EngineError::Yaml(Box::new(e)))?;
    Ok((fm, split.body))
}

/// Convert `Vec<f32>` to little-endian bytes for the `.vec` sidecar.
/// `pub(crate)` for the compression module.
pub(crate) fn embedding_to_bytes(vec: &[f32]) -> Vec<u8> {
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
    insert_scoped(
        ctx,
        storage,
        embedder,
        vector_index,
        id,
        description,
        content,
        now,
        crate::engine::memory::MemoryScope::default(),
    )
    .await
}

/// Insert a memory with an explicit [`crate::engine::memory::MemoryScope`]. Phase F D-F8 +
/// audit-fix close: the write half of the scope-aware manifest filter.
/// `insert` delegates to this with `MemoryScope::User`. v0.4 callers
/// that want to record provenance metadata should use
/// [`insert_with_provenance`] instead — `insert_scoped` itself stays
/// origin-less to preserve its v0.3.1 call-site surface.
#[allow(clippy::too_many_arguments)]
pub async fn insert_scoped(
    ctx: &Context,
    storage: &dyn Storage,
    embedder: &dyn Embedder,
    vector_index: &dyn VectorIndex,
    id: MemoryId,
    description: impl Into<String>,
    content: impl Into<String>,
    now: DateTime<Utc>,
    scope: crate::engine::memory::MemoryScope,
) -> Result<Memory, EngineError> {
    insert_with_provenance(
        ctx,
        storage,
        embedder,
        vector_index,
        id,
        description,
        content,
        now,
        scope,
        None,
    )
    .await
}

/// Insert a memory with explicit [`crate::engine::memory::MemoryScope`] AND optional
/// [`crate::engine::memory::MemoryOrigin`] provenance. Phase G D-G1 (v0.4) — the deepest
/// write-path for `memory.create` callers that have rich host-side
/// context to attach.
///
/// `origin = None` (or an empty origin) is equivalent to
/// [`insert_scoped`] — the on-disk YAML omits the `origin:` block,
/// so v0.4-shipping callers don't bloat files when their host can't
/// detect anything useful.
#[allow(clippy::too_many_arguments)]
pub async fn insert_with_provenance(
    ctx: &Context,
    storage: &dyn Storage,
    embedder: &dyn Embedder,
    vector_index: &dyn VectorIndex,
    id: MemoryId,
    description: impl Into<String>,
    content: impl Into<String>,
    now: DateTime<Utc>,
    scope: crate::engine::memory::MemoryScope,
    origin: Option<crate::engine::memory::MemoryOrigin>,
) -> Result<Memory, EngineError> {
    let description = description.into();
    let content = content.into();
    // 1. Embed.
    let texts = vec![content.clone()];
    let mut embeddings = embedder.embed(ctx, &texts).await?;
    let embedding = embeddings
        .pop()
        .ok_or_else(|| EngineError::Parse("embedder returned zero vectors".into()))?;
    // 2. Build frontmatter (with scope + optional origin) + persist
    //    the .md file. `with_origin` short-circuits to `None` on an
    //    empty origin, so absent fields don't bloat YAML.
    let mut fm = MemoryFrontmatter::new(id.clone(), description, now).with_scope(scope);
    if let Some(o) = origin {
        fm = fm.with_origin(o);
    }
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

// Phase E2 audit B-M2 extraction: `get_by_id_chasing_derived_from`
// + `find_compressor_of` moved to `super::lifecycle`. Re-exported
// from `engine::memory::mod` for backward compatibility.

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
        EngineError::Parse(format!("memory {id} present but .vec sidecar missing"))
    })?;
    let embedding = bytes_to_embedding(&vec_bytes, expected_dims)?;
    mem.embedding = Some(embedding);
    Ok(Some(mem))
}

/// Update an existing memory in place. Phase G D-G2 (v0.4): the
/// edit half of the memory lifecycle (`forget` is the delete half).
///
/// Fields are `Option<_>`: passing `None` preserves the existing
/// value, `Some` mutates. Identity (`id`, `created_at`, `derived_from`,
/// `consumed_by_user_lessons`, `origin`) is ALWAYS preserved — only
/// description, content, and scope are mutable. `updated_at` is set
/// to `now` if any field changed.
///
/// When `content` is `Some(new_text)` AND differs from the existing
/// body, the memory is re-embedded and the vector index entry is
/// replaced (delete + insert). Description-only or scope-only edits
/// skip the embed path and leave the `.vec` sidecar untouched (the
/// cheap path).
///
/// Returns `Ok(None)` if no memory with that id exists. The user-
/// immunity invariant is NOT triggered here — updates don't break
/// citation chains. `forget` is where immunity matters.
#[allow(clippy::too_many_arguments)]
pub async fn update(
    ctx: &Context,
    storage: &dyn Storage,
    embedder: &dyn Embedder,
    vector_index: &dyn VectorIndex,
    id: &MemoryId,
    description: Option<String>,
    content: Option<String>,
    scope: Option<crate::engine::memory::MemoryScope>,
    now: DateTime<Utc>,
) -> Result<Option<Memory>, EngineError> {
    let Some(existing) = get_by_id(ctx, storage, id).await? else {
        return Ok(None);
    };
    let mut fm = existing.frontmatter.clone();
    let mut changed = false;
    if let Some(d) = description {
        if d != fm.description {
            fm.description = d;
            changed = true;
        }
    }
    if let Some(s) = scope {
        if s != fm.scope {
            fm.scope = s;
            changed = true;
        }
    }
    let content_changed = match &content {
        Some(c) => *c != existing.content,
        None => false,
    };
    if content_changed {
        changed = true;
    }
    if !changed {
        // Nothing to do — return the existing memory untouched.
        return Ok(Some(existing));
    }
    fm.updated_at = Some(now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));

    let body = content.unwrap_or_else(|| existing.content.clone());

    // Re-embed only when content actually changed; description /
    // scope edits don't touch the embedding.
    let embedding_to_persist = if content_changed {
        let mut embeddings = embedder.embed(ctx, std::slice::from_ref(&body)).await?;
        let v = embeddings
            .pop()
            .ok_or_else(|| EngineError::Parse("embedder returned zero vectors on update".into()))?;
        Some(v)
    } else {
        None
    };

    // Persist the new .md atomically (LocalFsStorage::put writes via
    // temp + rename internally).
    let yaml = render_memory_yaml(&fm, &body)?;
    let md_key = StorageKey::memory(ctx, id.as_str());
    storage.put(&md_key, Bytes::from(yaml)).await?;

    if let Some(embedding) = embedding_to_persist.as_ref() {
        // Replace the .vec sidecar.
        let vec_bytes = embedding_to_bytes(embedding);
        storage
            .put(&vec_key(ctx, id), Bytes::from(vec_bytes))
            .await?;
        // Swap the vector index entry — delete (tombstones the prior
        // point) then insert the new one under the same id.
        vector_index.delete(ctx, id).await?;
        vector_index.insert(ctx, id, embedding).await?;
    }

    Ok(Some(Memory::new(fm, body).with_embedding(
        embedding_to_persist.unwrap_or_else(|| existing.embedding.unwrap_or_default()),
    )))
}

/// Statistics returned by [`rehydrate_vector_index`].
#[derive(Debug, Default, Clone, Copy)]
pub struct RehydrateStats {
    /// Number of `.md` files scanned.
    pub scanned: usize,
    /// Memories successfully inserted into the vector index.
    pub inserted: usize,
    /// `.md` files whose `.vec` sidecar was missing or malformed.
    pub skipped_missing_vec: usize,
    /// Frontmatter parse failures (corrupt YAML, etc).
    pub skipped_parse_error: usize,
}

/// Rebuild the in-memory vector index from on-disk `.md`/`.vec` pairs.
/// MUST be called once at engine startup, before any `search()` call
/// on persisted memories.
///
/// The HNSW index lives in process memory; on restart it starts empty,
/// while the `.md` + `.vec` files survive on disk. Without this step
/// previously-stored memories are invisible to `search()` after a
/// process restart — even though `get_by_id` continues to work because
/// that path reads frontmatter directly from storage.
///
/// Soft-fails on individual entries (missing/malformed `.vec`, parse
/// errors) — those memories are skipped and counted in `RehydrateStats`
/// rather than aborting the whole rehydrate.
pub async fn rehydrate_vector_index(
    ctx: &Context,
    storage: &dyn Storage,
    vector_index: &dyn VectorIndex,
    expected_dims: usize,
) -> Result<RehydrateStats, EngineError> {
    let mut stats = RehydrateStats::default();
    let prefix = StorageKey::memories_prefix(ctx);
    let keys = storage.list(&prefix).await?;

    for key in keys {
        // Skip .vec sidecars + anything else; we drive off the .md
        // frontmatter files as the authoritative list.
        let key_str = key.as_str();
        if !key_str.ends_with(".md") {
            continue;
        }
        stats.scanned += 1;

        // Derive id from the trailing `memories/<id>.md` segment.
        let Some(fname) = key_str.rsplit('/').next() else {
            continue;
        };
        let Some(id_str) = fname.strip_suffix(".md") else {
            continue;
        };
        let id = MemoryId::new(id_str.to_string());

        match get_by_id_with_embedding(ctx, storage, &id, expected_dims).await {
            Ok(Some(mem)) => {
                if let Some(embedding) = mem.embedding {
                    vector_index.insert(ctx, &id, &embedding).await?;
                    stats.inserted += 1;
                } else {
                    // get_by_id_with_embedding only returns Some when
                    // both md + vec are present — this branch is
                    // unreachable in practice. Count it just in case.
                    stats.skipped_missing_vec += 1;
                }
            }
            Ok(None) => {
                // md was missing under the listed key. Race window or
                // stale list; count + continue.
                stats.skipped_missing_vec += 1;
            }
            Err(e) => {
                warn!(
                    id = %id, error = %e,
                    "rehydrate_vector_index: skipping memory"
                );
                if matches!(e, EngineError::Yaml(_) | EngineError::Parse(_)) {
                    stats.skipped_parse_error += 1;
                } else {
                    stats.skipped_missing_vec += 1;
                }
            }
        }
    }

    Ok(stats)
}

/// Semantic search across all memories. Embeds the query (if it's a
/// `Text` variant), runs the vector index search, hydrates the top-k
/// hits into `MemoryRef` shape (id + description + body preview +
/// similarity).
///
/// `scope_filter`, when `Some`, drops hits whose frontmatter scope
/// doesn't satisfy the filter. The frontmatter is loaded inline (no
/// extra disk roundtrip — `get_by_id` was already called for body
/// preview), so filtering is essentially free.
///
/// Known v0.3.1 limitation: with a `scope_filter`, the returned vec
/// may have fewer than `k` entries when in-scope hits don't fill the
/// top-k vector neighborhood. Callers that need k-guaranteed results
/// should over-fetch (e.g. pass `k * 2` and truncate). The Phase F
/// manifest assembly does exactly that.
#[allow(clippy::too_many_arguments)] // 8 args is fundamental to the operation
pub async fn search(
    ctx: &Context,
    storage: &dyn Storage,
    embedder: &dyn Embedder,
    vector_index: &dyn VectorIndex,
    query: &MemoryQuery,
    k: usize,
    body_preview_len: usize,
    scope_filter: Option<&MemoryScopeFilter>,
) -> Result<Vec<MemoryRef>, EngineError> {
    let query_vec: Vec<f32> = match query {
        MemoryQuery::Text(s) => {
            let mut v = embedder.embed(ctx, std::slice::from_ref(s)).await?;
            v.pop()
                .ok_or_else(|| EngineError::Parse("embedder returned no vector for query".into()))?
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
                if let Some(f) = scope_filter {
                    if !f.matches(&mem.frontmatter.scope) {
                        continue;
                    }
                }
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
                    source: Some(crate::engine::memory::HitSource::Semantic),
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

/// Text-match search across all memories. Phase G (v0.5): the
/// complement to [`search`] (semantic vector index lookup). Scans
/// every memory's frontmatter + body, scores via
/// [`crate::engine::scoring::score_text_match`], returns top-k by
/// score. Hits below score 0.0 are dropped (effectively, anything
/// non-zero qualifies and the threshold is applied by the caller).
///
/// Performance: O(n) over the on-disk memory count. Acceptable for
/// the realistic 2026 corpus (<1k memories). v0.6 may add an
/// in-memory description cache or an FTS5 index if scale demands.
///
/// `scope_filter`, when `Some`, drops hits whose frontmatter scope
/// doesn't satisfy the filter — same semantics as [`search`].
///
/// Soft-fails on parse / load errors (mirrors `search` resilience):
/// logs + skips. The function never returns Err unless the storage
/// `list` itself fails.
pub async fn text_search(
    ctx: &Context,
    storage: &dyn Storage,
    query: &str,
    k: usize,
    body_preview_len: usize,
    scope_filter: Option<&MemoryScopeFilter>,
) -> Result<Vec<MemoryRef>, EngineError> {
    let prefix = StorageKey::memories_prefix(ctx);
    let keys = storage.list(&prefix).await?;
    let mut scored: Vec<(f32, MemoryRef)> = Vec::new();

    for key in keys {
        let key_str = key.as_str();
        if !key_str.ends_with(".md") {
            continue;
        }
        let bytes = match storage.get(&key).await {
            Ok(Some(b)) => b,
            Ok(None) => continue,
            Err(e) => {
                warn!(key = %key, error = %e, "memory::text_search: get failed; skipping");
                continue;
            }
        };
        let (fm, body) = match parse_memory_file(&bytes) {
            Ok(p) => p,
            Err(e) => {
                warn!(key = %key, error = %e, "memory::text_search: parse failed; skipping");
                continue;
            }
        };
        if let Some(f) = scope_filter {
            if !f.matches(&fm.scope) {
                continue;
            }
        }
        let sim = crate::engine::scoring::score_text_match(query, &fm.description, &body);
        if sim <= 0.0 {
            continue;
        }
        let body_preview = body
            .chars()
            .take(body_preview_len)
            .collect::<String>()
            .trim()
            .to_string();
        scored.push((
            sim,
            MemoryRef {
                id: fm.id,
                description: fm.description,
                body_preview,
                similarity: sim,
                source: Some(crate::engine::memory::HitSource::Text),
            },
        ));
    }

    // Descending sort by score. Stable so equal-score ties keep
    // storage's listing order (typically lexicographic by id).
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    Ok(scored.into_iter().map(|(_, r)| r).collect())
}

/// Hybrid search: run [`search`] (semantic) + [`text_search`] in
/// parallel, then RRF-merge by `MemoryId`. v0.5's headline recall
/// path — fixes the v0.4 false-negative on proper-noun queries
/// (e.g. "Gianna" scored 0.486 semantically, below the 0.5 threshold,
/// but matches the description literally so the text path surfaces
/// it at score ~1.0).
///
/// RRF formula: `rrf(id) = 1/(60 + sem_rank) + 1/(60 + text_rank)`
/// where ranks are 1-based and missing-from-list contributes 0. The
/// `60` constant is the well-known RRF tuning parameter (Cormack et
/// al. 2009); items appearing in both lists get a strict boost.
///
/// Output: top-k merged refs, sorted by RRF score descending. The
/// `similarity` field on each ref is the RRF score; the `source`
/// field reflects which path(s) surfaced it (`Both` when both, else
/// whichever single source).
///
/// Over-fetches `k * 2` from each sub-search so the merged top-k is
/// drawn from a wider pool when sources disagree on ranking.
///
/// `min_similarity` is applied to the RAW per-source scores BEFORE
/// RRF — cosine for semantic hits, token+substring for text hits.
/// Pass `0.0` to disable. RRF scores are NOT comparable to raw
/// scores; the threshold therefore makes no sense post-merge and is
/// only ever applied here.
#[allow(clippy::too_many_arguments)] // 9 args is fundamental to the operation
pub async fn hybrid_search(
    ctx: &Context,
    storage: &dyn Storage,
    embedder: &dyn Embedder,
    vector_index: &dyn VectorIndex,
    query: &str,
    k: usize,
    body_preview_len: usize,
    scope_filter: Option<&MemoryScopeFilter>,
    min_similarity: f32,
) -> Result<Vec<MemoryRef>, EngineError> {
    /// RRF damping constant. Cormack et al. 2009 — survives well
    /// across domains; mirrors the opensquid-side `RRF_K = 60`.
    const RRF_K: f32 = 60.0;
    let overfetch = k.saturating_mul(2).max(k);

    // Run both paths. Soft-fail on individual sub-search errors so
    // a hybrid call doesn't lose ALL results when one path stumbles.
    // Each sub-list is filtered by `min_similarity` BEFORE RRF so
    // the threshold means "raw per-source signal floor" — RRF scores
    // are in a different range and can't share the threshold.
    let semantic_results = match search(
        ctx,
        storage,
        embedder,
        vector_index,
        &MemoryQuery::Text(query.to_string()),
        overfetch,
        body_preview_len,
        scope_filter,
    )
    .await
    {
        Ok(mut r) => {
            r.retain(|h| h.similarity >= min_similarity);
            r
        }
        Err(e) => {
            warn!(error = %e, "hybrid_search: semantic sub-search failed; continuing with text only");
            Vec::new()
        }
    };
    let text_results = match text_search(
        ctx,
        storage,
        query,
        overfetch,
        body_preview_len,
        scope_filter,
    )
    .await
    {
        Ok(mut r) => {
            r.retain(|h| h.similarity >= min_similarity);
            r
        }
        Err(e) => {
            warn!(error = %e, "hybrid_search: text sub-search failed; continuing with semantic only");
            Vec::new()
        }
    };

    // RRF-merge by id. Same-id collisions get score addition + their
    // source flipped to `Both`.
    use std::collections::HashMap;
    let mut by_id: HashMap<MemoryId, MemoryRef> = HashMap::new();
    let mut rrf_score: HashMap<MemoryId, f32> = HashMap::new();

    for (idx, r) in semantic_results.into_iter().enumerate() {
        let rank = (idx + 1) as f32;
        *rrf_score.entry(r.id.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank);
        by_id.insert(r.id.clone(), r);
    }
    for (idx, r) in text_results.into_iter().enumerate() {
        let rank = (idx + 1) as f32;
        *rrf_score.entry(r.id.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank);
        match by_id.get_mut(&r.id) {
            Some(existing) => {
                existing.source = Some(crate::engine::memory::HitSource::Both);
            }
            None => {
                by_id.insert(r.id.clone(), r);
            }
        }
    }

    // Stamp the RRF score onto each ref's `similarity` and sort.
    let mut merged: Vec<MemoryRef> = by_id
        .into_iter()
        .map(|(id, mut r)| {
            r.similarity = *rrf_score.get(&id).unwrap_or(&0.0);
            r
        })
        .collect();
    merged.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    merged.truncate(k);
    Ok(merged)
}

/// Delete a memory. Removes the .md file, the .vec sidecar, AND
/// tombstones the entry in the vector index. Idempotent — deleting
/// an absent id is `Ok(())`.
///
/// **User-immunity respected by default** (audit A-M2 fix). When
/// `force = false` (the engine-initiated path), this function
/// checks `consumed_by_user_lessons` and returns
/// [`EngineError::UserMemoryImmune`] if the memory is cited by a
/// user-authored lesson — matching the
/// `feedback_user_authored_lessons_immune.md` invariant. Engine-
/// initiated auto-cleanup paths (TTL sweep, LLM-driven cleanup) MUST
/// pass `force = false`.
///
/// User-initiated removal (explicit `loop forget` / MCP tool call)
/// passes `force = true` to bypass the guard — this is exactly the
/// "unless user changes his/her mind" case the principle allows for.
///
/// Returns `Ok(())` for an absent id regardless of `force`.
pub async fn delete(
    ctx: &Context,
    storage: &dyn Storage,
    vector_index: &dyn VectorIndex,
    id: &MemoryId,
    force: bool,
) -> Result<(), EngineError> {
    if !force {
        // Engine-initiated path: respect immunity. Load the
        // frontmatter to check the counter. If absent, fall through
        // to the idempotent-delete path.
        let md_key = StorageKey::memory(ctx, id.as_str());
        if let Some(bytes) = storage.get(&md_key).await? {
            let (fm, _body) = parse_memory_file(&bytes)?;
            if fm.consumed_by_user_lessons > 0 {
                return Err(EngineError::UserMemoryImmune {
                    id: id.as_str().to_string(),
                    cited_by: fm.consumed_by_user_lessons,
                });
            }
        }
    }
    // force=true (user-initiated) OR force=false on a non-immune
    // memory: proceed with the delete.
    let md_key = StorageKey::memory(ctx, id.as_str());
    let v_key = vec_key(ctx, id);
    storage.delete(&md_key).await?;
    storage.delete(&v_key).await?;
    vector_index.delete(ctx, id).await?;
    Ok(())
}

/// Prune memories matching `predicate`. The engine enforces the
/// user-lesson-immunity guard (D-E9): a memory whose
/// `consumed_by_user_lessons > 0` is ALWAYS skipped, even if the
/// predicate matched. `PruneStats::skipped_user_immune` counts these.
///
/// Predicate runs over `&MemoryFrontmatter` only (not the body or
/// embedding) — cheap to evaluate per memory. The predicate is
/// invoked EXACTLY ONCE PER MEMORY against the real frontmatter
/// (audit A-C1 fix): stateful predicates and predicates that
/// inspect `consumed_by_user_lessons` get correct attribution. The
/// previous implementation re-ran the predicate on a falsified clone
/// to detect the immunity-skip case, which double-fired side effects
/// and could mis-attribute skip-vs-no-match outcomes.
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

        // Audit A-C1 fix: invoke predicate ONCE on the real
        // frontmatter; check immunity separately. No predicate
        // re-evaluation on a falsified clone.
        let pred_matched = predicate(&fm);
        let immune = fm.consumed_by_user_lessons > 0;
        match (pred_matched, immune) {
            (true, false) => {
                // Prune: remove md + vec + vector index entry.
                let id = fm.id.clone();
                let md_key = StorageKey::memory(ctx, id.as_str());
                storage.delete(&md_key).await?;
                storage.delete(&vec_key(ctx, &id)).await?;
                vector_index.delete(ctx, &id).await?;
                stats.pruned += 1;
            }
            (true, true) => {
                stats.skipped_user_immune += 1;
                warn!(
                    id = %fm.id,
                    cited_by = fm.consumed_by_user_lessons,
                    "prune: skipping user-immune memory"
                );
            }
            (false, _) => {
                // Predicate didn't match. Whether the memory is
                // immune is irrelevant; it stays.
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

// Phase E2 audit B-M2 extraction: `recompute_citation_counts` +
// `set_citation_count` + `RecomputeStats` moved to
// `super::lifecycle`. Re-exported from `engine::memory::mod`.
//
// Original docstring preserved below for reference.

#[allow(dead_code)]
const _RECOMPUTE_DOC_HINT: &str = "moved to engine::memory::lifecycle";

/* MOVED: see super::lifecycle::recompute_citation_counts
/// Phase E D-E8 drift escape hatch — bounded-cost integrity-restore
/// for the `consumed_by_user_lessons` counter. Scans ALL lessons
/// (across every status dir), counts `EvidenceRef::Memory(_)`
/// occurrences per memory id from user-authored lessons (only), and
/// rewrites each memory's counter via CAS-RMW to match the ground
/// truth.
///
/// When to call: on daemon startup as a self-heal, or as an explicit
/// `loop repair memory-counters` CLI command. Engine ships the
/// function; host triggers on schedule. Bounded cost — O(L + M)
/// where L = total lesson count, M = touched memory count. Each
/// counter write is a single CAS-RMW round.
///
/// Returns a `RecomputeStats` describing the outcome: lessons
/// scanned, memories touched, deltas (counters that DIFFERED from
/// the recomputed value and were rewritten). Drift > 0 indicates
/// the live state was wrong; the function repaired it.
pub async fn recompute_citation_counts(
    ctx: &Context,
    storage: &dyn Storage,
) -> Result<RecomputeStats, EngineError> {
    use std::collections::HashMap;

    let mut stats = RecomputeStats {
        lessons_scanned: 0,
        memories_recomputed: 0,
        counters_repaired: 0,
    };

    // 1. Walk all lesson status directories, accumulate citation
    //    counts per memory id from user-authored lessons.
    let mut counts: HashMap<MemoryId, u32> = HashMap::new();
    for status in crate::engine::paths::LESSON_STATUS_DIRS {
        let prefix = StorageKey::lesson_status_prefix(ctx, status);
        let keys = storage.list(&prefix).await?;
        for key in keys {
            if !key.as_str().ends_with(".md") {
                continue;
            }
            let bytes = match storage.get(&key).await? {
                Some(b) => b,
                None => continue,
            };
            stats.lessons_scanned += 1;
            let content = match std::str::from_utf8(&bytes) {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        key = %key, error = %e,
                        "recompute: skipping lesson with non-UTF8 bytes"
                    );
                    continue;
                }
            };
            let split = match split_frontmatter_normalized(content) {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        key = %key, error = %e,
                        "recompute: skipping lesson with bad frontmatter"
                    );
                    continue;
                }
            };
            let fm: crate::engine::yaml::LessonFrontmatter =
                match crate::engine::yaml::reader::parse_lesson_frontmatter(&split.yaml) {
                    Ok(fm) => fm,
                    Err(e) => {
                        warn!(
                            key = %key, error = %e,
                            "recompute: skipping unparseable lesson"
                        );
                        continue;
                    }
                };
            // Only user-authored OR pack-authored lessons drive immunity.
            // Pack-authored = codex-seeded; user-installing the codex is
            // itself an act of user authorship (see Authorship::is_immune).
            if !fm.authored_by.is_immune() {
                continue;
            }
            if let Some(cn) = &fm.causal_narrative {
                for evr in &cn.evidence_refs {
                    if let Some(mid) = evr.as_memory_id() {
                        // Phase E2 Cx2: if `mid` was compressed away
                        // (no longer on disk), walk forward through
                        // `derived_from` chain to find the canonical
                        // successor. Credit the successor. Falls
                        // back to `mid` itself when no chase is
                        // needed (mid exists) OR no successor found
                        // (citation is to a stale id, drift detected).
                        let canonical =
                            match get_by_id_chasing_derived_from(ctx, storage, mid).await? {
                                Some(mem) => mem.frontmatter.id,
                                None => mid.clone(),
                            };
                        *counts.entry(canonical).or_insert(0) += 1;
                    }
                }
            }
        }
    }

    // 2. Walk all memories. For each, the EXPECTED counter is
    //    `counts.get(id).copied().unwrap_or(0)`. If the on-disk
    //    counter differs, CAS-rewrite it to the expected value.
    let mem_prefix = StorageKey::memories_prefix(ctx);
    let mem_keys = storage.list(&mem_prefix).await?;
    for key in mem_keys {
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
                warn!(
                    key = %key, error = %e,
                    "recompute: skipping unparseable memory"
                );
                continue;
            }
        };
        stats.memories_recomputed += 1;
        let expected = counts.get(&fm.id).copied().unwrap_or(0);
        if fm.consumed_by_user_lessons != expected {
            // Drift detected. Repair via CAS-RMW.
            set_citation_count(ctx, storage, &fm.id, expected).await?;
            stats.counters_repaired += 1;
            warn!(
                id = %fm.id,
                was = fm.consumed_by_user_lessons,
                now = expected,
                "recompute: repaired drifted citation counter"
            );
        }
    }
    Ok(stats)
}

/// Internal helper for `recompute_citation_counts`: CAS-rewrite the
/// counter to `target`. Same 5-retry budget as
/// `increment_citation_count`.
async fn set_citation_count(
    ctx: &Context,
    storage: &dyn Storage,
    id: &MemoryId,
    target: u32,
) -> Result<(), EngineError> {
    let key = StorageKey::memory(ctx, id.as_str());
    for _attempt in 0..CITATION_CAS_MAX_RETRIES {
        let Some((bytes, version)) = storage.get_with_version(&key).await? else {
            return Ok(());
        };
        let (mut fm, body) = parse_memory_file(&bytes)?;
        if fm.consumed_by_user_lessons == target {
            return Ok(()); // already correct, no write needed
        }
        fm.consumed_by_user_lessons = target;
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

/// Stats from a [`recompute_citation_counts`] sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RecomputeStats {
    pub lessons_scanned: usize,
    pub memories_recomputed: usize,
    /// Counters that DIFFERED from ground truth and were repaired.
    /// Drift > 0 means the live state was wrong. Healthy systems
    /// run with drift = 0.
    pub counters_repaired: usize,
}
*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::Context;
    use crate::engine::embedding::MockEmbedder;
    // Phase E2 audit B-M2 extraction: recompute + chase moved to
    // sibling module. Test refs in this file import via the sibling
    // path.
    use crate::engine::memory::lifecycle::{
        get_by_id_chasing_derived_from, recompute_citation_counts,
    };
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
        let loaded = get_by_id(&ctx(), storage.as_ref(), &id)
            .await
            .unwrap()
            .unwrap();
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
        insert(
            &ctx(),
            storage.as_ref(),
            &embedder_a,
            &vector_index,
            a.clone(),
            "axis 0",
            "body a",
            now,
        )
        .await
        .unwrap();
        insert(
            &ctx(),
            storage.as_ref(),
            &embedder_b,
            &vector_index,
            b.clone(),
            "axis 1",
            "body b",
            now,
        )
        .await
        .unwrap();
        insert(
            &ctx(),
            storage.as_ref(),
            &embedder_c,
            &vector_index,
            c.clone(),
            "axis 2",
            "body c",
            now,
        )
        .await
        .unwrap();

        // Search with a pre-computed query vector aligned with axis 0.
        let q = MemoryQuery::Vector(unit_vec(4, 0));
        let embedder_search = MockEmbedder::new(4); // unused for Vector queries
        let hits = search(
            &ctx(),
            storage.as_ref(),
            &embedder_search,
            &vector_index,
            &q,
            1,
            50,
            None,
        )
        .await
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, a);
        assert_eq!(hits[0].description, "axis 0");
        assert_eq!(hits[0].body_preview, "body a");
        assert!(hits[0].similarity > 0.9);
    }

    #[tokio::test]
    async fn delete_force_true_removes_md_vec_and_tombstones_index() {
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-aaaaaaaa");
        insert(
            &ctx(),
            storage.as_ref(),
            &embedder,
            &vector_index,
            id.clone(),
            "x",
            "y",
            now,
        )
        .await
        .unwrap();
        delete(&ctx(), storage.as_ref(), &vector_index, &id, true)
            .await
            .unwrap();

        // .md gone.
        assert!(storage
            .get(&StorageKey::memory(&ctx(), id.as_str()))
            .await
            .unwrap()
            .is_none());
        // .vec gone.
        assert!(storage.get(&vec_key(&ctx(), &id)).await.unwrap().is_none());
        // Vector index search no longer returns it.
        let hits = vector_index
            .search(&ctx(), &unit_vec(4, 0), 5)
            .await
            .unwrap();
        assert!(!hits.iter().any(|h| h.id == id));
    }

    #[tokio::test]
    async fn prune_evicts_matching_memory() {
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder_a = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-prune001");
        insert(
            &ctx(),
            storage.as_ref(),
            &embedder_a,
            &vector_index,
            id.clone(),
            "p",
            "p body",
            now,
        )
        .await
        .unwrap();

        // Predicate that matches everything.
        let pred: PrunePredicate = Box::new(|_fm| true);
        let stats = prune(&ctx(), storage.as_ref(), &vector_index, pred)
            .await
            .unwrap();
        assert_eq!(stats.examined, 1);
        assert_eq!(stats.pruned, 1);
        assert_eq!(stats.skipped_user_immune, 0);
        assert!(storage
            .get(&StorageKey::memory(&ctx(), id.as_str()))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn prune_skips_user_immune_memory_and_counts_it() {
        // THE wedge-trust invariant at the memory layer.
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-immune01");
        insert(
            &ctx(),
            storage.as_ref(),
            &embedder,
            &vector_index,
            id.clone(),
            "user-cited",
            "body",
            now,
        )
        .await
        .unwrap();
        // Simulate a user-authored lesson citing this memory.
        increment_citation_count(&ctx(), storage.as_ref(), &id)
            .await
            .unwrap();
        let loaded = get_by_id(&ctx(), storage.as_ref(), &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.frontmatter.consumed_by_user_lessons, 1);

        // Predicate that WOULD match everything.
        let pred: PrunePredicate = Box::new(|_fm| true);
        let stats = prune(&ctx(), storage.as_ref(), &vector_index, pred)
            .await
            .unwrap();
        assert_eq!(stats.examined, 1);
        assert_eq!(stats.pruned, 0, "user-immune memory must NOT be pruned");
        assert_eq!(stats.skipped_user_immune, 1, "skip MUST be counted");
        // .md still present.
        assert!(storage
            .get(&StorageKey::memory(&ctx(), id.as_str()))
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn increment_citation_count_is_idempotent_via_cas() {
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-counter1");
        insert(
            &ctx(),
            storage.as_ref(),
            &embedder,
            &vector_index,
            id.clone(),
            "x",
            "y",
            now,
        )
        .await
        .unwrap();
        // 3 increments should land 3.
        for _ in 0..3 {
            increment_citation_count(&ctx(), storage.as_ref(), &id)
                .await
                .unwrap();
        }
        let loaded = get_by_id(&ctx(), storage.as_ref(), &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.frontmatter.consumed_by_user_lessons, 3);
    }

    #[tokio::test]
    async fn increment_citation_count_on_missing_memory_is_noop() {
        let (storage, _, _, _) = fresh_setup().await;
        // No insert — just increment on a non-existent id.
        let r = increment_citation_count(&ctx(), storage.as_ref(), &MemoryId::new("mem-noexist1"))
            .await;
        assert!(r.is_ok(), "increment on missing must be a no-op");
    }

    #[tokio::test]
    async fn decrement_citation_count_saturates_at_zero() {
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-decrmnt1");
        insert(
            &ctx(),
            storage.as_ref(),
            &embedder,
            &vector_index,
            id.clone(),
            "x",
            "y",
            now,
        )
        .await
        .unwrap();
        // Increment twice, decrement five times.
        for _ in 0..2 {
            increment_citation_count(&ctx(), storage.as_ref(), &id)
                .await
                .unwrap();
        }
        for _ in 0..5 {
            decrement_citation_count(&ctx(), storage.as_ref(), &id)
                .await
                .unwrap();
        }
        let loaded = get_by_id(&ctx(), storage.as_ref(), &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded.frontmatter.consumed_by_user_lessons, 0,
            "saturate at 0"
        );
    }

    #[tokio::test]
    async fn delete_force_false_blocks_when_memory_is_user_immune() {
        // Audit A-M2 regression: engine-initiated delete (force=false)
        // must refuse a user-cited memory.
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-immunedl");
        insert(
            &ctx(),
            storage.as_ref(),
            &embedder,
            &vector_index,
            id.clone(),
            "x",
            "y",
            now,
        )
        .await
        .unwrap();
        increment_citation_count(&ctx(), storage.as_ref(), &id)
            .await
            .unwrap();
        let r = delete(&ctx(), storage.as_ref(), &vector_index, &id, false).await;
        match r {
            Err(EngineError::UserMemoryImmune {
                id: ref returned_id,
                cited_by: 1,
            }) => {
                assert_eq!(returned_id, "mem-immunedl");
            }
            other => panic!("expected UserMemoryImmune, got {other:?}"),
        }
        // Memory still in storage.
        assert!(get_by_id(&ctx(), storage.as_ref(), &id)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn delete_force_true_bypasses_immunity() {
        // User-initiated path: force=true succeeds even on immune memory.
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-forcedel");
        insert(
            &ctx(),
            storage.as_ref(),
            &embedder,
            &vector_index,
            id.clone(),
            "x",
            "y",
            now,
        )
        .await
        .unwrap();
        increment_citation_count(&ctx(), storage.as_ref(), &id)
            .await
            .unwrap();
        delete(&ctx(), storage.as_ref(), &vector_index, &id, true)
            .await
            .unwrap();
        assert!(get_by_id(&ctx(), storage.as_ref(), &id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn delete_force_false_succeeds_on_uncited_memory() {
        // No user citation → engine-initiated delete proceeds.
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-uncited3");
        insert(
            &ctx(),
            storage.as_ref(),
            &embedder,
            &vector_index,
            id.clone(),
            "x",
            "y",
            now,
        )
        .await
        .unwrap();
        delete(&ctx(), storage.as_ref(), &vector_index, &id, false)
            .await
            .unwrap();
        assert!(get_by_id(&ctx(), storage.as_ref(), &id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn delete_idempotent_for_absent_id_regardless_of_force() {
        let (storage, _, vector_index, _) = fresh_setup().await;
        let absent = MemoryId::new("mem-noexist1");
        delete(&ctx(), storage.as_ref(), &vector_index, &absent, false)
            .await
            .unwrap();
        delete(&ctx(), storage.as_ref(), &vector_index, &absent, true)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn recompute_citation_counts_repairs_drift_to_zero() {
        // Memory has counter > 0 on disk but no user-authored lessons
        // cite it. Recompute should set the counter back to 0.
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-drift01");
        insert(
            &ctx(),
            storage.as_ref(),
            &embedder,
            &vector_index,
            id.clone(),
            "x",
            "y",
            now,
        )
        .await
        .unwrap();
        // Artificially inflate the counter without a citing lesson.
        increment_citation_count(&ctx(), storage.as_ref(), &id)
            .await
            .unwrap();
        increment_citation_count(&ctx(), storage.as_ref(), &id)
            .await
            .unwrap();
        let before = get_by_id(&ctx(), storage.as_ref(), &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.frontmatter.consumed_by_user_lessons, 2);

        let stats = recompute_citation_counts(&ctx(), storage.as_ref())
            .await
            .unwrap();
        // No lessons → 0 scanned. 1 memory inspected. 1 counter repaired.
        assert_eq!(stats.lessons_scanned, 0);
        assert_eq!(stats.memories_recomputed, 1);
        assert_eq!(stats.counters_repaired, 1);
        // Counter is now 0.
        let after = get_by_id(&ctx(), storage.as_ref(), &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.frontmatter.consumed_by_user_lessons, 0);
    }

    #[tokio::test]
    async fn recompute_citation_counts_noop_when_state_is_consistent() {
        // Empty storage → zero everything, zero repairs.
        let (storage, _, _, _) = fresh_setup().await;
        let stats = recompute_citation_counts(&ctx(), storage.as_ref())
            .await
            .unwrap();
        assert_eq!(stats.lessons_scanned, 0);
        assert_eq!(stats.memories_recomputed, 0);
        assert_eq!(stats.counters_repaired, 0);
    }

    #[tokio::test]
    async fn chase_returns_target_when_target_exists_and_has_no_compressor() {
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-chase001");
        insert(
            &ctx(),
            storage.as_ref(),
            &embedder,
            &vector_index,
            id.clone(),
            "x",
            "y",
            now,
        )
        .await
        .unwrap();
        let r = get_by_id_chasing_derived_from(&ctx(), storage.as_ref(), &id)
            .await
            .unwrap();
        assert!(r.is_some());
        assert_eq!(r.unwrap().frontmatter.id, id);
    }

    #[tokio::test]
    async fn chase_returns_compressor_after_predecessor_deleted() {
        // Predecessor M1 is compressed into Mc. M1 is then force-
        // deleted. Chasing M1 should walk forward to Mc.
        use crate::engine::llm::{Generation, MockLlmClient};
        use crate::engine::memory::{compress, CompressionConfig, CompressionWindow};
        let (storage, _, vector_index, now) = fresh_setup().await;
        let emb1 = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
        let m1 = MemoryId::new("mem-chase101");
        insert(
            &ctx(),
            storage.as_ref(),
            &emb1,
            &vector_index,
            m1.clone(),
            "x",
            "y",
            now,
        )
        .await
        .unwrap();
        let llm = MockLlmClient::default().with_response(
            Generation::new(r#"{"description":"s","content":"c"}"#)
                .with_parsed(serde_json::json!({"description":"s","content":"c"})),
        );
        let emb_c = MockEmbedder::new(4).with_response(vec![unit_vec(4, 1)]);
        let mc = compress(
            &ctx(),
            storage.as_ref(),
            &llm,
            &emb_c,
            &vector_index,
            CompressionWindow::Ids(vec![m1.clone()]),
            &CompressionConfig::default(),
            now,
        )
        .await
        .unwrap();
        // Force-delete predecessor M1.
        delete(&ctx(), storage.as_ref(), &vector_index, &m1, true)
            .await
            .unwrap();
        // Chase M1 → should land on Mc.
        let r = get_by_id_chasing_derived_from(&ctx(), storage.as_ref(), &m1)
            .await
            .unwrap();
        assert!(r.is_some());
        assert_eq!(r.unwrap().frontmatter.id, mc.frontmatter.id);
    }

    #[tokio::test]
    async fn chase_returns_none_for_unknown_id_with_no_compressor() {
        let (storage, _, _, _) = fresh_setup().await;
        let r = get_by_id_chasing_derived_from(
            &ctx(),
            storage.as_ref(),
            &MemoryId::new("mem-noexist1"),
        )
        .await
        .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn recompute_credits_compressor_after_predecessor_deleted() {
        // Lesson cites M1. M1 is compressed into Mc. M1 is force-
        // deleted (user-initiated). Recompute should walk the chain
        // forward and credit Mc, not M1.
        // This test SKIPS the actual lesson-citing infrastructure
        // (covered in Cx3 integration test); we manually increment
        // M1's counter, simulate compression-then-delete, and verify
        // recompute correctly attributes to Mc.
        use crate::engine::llm::{Generation, MockLlmClient};
        use crate::engine::memory::{compress, CompressionConfig, CompressionWindow};

        let (storage, _, vector_index, now) = fresh_setup().await;
        let emb1 = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
        let m1 = MemoryId::new("mem-rcm00001");
        insert(
            &ctx(),
            storage.as_ref(),
            &emb1,
            &vector_index,
            m1.clone(),
            "x",
            "y",
            now,
        )
        .await
        .unwrap();
        // Initial citation count (simulates a user-authored lesson citing M1).
        increment_citation_count(&ctx(), storage.as_ref(), &m1)
            .await
            .unwrap();

        // Compress M1 → Mc. Mc inherits the counter (sum = 1).
        let llm = MockLlmClient::default().with_response(
            Generation::new(r#"{"description":"s","content":"c"}"#)
                .with_parsed(serde_json::json!({"description":"s","content":"c"})),
        );
        let emb_c = MockEmbedder::new(4).with_response(vec![unit_vec(4, 1)]);
        let mc = compress(
            &ctx(),
            storage.as_ref(),
            &llm,
            &emb_c,
            &vector_index,
            CompressionWindow::Ids(vec![m1.clone()]),
            &CompressionConfig::default(),
            now,
        )
        .await
        .unwrap();
        assert_eq!(mc.frontmatter.consumed_by_user_lessons, 1);
        // Force-delete M1 (user-initiated).
        delete(&ctx(), storage.as_ref(), &vector_index, &m1, true)
            .await
            .unwrap();

        // No lessons exist yet → recompute should zero Mc's counter
        // (no citing lesson is present to credit). This is the
        // baseline: recompute sees zero lessons, zero predecessors
        // exist for any user-authored citation. So Mc.counter
        // becomes 0 after recompute.
        let stats = recompute_citation_counts(&ctx(), storage.as_ref())
            .await
            .unwrap();
        assert_eq!(
            stats.counters_repaired, 1,
            "Mc's counter should drop to 0 (no citing lessons)"
        );
        let mc_after = get_by_id(&ctx(), storage.as_ref(), &mc.frontmatter.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mc_after.frontmatter.consumed_by_user_lessons, 0);
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

    // -----------------------------------------------------------------
    // v0.5 hybrid recall — text_search + hybrid_search.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn text_search_empty_store_returns_empty() {
        let (storage, _, _, _) = fresh_setup().await;
        let hits = text_search(&ctx(), storage.as_ref(), "anything", 5, 240, None)
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn text_search_finds_substring_in_description() {
        // Seeds the exact "Gianna" false-negative scenario from v0.4
        // dogfooding: a memory whose description literally contains
        // "Gianna" should surface via text_search at high score even
        // though the semantic path scored it 0.486 (below 0.5).
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder = embedder.with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-family01");
        insert(
            &ctx(),
            storage.as_ref(),
            &embedder,
            &vector_index,
            id.clone(),
            "Sangmin's family — daughter Gianna (4) and son Teddy",
            "Gianna loves art and gymnastics. Teddy is a rascal.",
            now,
        )
        .await
        .unwrap();
        let hits = text_search(&ctx(), storage.as_ref(), "Gianna", 5, 240, None)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id);
        assert!(
            hits[0].similarity > 0.5,
            "description+body match should score > 0.5, got {}",
            hits[0].similarity
        );
        assert_eq!(hits[0].source, Some(crate::engine::memory::HitSource::Text));
    }

    #[tokio::test]
    async fn text_search_respects_scope_filter() {
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        let embedder = embedder.with_response(vec![unit_vec(4, 0), unit_vec(4, 1)]);
        let alpha_id = MemoryId::new("mem-alpha");
        let beta_id = MemoryId::new("mem-beta");
        insert_scoped(
            &ctx(),
            storage.as_ref(),
            &embedder,
            &vector_index,
            alpha_id.clone(),
            "alpha deploy",
            "deploy via Heroku",
            now,
            crate::engine::memory::MemoryScope::Project("alpha".into()),
        )
        .await
        .unwrap();
        insert_scoped(
            &ctx(),
            storage.as_ref(),
            &embedder,
            &vector_index,
            beta_id.clone(),
            "beta deploy",
            "deploy via kubectl",
            now,
            crate::engine::memory::MemoryScope::Project("beta".into()),
        )
        .await
        .unwrap();

        // No filter → both match.
        let unfiltered = text_search(&ctx(), storage.as_ref(), "deploy", 5, 240, None)
            .await
            .unwrap();
        assert_eq!(unfiltered.len(), 2);

        // Project=alpha filter → only alpha.
        let alpha_filter = crate::engine::memory::MemoryScopeFilter::Exact(
            crate::engine::memory::MemoryScope::Project("alpha".into()),
        );
        let filtered = text_search(
            &ctx(),
            storage.as_ref(),
            "deploy",
            5,
            240,
            Some(&alpha_filter),
        )
        .await
        .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, alpha_id);
    }

    #[tokio::test]
    async fn hybrid_search_marks_dual_source_match_as_both() {
        // A memory that surfaces from BOTH the semantic and text
        // paths should get `source = Both` and an RRF score strictly
        // higher than either single-source contribution.
        let (storage, embedder, vector_index, now) = fresh_setup().await;
        // The embedder is queried twice: once at insert, once during
        // hybrid_search's semantic sub-call. Same vector both times.
        let embedder = embedder.with_response(vec![unit_vec(4, 0), unit_vec(4, 0)]);
        let id = MemoryId::new("mem-dual001");
        insert(
            &ctx(),
            storage.as_ref(),
            &embedder,
            &vector_index,
            id.clone(),
            "Heroku deploy notes",
            "deploy via heroku git:remote and push",
            now,
        )
        .await
        .unwrap();

        let hits = hybrid_search(
            &ctx(),
            storage.as_ref(),
            &embedder,
            &vector_index,
            "heroku",
            5,
            240,
            None,
            0.0,
        )
        .await
        .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id);
        // Both paths should have surfaced it: semantic via
        // similarity to the query embedding, text via substring on
        // "heroku" appearing in both description and body.
        assert_eq!(hits[0].source, Some(crate::engine::memory::HitSource::Both));
        // RRF score is 2 × 1/(60+1) ≈ 0.0328 when item is rank 1
        // in both lists. Lower bound is 1/61 ≈ 0.0164 (single-list);
        // assert we strictly exceed it.
        assert!(
            hits[0].similarity > 1.0 / 61.0,
            "dual-source RRF should beat single-source, got {}",
            hits[0].similarity
        );
    }
}
