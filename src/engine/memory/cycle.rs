//! Phase E2 — `derived_from` cycle + depth detection.
//!
//! D-Cx8: walk each predecessor's `derived_from` chain back, detect
//! cycles via a visited-id set, cap depth at
//! [`super::compress::COMPRESSION_MAX_CHAIN_DEPTH`] (16).
//!
//! Algorithm:
//!   - DFS from each predecessor's `derived_from` set.
//!   - Track visited ids; revisit ⇒ cycle.
//!   - Track depth; exceed 16 ⇒ depth-exceeded (treated same as
//!     cycle for the audit trail — the chain that triggered it is
//!     reported in `EngineError::CompressionCycle.chain`).
//!
//! The walk loads predecessors lazily from storage. A predecessor
//! that doesn't exist is silently skipped (compress() will surface
//! the missing-predecessor error at a later point if it matters).

use std::collections::HashSet;

use crate::engine::context::Context;
use crate::engine::error::EngineError;
use crate::engine::memory::compress::COMPRESSION_MAX_CHAIN_DEPTH;
use crate::engine::memory::id::MemoryId;
use crate::engine::memory::store::get_by_id;
use crate::engine::memory::Memory;
use crate::engine::storage::Storage;

/// Detect cycles or depth-exceedance in the `derived_from` chains
/// rooted at each member of `predecessors`. Returns `Err` with the
/// triggering chain on detection; `Ok(())` otherwise.
pub(crate) async fn detect_cycle_in_window(
    ctx: &Context,
    storage: &dyn Storage,
    predecessors: &[Memory],
) -> Result<(), EngineError> {
    // Each predecessor walks INDEPENDENTLY — sharing the visited set
    // across walks would over-report (two distinct predecessors
    // legitimately sharing a common ancestor isn't a cycle).
    for root in predecessors {
        walk_chain(ctx, storage, &root.frontmatter.id, &root.frontmatter.derived_from)
            .await?;
    }
    Ok(())
}

/// Walk one chain back from `root` through its `derived_from`
/// ancestors. Detects cycles via visited set + depth via counter.
async fn walk_chain(
    ctx: &Context,
    storage: &dyn Storage,
    root_id: &MemoryId,
    initial_derived_from: &[MemoryId],
) -> Result<(), EngineError> {
    let mut visited: HashSet<MemoryId> = HashSet::new();
    visited.insert(root_id.clone());
    // Frontier = stack of (id, depth). DFS.
    let mut frontier: Vec<(MemoryId, usize)> = initial_derived_from
        .iter()
        .map(|id| (id.clone(), 1))
        .collect();

    while let Some((current, depth)) = frontier.pop() {
        if depth > COMPRESSION_MAX_CHAIN_DEPTH {
            return Err(EngineError::CompressionCycle {
                chain: visited.iter().map(|i| i.as_str().to_string()).collect(),
            });
        }
        if !visited.insert(current.clone()) {
            // Revisit — cycle. Include the offending id in the chain.
            let mut chain: Vec<String> =
                visited.iter().map(|i| i.as_str().to_string()).collect();
            chain.push(format!("(revisit: {current})"));
            return Err(EngineError::CompressionCycle { chain });
        }
        // Load the predecessor; if not found, treat as a leaf (no
        // further chain to walk). Missing predecessors are
        // legitimately possible mid-compression on a half-populated
        // window — compress() validates existence separately.
        let mem = match get_by_id(ctx, storage, &current).await? {
            Some(m) => m,
            None => continue,
        };
        for next in &mem.frontmatter.derived_from {
            frontier.push((next.clone(), depth + 1));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::embedding::MockEmbedder;
    use crate::engine::memory::store::insert;
    use crate::engine::memory::{compress as do_compress, CompressionConfig, CompressionWindow};
    use crate::engine::storage::MemoryStorage;
    use crate::engine::vector::HnswVectorIndex;
    use chrono::{DateTime, Utc};
    use std::sync::Arc;

    fn ctx() -> Context {
        Context::single_user_local()
    }

    fn now_t() -> DateTime<Utc> {
        "2026-05-14T12:00:00Z".parse().unwrap()
    }

    fn unit_vec(dim: usize, axis: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; dim];
        v[axis % dim] = 1.0;
        v
    }

    #[tokio::test]
    async fn detects_no_cycle_in_raw_memory_window() {
        // Raw memories have empty derived_from → no chain to walk.
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vi = HnswVectorIndex::new(4);
        let emb = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-raw00000");
        insert(&ctx(), storage.as_ref(), &emb, &vi, id.clone(), "x", "y", now_t()).await.unwrap();
        let mem = get_by_id(&ctx(), storage.as_ref(), &id).await.unwrap().unwrap();
        let r = detect_cycle_in_window(&ctx(), storage.as_ref(), &[mem]).await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn detects_no_cycle_in_one_level_compressed_window() {
        // Compress two raw memories, then try to detect cycle on the
        // compressed memory's window. The compressed memory has
        // derived_from = [raw1, raw2] (both raw, no further chain).
        // Should NOT detect a cycle.
        use crate::engine::llm::{Generation, MockLlmClient};
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vi = HnswVectorIndex::new(4);
        let emb1 = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
        let emb2 = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
        let id1 = MemoryId::new("mem-raw00001");
        let id2 = MemoryId::new("mem-raw00002");
        insert(&ctx(), storage.as_ref(), &emb1, &vi, id1.clone(), "a", "a body", now_t()).await.unwrap();
        insert(&ctx(), storage.as_ref(), &emb2, &vi, id2.clone(), "b", "b body", now_t()).await.unwrap();
        let llm = MockLlmClient::default().with_response(
            Generation::new(r#"{"description":"s","content":"c"}"#)
                .with_parsed(serde_json::json!({"description":"s","content":"c"})),
        );
        let emb_c = MockEmbedder::new(4).with_response(vec![unit_vec(4, 1)]);
        let mc = do_compress(
            &ctx(),
            storage.as_ref(),
            &llm,
            &emb_c,
            &vi,
            CompressionWindow::Ids(vec![id1, id2]),
            &CompressionConfig::default(),
            now_t(),
        )
        .await
        .unwrap();
        // Mc has 2 raw predecessors. Walk should succeed.
        let r = detect_cycle_in_window(&ctx(), storage.as_ref(), &[mc]).await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn detects_cycle_when_derived_from_self_references() {
        // Hand-build a memory where derived_from contains its own id
        // (synthetic corruption). The walk should detect the
        // revisit. We construct via direct frontmatter write since
        // the normal compress() path wouldn't produce this.
        use crate::engine::memory::{Memory, MemoryFrontmatter};
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let id = MemoryId::new("mem-cyc00001");
        // Bootstrap: write a memory file with derived_from = [self].
        let mut fm = MemoryFrontmatter::new(id.clone(), "cyclic", now_t());
        fm.derived_from = vec![id.clone()];
        let yaml = crate::engine::memory::store::render_memory_yaml(&fm, "body").unwrap();
        let key = crate::engine::storage::StorageKey::memory(&ctx(), id.as_str());
        storage.put(&key, bytes::Bytes::from(yaml)).await.unwrap();
        let mem = Memory::new(fm, "body");
        let r = detect_cycle_in_window(&ctx(), storage.as_ref(), &[mem]).await;
        match r {
            Err(EngineError::CompressionCycle { chain }) => {
                assert!(
                    chain.iter().any(|s| s.contains("mem-cyc00001")),
                    "cycle chain should include the cyclic id: {chain:?}"
                );
            }
            other => panic!("expected CompressionCycle, got {other:?}"),
        }
    }
}
