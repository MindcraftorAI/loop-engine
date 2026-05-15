//! Memory lifecycle helpers — Phase E2 audit-fix close.
//!
//! Phase E2 audit B-M2: split out of `store.rs` (which had grown to
//! 729 prod LOC, past the 500 cap). Houses:
//!   - `get_by_id_chasing_derived_from` — forward-walks `derived_from`
//!     chains for citation-resolution after predecessor force-delete.
//!   - `find_compressor_of` — internal one-hop helper for the chase.
//!   - `recompute_citation_counts` — drift escape hatch. Walks all
//!     lessons + all memories, repairs counter drift, surfaces
//!     orphan citations via `RecomputeStats::orphan_citations`
//!     (Phase E2 audit C2 fix).
//!   - `set_citation_count` — internal CAS-RMW helper.
//!   - `RecomputeStats` — public summary type with the new
//!     `orphan_citations` field.

use std::collections::HashMap;

use bytes::Bytes;
use tracing::warn;

use crate::engine::context::Context;
use crate::engine::error::EngineError;
use crate::engine::memory::compress::COMPRESSION_MAX_CHAIN_DEPTH;
use crate::engine::memory::id::MemoryId;
use crate::engine::memory::store::{
    get_by_id, parse_memory_file, render_memory_yaml, CITATION_CAS_MAX_RETRIES,
};
use crate::engine::memory::Memory;
use crate::engine::storage::{Storage, StorageKey};

/// Phase E2 D-Cx6: forward-walk the `derived_from` chain from
/// `target` to the most-recent existing memory. Useful for citation
/// resolution when raw memories have been force-deleted post-
/// compression.
///
/// Walks forward through compressors: if `target` exists AND some Mc
/// has `target` in its `derived_from`, recurses on Mc. Continues
/// until either no further compressor exists (returns the leaf) OR
/// no memory at all exists in the chain (returns None) OR depth-cap
/// `COMPRESSION_MAX_CHAIN_DEPTH` (16) is hit.
///
/// Cost: O(N) per hop (full memory scan). Use sparingly; for bulk
/// resolution (e.g. inside `recompute_citation_counts`) build a
/// predecessor→compressor map ONCE and walk that instead — see
/// `build_predecessor_index` (private to this module).
pub async fn get_by_id_chasing_derived_from(
    ctx: &Context,
    storage: &dyn Storage,
    target: &MemoryId,
) -> Result<Option<Memory>, EngineError> {
    let mut current = target.clone();
    let mut visited: std::collections::HashSet<MemoryId> = std::collections::HashSet::new();
    for _depth in 0..COMPRESSION_MAX_CHAIN_DEPTH {
        if !visited.insert(current.clone()) {
            return Ok(None); // cycle defense
        }
        if let Some(mem) = get_by_id(ctx, storage, &current).await? {
            if let Some(successor) = find_compressor_of(ctx, storage, &current).await? {
                current = successor;
                continue;
            }
            return Ok(Some(mem));
        }
        match find_compressor_of(ctx, storage, &current).await? {
            Some(successor) => current = successor,
            None => return Ok(None),
        }
    }
    Ok(None)
}

/// Scan memories for one whose `derived_from` contains `target`.
/// Returns the FIRST such memory's id; subsequent matches are
/// silently ignored. O(N) per call.
async fn find_compressor_of(
    ctx: &Context,
    storage: &dyn Storage,
    target: &MemoryId,
) -> Result<Option<MemoryId>, EngineError> {
    let prefix = StorageKey::memories_prefix(ctx);
    let keys = storage.list(&prefix).await?;
    for key in keys {
        if !key.as_str().ends_with(".md") {
            continue;
        }
        let bytes = match storage.get(&key).await? {
            Some(b) => b,
            None => continue,
        };
        let (fm, _body) = match parse_memory_file(&bytes) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if fm.derived_from.contains(target) {
            return Ok(Some(fm.id));
        }
    }
    Ok(None)
}

/// One-pass predecessor→compressor reverse index for the recompute
/// pass (Phase E2 audit M2 fix: previously each chase from recompute
/// did its own O(N) scan, giving O(L·E·D·N) total — pathological on
/// large stores). Built once before the lesson scan; chase below
/// walks the in-memory map instead of re-scanning.
///
/// Returns `(map, mem_ids)` — the map plus the full set of memory
/// ids for the "does this memory exist at all" check needed by C2.
async fn build_predecessor_index(
    ctx: &Context,
    storage: &dyn Storage,
) -> Result<
    (
        HashMap<MemoryId, MemoryId>,
        std::collections::HashSet<MemoryId>,
    ),
    EngineError,
> {
    let prefix = StorageKey::memories_prefix(ctx);
    let keys = storage.list(&prefix).await?;
    let mut map: HashMap<MemoryId, MemoryId> = HashMap::new();
    let mut all_ids: std::collections::HashSet<MemoryId> = std::collections::HashSet::new();
    for key in keys {
        if !key.as_str().ends_with(".md") {
            continue;
        }
        let bytes = match storage.get(&key).await? {
            Some(b) => b,
            None => continue,
        };
        let (fm, _body) = match parse_memory_file(&bytes) {
            Ok(p) => p,
            Err(_) => continue,
        };
        all_ids.insert(fm.id.clone());
        for predecessor in &fm.derived_from {
            // First-write-wins: ambiguous predecessor (two
            // compressors share a predecessor) keeps the first
            // encountered. Documented as host concern.
            map.entry(predecessor.clone())
                .or_insert_with(|| fm.id.clone());
        }
    }
    Ok((map, all_ids))
}

/// Chase using the pre-built index. Walks forward through
/// `predecessor → compressor` hops in `map` up to depth-cap. Returns
/// the leaf id (the latest still-alive compressor in the chain) OR
/// `None` if the citation has been orphaned (nothing in the chain
/// exists in `all_ids`).
fn chase_via_index(
    map: &HashMap<MemoryId, MemoryId>,
    all_ids: &std::collections::HashSet<MemoryId>,
    target: &MemoryId,
) -> Option<MemoryId> {
    let mut current = target.clone();
    let mut visited: std::collections::HashSet<MemoryId> = std::collections::HashSet::new();
    for _depth in 0..COMPRESSION_MAX_CHAIN_DEPTH {
        if !visited.insert(current.clone()) {
            return None; // cycle defense
        }
        // If `current` exists AND is the leaf (no compressor for
        // it), return it.
        if all_ids.contains(&current) && !map.contains_key(&current) {
            return Some(current);
        }
        // Walk forward.
        match map.get(&current) {
            Some(successor) => current = successor.clone(),
            None => {
                // No successor for `current`. If `current` exists,
                // it's the leaf. Otherwise orphaned.
                return if all_ids.contains(&current) {
                    Some(current)
                } else {
                    None
                };
            }
        }
    }
    None
}

/// Phase E2 D-Cx7 audit C2 fix: walks all lessons, recomputes
/// citation counters using the predecessor→compressor index so
/// citations through compressed chains resolve correctly. Surfaces
/// orphaned citations (cited memory + chase both gone) via the new
/// `RecomputeStats::orphan_citations` counter — these don't cause
/// the function to fail, but the host can act on them.
pub async fn recompute_citation_counts(
    ctx: &Context,
    storage: &dyn Storage,
) -> Result<RecomputeStats, EngineError> {
    let mut stats = RecomputeStats {
        lessons_scanned: 0,
        memories_recomputed: 0,
        counters_repaired: 0,
        orphan_citations: 0,
    };

    // 1. Build the reverse-index ONCE (M2 fix).
    let (index, all_mem_ids) = build_predecessor_index(ctx, storage).await?;

    // 2. Walk all lessons, accumulate canonical citation counts.
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
            let split = match crate::engine::yaml::split_frontmatter_normalized(content) {
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
            // User-authored OR pack-authored lessons drive immunity.
            // Pack-authored = codex-seeded; user-installing the codex is
            // itself an act of user authorship.
            if !fm.authored_by.is_immune() {
                continue;
            }
            if let Some(cn) = &fm.causal_narrative {
                for evr in &cn.evidence_refs {
                    if let Some(mid) = evr.as_memory_id() {
                        match chase_via_index(&index, &all_mem_ids, mid) {
                            Some(canonical) => {
                                *counts.entry(canonical).or_insert(0) += 1;
                            }
                            None => {
                                // Phase E2 audit C2: orphan citation.
                                // Lesson cites a memory that no
                                // longer exists AND no compressor
                                // contains it. Audit signal.
                                stats.orphan_citations += 1;
                                warn!(
                                    lesson = %fm.id, memory = %mid,
                                    "recompute: ORPHAN citation — lesson cites memory \
                                     with no live successor in derived_from chain"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // 3. Walk all memories. Repair counter drift.
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

/// CAS-RMW counter rewrite. `pub(crate)` for compress.rs internal use.
pub(crate) async fn set_citation_count(
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
            return Ok(());
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

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
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
    /// Phase E2 audit C2: lessons that cited a memory id where
    /// BOTH the memory itself AND its forward `derived_from`
    /// successors are gone. The citation is unresolvable — audit
    /// trail integrity is compromised. Host should investigate
    /// (recompute does NOT fail on orphans; it counts them).
    pub orphan_citations: usize,
}
