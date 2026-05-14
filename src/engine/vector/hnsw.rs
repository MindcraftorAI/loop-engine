//! `HnswVectorIndex` — local in-process vector index backed by
//! `hnsw_rs`. Phase E C-E2.
//!
//! ## Algorithm
//!
//! HNSW (Hierarchical Navigable Small World) gives sub-10ms p95 search
//! over 100K+ vectors at dim ≤1536 — the envelope Phase E targets.
//! Cosine similarity via [`anndists::dist::distances::DistCosine`].
//!
//! ## State + concurrency
//!
//! HNSW state + the id-mapping table + the tombstone set live behind a
//! single `parking_lot::RwLock`. Lookups (`search`) take a read lock;
//! mutations (`insert`, `delete`) take a write lock. parking_lot's
//! locks are panic-free (no poisoning) and faster than std on most
//! platforms.
//!
//! ## Tombstone-and-filter delete
//!
//! HNSW has NO native delete (per the algorithm — deleting a node
//! would require rebuilding graph edges across multiple layers).
//! [`HnswVectorIndex::delete`] inserts the id into a tombstone set;
//! [`HnswVectorIndex::search`] filters results against the set
//! before returning. Compaction (rebuilding without tombstoned ids)
//! is a future operation; today the tombstone set grows until
//! [`persist`](HnswVectorIndex::persist) + reload-from-fresh-build.
//!
//! ## Persistence via `Storage` trait
//!
//! `hnsw_rs::Hnsw::file_dump` writes to a real filesystem path. Our
//! engine architecture forbids direct `std::fs` from non-backend
//! engine code, BUT [`HnswVectorIndex`] is itself a backend impl —
//! the same allowance that [`crate::engine::storage::LocalFsStorage`]
//! has. We bridge via a tempdir: dump to temp, read bytes, write
//! through [`Storage`]. Symmetric on reload.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use async_trait::async_trait;
use bytes::Bytes;
use hnsw_rs::anndists::dist::distances::DistCosine;
use hnsw_rs::api::AnnT;
use hnsw_rs::hnsw::Hnsw;
use hnsw_rs::hnswio::HnswIo;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tokio::task::spawn_blocking;

use crate::engine::context::Context;
use crate::engine::memory::MemoryId;
use crate::engine::storage::{Storage, StorageKey};
use crate::engine::vector::error::VectorIndexError;
use crate::engine::vector::{sealed::Sealed, SearchHit, VectorIndex};

// Phase E D-E4 / D-E3 — tuning constants for the local HNSW. Defaults
// tuned for the engine's "memory store" use case: insert-heavy at
// embedding-time, occasional search at manifest-assembly time, N up
// to ~100K, dimension 384-1536.
const HNSW_MAX_NB_CONNECTION: usize = 16; // M parameter, default for HNSW
const HNSW_MAX_LAYER: usize = 16;
const HNSW_MAX_ELEMENTS: usize = 100_000;
const HNSW_EF_CONSTRUCTION: usize = 200;
const HNSW_EF_SEARCH_PADDING: usize = 100; // ef arg in search

const DUMP_BASENAME: &str = "loop_hnsw";

/// Storage keys for HNSW persistence (D-E3). Three keys — two raw
/// dump files from hnsw_rs (data + graph) plus an engine-managed
/// metadata sidecar with id-map + tombstones + dim + count.
fn key_hnsw_data() -> StorageKey {
    StorageKey::from_raw("vector_index/loop_hnsw.hnsw.data".to_string())
}
fn key_hnsw_graph() -> StorageKey {
    StorageKey::from_raw("vector_index/loop_hnsw.hnsw.graph".to_string())
}
fn key_hnsw_meta() -> StorageKey {
    StorageKey::from_raw("vector_index/loop_hnsw_meta.json".to_string())
}

/// Engine-managed sidecar metadata persisted alongside the raw hnsw_rs
/// dump. Carries the load-bearing state that hnsw_rs can't preserve on
/// its own: the `MemoryId` ↔ point_id mapping, the tombstone set, the
/// next-point-id counter, and the dimension (cross-checks the dump).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HnswMeta {
    /// Vector dimension. Must equal the dimension of every embedding
    /// in the dump.
    dimensions: usize,
    /// Next point_id to mint on the next insert.
    next_point_id: usize,
    /// point_id → MemoryId. Used to resolve search results.
    id_map: HashMap<usize, String>,
    /// MemoryIds that have been deleted. Search filters against this.
    tombstones: HashSet<String>,
}

struct HnswInner {
    hnsw: Hnsw<'static, f32, DistCosine>,
    dimensions: usize,
    next_point_id: usize,
    id_map: HashMap<usize, MemoryId>,
    rev_map: HashMap<MemoryId, usize>,
    tombstones: HashSet<MemoryId>,
}

impl std::fmt::Debug for HnswInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HnswInner")
            .field("dimensions", &self.dimensions)
            .field("next_point_id", &self.next_point_id)
            .field("id_map_len", &self.id_map.len())
            .field("tombstones_len", &self.tombstones.len())
            .finish()
    }
}

/// HNSW-backed [`VectorIndex`] for in-process memory search. Holds
/// state behind a `parking_lot::RwLock`; supports concurrent reads,
/// exclusive writes. Persistence rides on [`Storage`].
pub struct HnswVectorIndex {
    inner: RwLock<HnswInner>,
}

impl std::fmt::Debug for HnswVectorIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HnswVectorIndex")
            .field("inner", &*self.inner.read())
            .finish()
    }
}

impl HnswVectorIndex {
    /// Construct an empty index for the given dimension.
    pub fn new(dimensions: usize) -> Self {
        let hnsw = Hnsw::<f32, DistCosine>::new(
            HNSW_MAX_NB_CONNECTION,
            HNSW_MAX_ELEMENTS,
            HNSW_MAX_LAYER,
            HNSW_EF_CONSTRUCTION,
            DistCosine {},
        );
        Self {
            inner: RwLock::new(HnswInner {
                hnsw,
                dimensions,
                next_point_id: 0,
                id_map: HashMap::new(),
                rev_map: HashMap::new(),
                tombstones: HashSet::new(),
            }),
        }
    }

    /// Reload an index from storage. Returns a fresh empty index if
    /// no dump exists yet (first-run case). Used by the daemon at
    /// startup.
    pub async fn load(
        _ctx: &Context,
        storage: &dyn Storage,
        dimensions: usize,
    ) -> Result<Self, VectorIndexError> {
        let meta_bytes = storage
            .get(&key_hnsw_meta())
            .await
            .map_err(VectorIndexError::transport)?;
        let meta_bytes = match meta_bytes {
            Some(b) => b,
            None => return Ok(Self::new(dimensions)),
        };
        let meta: HnswMeta = serde_json::from_slice(&meta_bytes).map_err(|e| {
            VectorIndexError::Internal(format!("malformed hnsw meta: {e}"))
        })?;
        if meta.dimensions != dimensions {
            return Err(VectorIndexError::DimensionMismatch {
                provided: dimensions,
                expected: meta.dimensions,
            });
        }
        let data_bytes = storage
            .get(&key_hnsw_data())
            .await
            .map_err(VectorIndexError::transport)?
            .ok_or_else(|| {
                VectorIndexError::Internal(
                    "hnsw_data missing despite meta present".to_string(),
                )
            })?;
        let graph_bytes = storage
            .get(&key_hnsw_graph())
            .await
            .map_err(VectorIndexError::transport)?
            .ok_or_else(|| {
                VectorIndexError::Internal(
                    "hnsw_graph missing despite meta present".to_string(),
                )
            })?;

        // Bridge to hnsw_rs's file-based loader via a tempdir.
        //
        // hnsw_rs's `Hnsw<'b, T, D>` borrows from the `HnswIo`
        // reloader (the lifetime bound on `load_hnsw` is `'a: 'b`).
        // To get an owned `Hnsw<'static, _, _>` we leak the reloader
        // — it's a small struct, and loads happen once per daemon
        // start (not hot path). With `datamap=false` (the default)
        // the actual graph + point data lives inside the returned
        // Hnsw, so the leaked HnswIo carries minimal state.
        let hnsw = spawn_blocking(move || -> Result<_, VectorIndexError> {
            let tmp = TempDir::new().map_err(|e| {
                VectorIndexError::Internal(format!("tempdir create: {e}"))
            })?;
            let data_path = tmp.path().join(format!("{DUMP_BASENAME}.hnsw.data"));
            let graph_path = tmp.path().join(format!("{DUMP_BASENAME}.hnsw.graph"));
            fs::write(&data_path, &data_bytes).map_err(|e| {
                VectorIndexError::Internal(format!("write data tempfile: {e}"))
            })?;
            fs::write(&graph_path, &graph_bytes).map_err(|e| {
                VectorIndexError::Internal(format!("write graph tempfile: {e}"))
            })?;
            let reloader: &'static mut HnswIo =
                Box::leak(Box::new(HnswIo::new(tmp.path(), DUMP_BASENAME)));
            let hnsw = reloader
                .load_hnsw::<f32, DistCosine>()
                .map_err(|e| VectorIndexError::Internal(format!("hnsw reload: {e}")))?;
            // Tempdir lives until the end of this closure; the loaded
            // Hnsw has by now read everything it needs out of the
            // files (datamap=false). Drop is implicit at scope end.
            drop(tmp);
            Ok(hnsw)
        })
        .await
        .map_err(|e| VectorIndexError::Internal(format!("join: {e}")))??;

        let id_map: HashMap<usize, MemoryId> = meta
            .id_map
            .into_iter()
            .map(|(k, v)| (k, MemoryId::new(v)))
            .collect();
        let rev_map: HashMap<MemoryId, usize> = id_map
            .iter()
            .map(|(k, v)| (v.clone(), *k))
            .collect();
        let tombstones: HashSet<MemoryId> = meta
            .tombstones
            .into_iter()
            .map(MemoryId::new)
            .collect();

        Ok(Self {
            inner: RwLock::new(HnswInner {
                hnsw,
                dimensions: meta.dimensions,
                next_point_id: meta.next_point_id,
                id_map,
                rev_map,
                tombstones,
            }),
        })
    }
}

impl Sealed for HnswVectorIndex {}

#[async_trait]
impl VectorIndex for HnswVectorIndex {
    async fn insert(
        &self,
        _ctx: &Context,
        id: &MemoryId,
        vector: &[f32],
    ) -> Result<(), VectorIndexError> {
        let dims = self.dimensions();
        if vector.len() != dims {
            return Err(VectorIndexError::DimensionMismatch {
                provided: vector.len(),
                expected: dims,
            });
        }
        if vector.iter().any(|v| !v.is_finite()) {
            return Err(VectorIndexError::InvalidVector(
                "vector contains NaN or Inf".into(),
            ));
        }
        let mut inner = self.inner.write();
        // Replace semantics: if the id already exists, tombstone the
        // old point_id (so search no longer surfaces it) and insert
        // the new vector with a fresh point_id.
        if let Some(old_pid) = inner.rev_map.remove(id) {
            inner.tombstones.insert(id.clone());
            inner.id_map.remove(&old_pid);
        }
        // Clear ANY prior tombstone — the new insert replaces the
        // deleted state.
        inner.tombstones.remove(id);

        let pid = inner.next_point_id;
        inner.next_point_id += 1;
        inner.hnsw.insert_slice((vector, pid));
        inner.id_map.insert(pid, id.clone());
        inner.rev_map.insert(id.clone(), pid);
        Ok(())
    }

    async fn search(
        &self,
        _ctx: &Context,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<SearchHit>, VectorIndexError> {
        let dims = self.dimensions();
        if query.len() != dims {
            return Err(VectorIndexError::DimensionMismatch {
                provided: query.len(),
                expected: dims,
            });
        }
        if query.iter().any(|v| !v.is_finite()) {
            return Err(VectorIndexError::InvalidVector(
                "query contains NaN or Inf".into(),
            ));
        }
        let inner = self.inner.read();
        // Over-fetch so tombstone filtering doesn't starve the top-k.
        let fetch = k.saturating_add(inner.tombstones.len());
        let neighbours = inner.hnsw.search(query, fetch, HNSW_EF_SEARCH_PADDING);
        let mut out: Vec<SearchHit> = Vec::with_capacity(k);
        for n in neighbours {
            let pid = n.get_origin_id();
            let Some(memory_id) = inner.id_map.get(&pid) else {
                // Orphan point_id (no id_map entry). Skip silently —
                // can occur after replace-on-insert leaves the old
                // pid in the hnsw graph but removes its id_map entry.
                continue;
            };
            if inner.tombstones.contains(memory_id) {
                continue;
            }
            // hnsw_rs returns DISTANCE (lower = better); we want
            // SIMILARITY in [0.0, 1.0]. For DistCosine, distance ≈
            // 1 - cosine_similarity, so similarity = 1 - distance.
            // Clamp for floating-point error.
            let similarity = (1.0_f32 - n.get_distance()).clamp(0.0, 1.0);
            out.push(SearchHit {
                id: memory_id.clone(),
                similarity,
            });
            if out.len() == k {
                break;
            }
        }
        Ok(out)
    }

    async fn delete(
        &self,
        _ctx: &Context,
        id: &MemoryId,
    ) -> Result<(), VectorIndexError> {
        let mut inner = self.inner.write();
        if inner.rev_map.contains_key(id) {
            inner.tombstones.insert(id.clone());
        }
        // Idempotent — deleting an absent id is Ok.
        Ok(())
    }

    async fn persist(
        &self,
        _ctx: &Context,
        storage: &dyn Storage,
    ) -> Result<(), VectorIndexError> {
        // 1. Snapshot the meta state under the read lock; dump the
        //    HNSW to a tempdir (this is the slow op).
        let (meta_bytes, data_bytes, graph_bytes) = {
            let inner = self.inner.read();
            let meta = HnswMeta {
                dimensions: inner.dimensions,
                next_point_id: inner.next_point_id,
                id_map: inner
                    .id_map
                    .iter()
                    .map(|(k, v)| (*k, v.as_str().to_string()))
                    .collect(),
                tombstones: inner
                    .tombstones
                    .iter()
                    .map(|m| m.as_str().to_string())
                    .collect(),
            };
            let meta_bytes = serde_json::to_vec(&meta).map_err(|e| {
                VectorIndexError::Internal(format!("serialize meta: {e}"))
            })?;
            // Dump HNSW to tempdir while holding the read lock — the
            // hnsw_rs `file_dump` is read-only on the index state.
            // We then read the bytes out and release the lock; the
            // Storage::put calls happen after.
            let tmp = TempDir::new().map_err(|e| {
                VectorIndexError::Internal(format!("tempdir create: {e}"))
            })?;
            inner
                .hnsw
                .file_dump(tmp.path(), DUMP_BASENAME)
                .map_err(|e| VectorIndexError::Internal(format!("hnsw dump: {e}")))?;
            let data_path = tmp.path().join(format!("{DUMP_BASENAME}.hnsw.data"));
            let graph_path = tmp.path().join(format!("{DUMP_BASENAME}.hnsw.graph"));
            let data_bytes = fs::read(&data_path).map_err(|e| {
                VectorIndexError::Internal(format!("read dump data: {e}"))
            })?;
            let graph_bytes = fs::read(&graph_path).map_err(|e| {
                VectorIndexError::Internal(format!("read dump graph: {e}"))
            })?;
            (meta_bytes, data_bytes, graph_bytes)
        };

        // 2. Write through Storage.
        storage
            .put(&key_hnsw_data(), Bytes::from(data_bytes))
            .await
            .map_err(VectorIndexError::transport)?;
        storage
            .put(&key_hnsw_graph(), Bytes::from(graph_bytes))
            .await
            .map_err(VectorIndexError::transport)?;
        storage
            .put(&key_hnsw_meta(), Bytes::from(meta_bytes))
            .await
            .map_err(VectorIndexError::transport)?;
        Ok(())
    }

    fn dimensions(&self) -> usize {
        self.inner.read().dimensions
    }
}

// Suppress an unused-import warning if the path-aware Path import
// becomes superfluous on a refactor. Currently used in the tempdir
// bridge inside persist/load.
#[allow(dead_code)]
fn _silence_path(_p: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::Context;
    use crate::engine::storage::MemoryStorage;
    use std::sync::Arc;

    fn ctx() -> Context {
        Context::single_user_local()
    }

    fn unit_vec(dim: usize, axis: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; dim];
        v[axis % dim] = 1.0;
        v
    }

    #[tokio::test]
    async fn insert_then_search_returns_self_first() {
        let idx = HnswVectorIndex::new(4);
        let a = MemoryId::new("mem-aaaaaaaa");
        let b = MemoryId::new("mem-bbbbbbbb");
        idx.insert(&ctx(), &a, &unit_vec(4, 0)).await.unwrap();
        idx.insert(&ctx(), &b, &unit_vec(4, 1)).await.unwrap();
        // Query along axis 0 — should match `a` first.
        let hits = idx.search(&ctx(), &unit_vec(4, 0), 2).await.unwrap();
        assert!(!hits.is_empty(), "expected at least one hit");
        assert_eq!(hits[0].id, a);
        assert!(hits[0].similarity > 0.9, "self-match similarity: {}", hits[0].similarity);
    }

    #[tokio::test]
    async fn dimension_mismatch_on_insert_errors() {
        let idx = HnswVectorIndex::new(4);
        let id = MemoryId::new("mem-aaaaaaaa");
        let r = idx.insert(&ctx(), &id, &[1.0, 0.0]).await;
        match r {
            Err(VectorIndexError::DimensionMismatch { provided: 2, expected: 4 }) => {}
            other => panic!("expected DimensionMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn nan_vector_rejected() {
        let idx = HnswVectorIndex::new(2);
        let id = MemoryId::new("mem-aaaaaaaa");
        let r = idx.insert(&ctx(), &id, &[1.0, f32::NAN]).await;
        assert!(matches!(r, Err(VectorIndexError::InvalidVector(_))));
    }

    #[tokio::test]
    async fn delete_tombstones_id_and_search_skips() {
        let idx = HnswVectorIndex::new(4);
        let a = MemoryId::new("mem-aaaaaaaa");
        let b = MemoryId::new("mem-bbbbbbbb");
        idx.insert(&ctx(), &a, &unit_vec(4, 0)).await.unwrap();
        idx.insert(&ctx(), &b, &unit_vec(4, 1)).await.unwrap();
        idx.delete(&ctx(), &a).await.unwrap();
        // Query along axis 0 — `a` is tombstoned; the next-best match
        // (lower similarity) might still get returned but it should
        // NOT be `a`.
        let hits = idx.search(&ctx(), &unit_vec(4, 0), 5).await.unwrap();
        assert!(
            !hits.iter().any(|h| h.id == a),
            "tombstoned id should not appear: {hits:?}"
        );
    }

    #[tokio::test]
    async fn delete_idempotent_for_absent_id() {
        let idx = HnswVectorIndex::new(4);
        let absent = MemoryId::new("mem-noexist1");
        idx.delete(&ctx(), &absent).await.unwrap();
        idx.delete(&ctx(), &absent).await.unwrap();
    }

    #[tokio::test]
    async fn insert_replaces_when_id_already_present() {
        let idx = HnswVectorIndex::new(4);
        let a = MemoryId::new("mem-replace1");
        idx.insert(&ctx(), &a, &unit_vec(4, 0)).await.unwrap();
        // Insert with a different vector — old should be tombstoned
        // internally; the new vector should match the new query.
        idx.insert(&ctx(), &a, &unit_vec(4, 2)).await.unwrap();
        let hits = idx.search(&ctx(), &unit_vec(4, 2), 1).await.unwrap();
        assert_eq!(hits.first().map(|h| &h.id), Some(&a));
        assert!(hits[0].similarity > 0.9);
    }

    #[tokio::test]
    async fn persist_and_load_round_trip() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let idx1 = HnswVectorIndex::new(4);
        let a = MemoryId::new("mem-aaaaaaaa");
        let b = MemoryId::new("mem-bbbbbbbb");
        idx1.insert(&ctx(), &a, &unit_vec(4, 0)).await.unwrap();
        idx1.insert(&ctx(), &b, &unit_vec(4, 1)).await.unwrap();
        idx1.delete(&ctx(), &b).await.unwrap();
        idx1.persist(&ctx(), storage.as_ref()).await.unwrap();

        let idx2 = HnswVectorIndex::load(&ctx(), storage.as_ref(), 4).await.unwrap();
        // Tombstone state preserved.
        let hits = idx2.search(&ctx(), &unit_vec(4, 1), 5).await.unwrap();
        assert!(
            !hits.iter().any(|h| h.id == b),
            "tombstone state must survive persist+reload"
        );
        // Active state preserved.
        let hits = idx2.search(&ctx(), &unit_vec(4, 0), 1).await.unwrap();
        assert_eq!(hits.first().map(|h| &h.id), Some(&a));
    }

    #[tokio::test]
    async fn load_returns_fresh_index_when_storage_empty() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let idx = HnswVectorIndex::load(&ctx(), storage.as_ref(), 4).await.unwrap();
        assert_eq!(idx.dimensions(), 4);
        // Empty — search returns no hits.
        let hits = idx.search(&ctx(), &unit_vec(4, 0), 5).await.unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn load_with_wrong_dimension_errors() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let idx1 = HnswVectorIndex::new(4);
        idx1.insert(&ctx(), &MemoryId::new("mem-aaaaaaaa"), &unit_vec(4, 0))
            .await
            .unwrap();
        idx1.persist(&ctx(), storage.as_ref()).await.unwrap();
        let r = HnswVectorIndex::load(&ctx(), storage.as_ref(), 8).await;
        match r {
            Err(VectorIndexError::DimensionMismatch { provided: 8, expected: 4 }) => {}
            other => panic!("expected DimensionMismatch, got {other:?}"),
        }
    }
}
