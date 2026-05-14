//! Phase E2 — `derived_from` cycle + depth detection.
//!
//! D-Cx8 + Phase E2 audit C1 fix: proper DAG cycle detection. Walks
//! each predecessor's `derived_from` chain back with an iterative
//! DFS that tracks the CURRENT PATH (not all-visited), so diamonds
//! (legitimate DAG shapes where the same ancestor is reachable via
//! two distinct branches) don't false-positive as cycles.
//!
//! Depth cap = [`super::compress::COMPRESSION_MAX_CHAIN_DEPTH`] (16).
//! Exceeded depth surfaces as `EngineError::CompressionCycle` (treated
//! same as a true cycle for audit purposes).
//!
//! The walk loads predecessors lazily from storage. A predecessor
//! that doesn't exist is silently skipped (compress() validates
//! existence separately).

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
    for root in predecessors {
        walk_chain(ctx, storage, &root.frontmatter.id, &root.frontmatter.derived_from)
            .await?;
    }
    Ok(())
}

/// One stack frame for the iterative DFS. `pending_children` is the
/// remaining-to-process iterator state; when empty, the frame pops
/// + the id is removed from the `path` set.
struct Frame {
    id: MemoryId,
    pending_children: Vec<MemoryId>,
    depth: usize,
}

/// Walk one chain back from `root` through its `derived_from`
/// ancestors. Iterative DFS with PATH-tracking — `path` contains
/// only the ids on the current root-to-leaf walk. Diamond DAGs
/// (sibling-branches sharing an ancestor) don't trigger the cycle
/// detector because the shared ancestor is popped from `path` when
/// the first branch finishes.
async fn walk_chain(
    ctx: &Context,
    storage: &dyn Storage,
    root_id: &MemoryId,
    initial_derived_from: &[MemoryId],
) -> Result<(), EngineError> {
    let mut path: HashSet<MemoryId> = HashSet::new();
    path.insert(root_id.clone());
    let mut stack: Vec<Frame> = vec![Frame {
        id: root_id.clone(),
        pending_children: initial_derived_from.iter().rev().cloned().collect(),
        depth: 0,
    }];

    while let Some(top) = stack.last_mut() {
        match top.pending_children.pop() {
            Some(child_id) => {
                let child_depth = top.depth + 1;
                if child_depth > COMPRESSION_MAX_CHAIN_DEPTH {
                    return Err(EngineError::CompressionCycle {
                        chain: stack_to_chain(&stack, Some(&child_id)),
                    });
                }
                // Path-based cycle check: if `child_id` is already
                // on the current root-to-here path, we have a true
                // cycle. (NOT the audit C1 false-positive: a diamond
                // ancestor is on `path` ONLY during its own subtree
                // walk; popped before the sibling branch visits.)
                if !path.insert(child_id.clone()) {
                    return Err(EngineError::CompressionCycle {
                        chain: stack_to_chain(&stack, Some(&child_id)),
                    });
                }
                // Load child's derived_from. Missing predecessor →
                // treat as leaf (no further chain).
                let child_children = match get_by_id(ctx, storage, &child_id).await? {
                    Some(m) => m.frontmatter.derived_from.into_iter().rev().collect(),
                    None => Vec::new(),
                };
                stack.push(Frame {
                    id: child_id,
                    pending_children: child_children,
                    depth: child_depth,
                });
            }
            None => {
                // Done processing this node's children. Pop from
                // path + stack. Sibling branches at the parent
                // level can now legitimately re-traverse any
                // ancestors that were only reached through THIS
                // branch.
                let frame = stack.pop().expect("just matched");
                path.remove(&frame.id);
            }
        }
    }
    Ok(())
}

/// Render the current stack + optional revisit-target as a
/// human-readable chain string for the error variant.
fn stack_to_chain(stack: &[Frame], revisit: Option<&MemoryId>) -> Vec<String> {
    let mut chain: Vec<String> = stack.iter().map(|f| f.id.as_str().to_string()).collect();
    if let Some(r) = revisit {
        chain.push(format!("(revisit: {r})"));
    }
    chain
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
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vi = HnswVectorIndex::new(4);
        let emb = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
        let id = MemoryId::new("mem-raw00000");
        insert(&ctx(), storage.as_ref(), &emb, &vi, id.clone(), "x", "y", now_t()).await.unwrap();
        let mem = get_by_id(&ctx(), storage.as_ref(), &id).await.unwrap().unwrap();
        assert!(detect_cycle_in_window(&ctx(), storage.as_ref(), &[mem]).await.is_ok());
    }

    /// Phase E2 audit C1 regression: diamond DAGs are NOT cycles.
    /// `Mcc.derived_from = [Mc1, Mc2]` where both Mc1 and Mc2 derive
    /// from raw1 is legitimate (recursive compression with overlap).
    /// The previous implementation false-positived because the
    /// visited set was shared across DFS branches.
    #[tokio::test]
    async fn diamond_dag_is_not_a_cycle() {
        use crate::engine::llm::{Generation, MockLlmClient};
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vi = HnswVectorIndex::new(4);
        // Insert raw1.
        let raw1 = MemoryId::new("mem-dia00001");
        let emb_r = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
        insert(&ctx(), storage.as_ref(), &emb_r, &vi, raw1.clone(), "raw1", "raw1 body", now_t()).await.unwrap();

        // Compress raw1 → Mc1.
        let llm = MockLlmClient::default()
            .with_response(Generation::new(r#"{"description":"mc1","content":"c1"}"#)
                .with_parsed(serde_json::json!({"description":"mc1","content":"c1"})))
            .with_response(Generation::new(r#"{"description":"mc2","content":"c2"}"#)
                .with_parsed(serde_json::json!({"description":"mc2","content":"c2"})));
        let emb_c1 = MockEmbedder::new(4).with_response(vec![unit_vec(4, 1)]);
        let mc1 = do_compress(
            &ctx(), storage.as_ref(), &llm, &emb_c1, &vi,
            CompressionWindow::Ids(vec![raw1.clone()]),
            &CompressionConfig::default(), now_t(),
        ).await.unwrap();

        // Compress raw1 (again) → Mc2. (Same raw1; two distinct
        // compressors pointing at it. Diamond when Mcc later
        // compresses both Mc1 + Mc2.)
        let emb_c2 = MockEmbedder::new(4).with_response(vec![unit_vec(4, 2)]);
        let mc2 = do_compress(
            &ctx(), storage.as_ref(), &llm, &emb_c2, &vi,
            // Slight perturbation in the window so mint_compressed_id
            // produces a different id than mc1.
            CompressionWindow::Ids(vec![raw1.clone()]),
            &CompressionConfig::default(),
            // shift the timestamp slightly to differentiate the id
            now_t() + chrono::Duration::milliseconds(1),
        ).await.unwrap();
        assert_ne!(mc1.frontmatter.id, mc2.frontmatter.id, "diamond setup needs distinct compressors");

        // Now hand-build Mcc with derived_from = [Mc1, Mc2].
        // (We can't compress(Mc1, Mc2) easily because they share raw1
        // — that's the diamond setup. We assert cycle-check directly.)
        use crate::engine::memory::{Memory, MemoryFrontmatter};
        let mcc_id = MemoryId::new("mem-c-diamond01");
        let mut mcc_fm = MemoryFrontmatter::new(mcc_id.clone(), "mcc", now_t());
        mcc_fm.derived_from = vec![mc1.frontmatter.id.clone(), mc2.frontmatter.id.clone()];
        let mcc = Memory::new(mcc_fm, "mcc body");

        // Run cycle detection. Must NOT detect a cycle (this is the
        // C1 regression).
        let r = detect_cycle_in_window(&ctx(), storage.as_ref(), &[mcc]).await;
        assert!(
            r.is_ok(),
            "diamond DAG must NOT trigger cycle detector: {r:?}"
        );
    }

    #[tokio::test]
    async fn detects_self_reference_as_cycle() {
        use crate::engine::memory::{Memory, MemoryFrontmatter};
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let id = MemoryId::new("mem-cyc00001");
        let mut fm = MemoryFrontmatter::new(id.clone(), "cyclic", now_t());
        fm.derived_from = vec![id.clone()];
        let yaml = crate::engine::memory::store::render_memory_yaml(&fm, "body").unwrap();
        let key = crate::engine::storage::StorageKey::memory(&ctx(), id.as_str());
        storage.put(&key, bytes::Bytes::from(yaml)).await.unwrap();
        let mem = Memory::new(fm, "body");
        let r = detect_cycle_in_window(&ctx(), storage.as_ref(), &[mem]).await;
        assert!(matches!(r, Err(EngineError::CompressionCycle { .. })));
    }

    #[tokio::test]
    async fn detects_two_node_cycle() {
        // M1.derived_from = [M2]; M2.derived_from = [M1]. Walking
        // from M1 hits M1 again via M2. Cycle.
        use crate::engine::memory::{Memory, MemoryFrontmatter};
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let m1_id = MemoryId::new("mem-cy200001");
        let m2_id = MemoryId::new("mem-cy200002");
        // Write M2 first with derived_from = [M1].
        let mut m2_fm = MemoryFrontmatter::new(m2_id.clone(), "m2", now_t());
        m2_fm.derived_from = vec![m1_id.clone()];
        let yaml = crate::engine::memory::store::render_memory_yaml(&m2_fm, "body").unwrap();
        let key = crate::engine::storage::StorageKey::memory(&ctx(), m2_id.as_str());
        storage.put(&key, bytes::Bytes::from(yaml)).await.unwrap();
        // M1 has derived_from = [M2]. Construct in memory + run.
        let mut m1_fm = MemoryFrontmatter::new(m1_id.clone(), "m1", now_t());
        m1_fm.derived_from = vec![m2_id.clone()];
        let yaml = crate::engine::memory::store::render_memory_yaml(&m1_fm, "body").unwrap();
        let key = crate::engine::storage::StorageKey::memory(&ctx(), m1_id.as_str());
        storage.put(&key, bytes::Bytes::from(yaml)).await.unwrap();
        let m1 = Memory::new(m1_fm, "body");
        let r = detect_cycle_in_window(&ctx(), storage.as_ref(), &[m1]).await;
        assert!(matches!(r, Err(EngineError::CompressionCycle { .. })));
    }
}
