//! Vector index abstraction.
//!
//! Phase E D-E4: third sealed engine trait (mirrors
//! [`crate::engine::storage::Storage`] +
//! [`crate::engine::embedding::Embedder`]). Phase E ships:
//! - The trait + types (this commit, C-E1).
//! - `HnswVectorIndex` default local impl using `hnsw_rs` (C-E2).
//!
//! Monolith adapters can ship remote impls (Qdrant, Pinecone, etc) via
//! the workspace pattern when the monolith repo lands. Sealed trait
//! prevents external crates from implementing directly.
//!
//! **Important**: HNSW (the default algo) has NO native delete.
//! [`HnswVectorIndex`] (Phase E C-E2) implements tombstone-and-filter
//! — `delete` marks the id as removed; `search` results filter
//! against the tombstone set; full compaction is a future operation.
//! Remote backends with native delete override via the trait
//! contract.

use std::fmt::Debug;

use async_trait::async_trait;

pub mod error;
pub mod hnsw;

pub use error::VectorIndexError;
pub use hnsw::HnswVectorIndex;

use crate::engine::context::Context;
use crate::engine::memory::MemoryId;
use crate::engine::storage::Storage;

/// One result from [`VectorIndex::search`]. `similarity` is cosine
/// similarity in `[0.0, 1.0]` (1.0 = identical direction). Higher is
/// better.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SearchHit {
    pub id: MemoryId,
    pub similarity: f32,
}

impl SearchHit {
    pub fn new(id: MemoryId, similarity: f32) -> Self {
        Self { id, similarity }
    }
}

/// Vector-index abstraction. Owns the HNSW (or remote ANN) state for
/// memory embeddings and exposes insert/search/delete/persist.
///
/// `&self` with interior mutability: impls hold the state behind a
/// `RwLock` or equivalent. Adapters control concurrency; callers
/// don't fight for `&mut self`. Trait is sealed.
///
/// Phase E C-E1 ships the trait + the local impl's seal marker.
/// [`HnswVectorIndex`](self::HnswVectorIndex) lands in C-E2.
#[async_trait]
pub trait VectorIndex: Send + Sync + Debug + sealed::Sealed {
    /// Insert (or update) a vector for `id`. Replacing an existing
    /// id is implementation-defined; the engine-shipped HNSW impl
    /// (C-E2) tombstones the old entry and inserts the new one.
    async fn insert(
        &self,
        ctx: &Context,
        id: &MemoryId,
        vector: &[f32],
    ) -> Result<(), VectorIndexError>;

    /// Find the `k` nearest neighbors of `query` by cosine similarity.
    /// Returns hits in descending similarity order. Tombstoned entries
    /// are excluded from results.
    async fn search(
        &self,
        ctx: &Context,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<SearchHit>, VectorIndexError>;

    /// Remove `id` from the index. The local HNSW impl tombstones;
    /// `search` filters out tombstoned ids. Remote backends with
    /// native delete (Qdrant, Pinecone) physically remove. Callers
    /// MUST NOT depend on physical removal.
    async fn delete(
        &self,
        ctx: &Context,
        id: &MemoryId,
    ) -> Result<(), VectorIndexError>;

    /// Serialize index state to the supplied `Storage`. The local
    /// HNSW impl writes two known keys: `vector_index/hnsw_state.bin`
    /// (the `hnswio::HnswIo` dump) and `vector_index/hnsw_meta.yaml`
    /// (algo version, dim, count). Callers SHOULD invoke periodically
    /// (e.g. on shutdown) — engine doesn't auto-persist on every
    /// insert (would be too slow).
    async fn persist(
        &self,
        ctx: &Context,
        storage: &dyn Storage,
    ) -> Result<(), VectorIndexError>;

    /// Vector dimensionality. Must match the [`Embedder::dimensions()`]
    /// of the embedder used to produce vectors for this index.
    fn dimensions(&self) -> usize;
}

pub(crate) mod sealed {
    /// Private marker — external crates cannot satisfy this, so they
    /// cannot implement [`super::VectorIndex`] directly. Monolith
    /// adapters land via the workspace pattern.
    ///
    /// Stabilized per Phase H D-H2: sealed for v1.0. See
    /// `INTEGRATING.md` for the workspace-pattern integration
    /// model.
    pub trait Sealed {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_hit_constructor() {
        let id = MemoryId::new("mem-aaaaaaaa");
        let hit = SearchHit::new(id.clone(), 0.85);
        assert_eq!(hit.id, id);
        assert!((hit.similarity - 0.85).abs() < f32::EPSILON);
    }

    /// Compile-time test: `VectorIndex` is object-safe. If this stops
    /// compiling, Phase E's `dyn` guarantee is broken.
    #[allow(dead_code)]
    fn object_safety_check(_: std::sync::Arc<dyn VectorIndex>) {}
}
