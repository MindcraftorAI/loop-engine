//! Phase E2 — memory compression.
//!
//! `compress(window)` takes a set of raw memories (specified by ids
//! or a predicate), invokes the supplied [`LlmClient`] to summarize
//! them, embeds the resulting summary via the supplied [`Embedder`],
//! and persists a NEW memory whose `derived_from` field carries the
//! predecessor ids. Citation counts are transferred (summed) from
//! the predecessors so the user-immunity invariant is preserved
//! across the compress → predecessor-delete boundary.
//!
//! Mirrors `narrative::generate` design template: engine-owned prompt
//! template, `ResponseFormat::JsonSchema`, refusal-vs-validation
//! discrimination (Phase D A-M4 lesson), parse-time validation.
//!
//! Predecessor lifecycle (D-Cx4): `compress` DOES NOT delete the
//! predecessors. The host explicitly calls
//! `memory::delete(predecessor, force=true)` on its own schedule
//! once Mc is verified intact. This preserves rollback safety AND
//! keeps the immunity invariant safe across the compress-delete
//! window — the citation lives on BOTH Mi and Mc during the
//! transition; orphaning is impossible.

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use crate::engine::context::Context;
use crate::engine::embedding::Embedder;
use crate::engine::error::EngineError;
use crate::engine::llm::{GenerateRequest, LlmClient, LlmError, ResponseFormat};
use crate::engine::memory::id::MemoryId;
use crate::engine::memory::store::{
    embedding_to_bytes, get_by_id, parse_memory_file, render_memory_yaml, vec_key,
};
use crate::engine::memory::{Memory, MemoryFrontmatter, MemoryQuery, PrunePredicate};
use crate::engine::storage::{Storage, StorageKey};
use crate::engine::vector::VectorIndex;

/// Per-field caps for parse-time validation (D-Cx7).
const MAX_DESCRIPTION_CHARS: usize = 200;
const MAX_CONTENT_CHARS: usize = 4_000;
const MAX_DERIVED_FROM_LEN: usize = 64;

/// Maximum `derived_from` chain depth allowed when compress-validating
/// (D-Cx8). Walks BACK through every predecessor's `derived_from`;
/// any path exceeding this depth (or revisiting an id) trips the
/// cycle detector.
#[allow(dead_code)] // Cx2 cycle::detect_cycle_in_window consumes
pub(crate) const COMPRESSION_MAX_CHAIN_DEPTH: usize = 16;

/// Configuration knobs for [`compress`]. Defaults match the locked
/// D-Cx7 values; `#[non_exhaustive]` so future cycles can add knobs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CompressionConfig {
    /// Max LLM output tokens. Default 2048.
    pub max_tokens: usize,
    /// Sampling temperature. Default 0.0 — structured output should
    /// be deterministic.
    pub temperature: f32,
    /// Optional system-prompt override.
    pub system_override: Option<String>,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            max_tokens: 2048,
            temperature: 0.0,
            system_override: None,
        }
    }
}

/// Caller's way of specifying which memories to compress.
/// `#[non_exhaustive]` — future variants (e.g. Cluster) land additively.
///
/// Not `Debug` / `Clone` because `PrunePredicate` is a boxed `dyn Fn`
/// without either. Callers wanting to inspect / clone construct
/// `Ids` variant from a `Vec<MemoryId>`.
#[non_exhaustive]
pub enum CompressionWindow {
    /// Explicit set of memory ids. Caller already decided.
    Ids(Vec<MemoryId>),
    /// Engine walks `memories/`, collects ids whose frontmatter
    /// matches `predicate`.
    Predicate(PrunePredicate),
}

/// Engine-side compression prompt. Loaded once as `const &'static
/// str` (S149 pattern). The wedge invariants are baked into both
/// the prompt rules + the parse-time validator (D-D10 defense-in-
/// depth from Phase D).
const COMPRESSION_PROMPT_TEMPLATE: &str = "You are compressing a window of MEMORIES into ONE summary memory. The summary will replace the originals in the long-tail memory store.

Inputs (each item is one memory, `--- MEMORY <id> ---` separator):
{MEMORIES_BLOCK}

Rules:
- Preserve key facts, decisions, references, names, paths. Drop ephemera (small talk, exact timestamps, redundant repetitions).
- Do NOT invent facts not present in the inputs.
- Do NOT use praise words (\"great\", \"excellent\", \"successfully\") — sycophancy markers.
- description: a single-sentence summary, \u{2264}200 chars, no period at end.
- content: the compressed body. Multi-paragraph allowed. \u{2264}4000 chars.
- If the input memories are too thin / contradictory / off-topic to compose a coherent summary, return {\"error\": \"insufficient_input\"} instead.

Output as JSON matching one of:
  Success: {\"description\": \"...\", \"content\": \"...\"}
  Refusal: {\"error\": \"insufficient_input\"}";

/// LLM output shape — parsed AFTER explicit refusal-key check
/// (Phase D A-M4 audit lesson: NO `serde(untagged)`).
#[derive(Debug, Deserialize)]
struct CompressedDraft {
    description: String,
    content: String,
}

/// Compress a window of memories. Pure async function. Side effect:
/// writes the new compressed memory (md + vec sidecar + vector
/// index entry) to storage. Does NOT delete predecessors — host
/// triggers `delete(predecessor, force=true)` on its schedule
/// once Mc is verified intact (D-Cx4).
///
/// Errors:
/// - `EngineError::CompressionInsufficientInput` — LLM refused
///   (graceful no-op signal; not a defect).
/// - `EngineError::Llm(LlmError::InvalidOutput | ValidationFailed)`
///   — LLM defect.
/// - `EngineError::CompressionCycle` — `derived_from` chain among
///   the input window forms a cycle OR exceeds depth-16.
/// - `EngineError::Storage(_)` / `Parse(_)` — I/O failures.
#[allow(clippy::too_many_arguments)] // 8 args is fundamental to the operation
pub async fn compress(
    ctx: &Context,
    storage: &dyn Storage,
    llm: &dyn LlmClient,
    embedder: &dyn Embedder,
    vector_index: &dyn VectorIndex,
    window: CompressionWindow,
    config: &CompressionConfig,
    now: DateTime<Utc>,
) -> Result<Memory, EngineError> {
    // 1. Resolve window → Vec<MemoryId> + load each Memory.
    //    Phase E2 audit M1 fix: dedupe the resolved id set BEFORE
    //    loading so a window like `Ids(vec![M1, M1])` doesn't
    //    inflate `derived_from` AND double-count the citation
    //    transfer.
    let mut predecessor_ids = resolve_window(ctx, storage, window).await?;
    {
        let mut seen: std::collections::HashSet<MemoryId> = std::collections::HashSet::new();
        predecessor_ids.retain(|id| seen.insert(id.clone()));
    }
    if predecessor_ids.is_empty() {
        // No memories to compress → graceful refusal. Distinct from
        // LLM refusal; this is an "empty input" guard.
        return Err(EngineError::CompressionInsufficientInput);
    }
    let mut predecessors: Vec<Memory> = Vec::with_capacity(predecessor_ids.len());
    for id in &predecessor_ids {
        let mem = get_by_id(ctx, storage, id)
            .await?
            .ok_or_else(|| EngineError::Parse(format!("compress: predecessor {id} not found")))?;
        predecessors.push(mem);
    }

    // 2. Cycle + depth check across the input set. Walks each
    //    predecessor's `derived_from` chain (recursively) up to the
    //    depth cap. Detects cycles by tracking visited ids per walk.
    super::cycle::detect_cycle_in_window(ctx, storage, &predecessors).await?;

    // 2b. Phase F audit-fix close: scope-consistency check across
    //     the input set. Compressing across `MemoryScope` boundaries
    //     would violate the privacy invariant — the new compressed
    //     memory can't simultaneously be (e.g.) team-scoped AND
    //     skill-scoped. Refuse with `CompressionScopeMismatch`.
    {
        let mut iter = predecessors.iter();
        if let Some(first) = iter.next() {
            let first_scope = &first.frontmatter.scope;
            if !iter.all(|m| &m.frontmatter.scope == first_scope) {
                return Err(EngineError::CompressionScopeMismatch {
                    window: predecessors
                        .iter()
                        .map(|m| m.frontmatter.id.as_str().to_string())
                        .collect(),
                });
            }
        }
    }

    // 3. Build prompt + invoke LLM.
    let prompt = fill_template(&predecessors);
    let request = GenerateRequest {
        prompt,
        system: config.system_override.clone(),
        max_tokens: config.max_tokens,
        temperature: config.temperature,
        stop_sequences: Vec::new(),
        response_format: ResponseFormat::JsonSchema {
            schema: build_schema(),
            name: "CompressedDraftOrRefusal".to_string(),
        },
        model: None,
    };
    let generation = llm.generate(ctx, &request).await?;
    let parsed = generation.parsed.ok_or_else(|| {
        EngineError::from(LlmError::InvalidOutput(
            "compress: adapter produced no parsed output for JsonSchema request".into(),
        ))
    })?;
    let draft = discriminate_compress_output(parsed)?;
    validate_compressed_invariants(&draft, &predecessor_ids)?;

    // 4. Build new memory: mint id, sum citation counters from
    //    predecessors (saturating), stamp `derived_from`.
    let mc_id = mint_compressed_id(now, &predecessor_ids);
    let mut fm = MemoryFrontmatter::new(mc_id.clone(), draft.description, now);
    fm.derived_from = predecessor_ids;
    fm.consumed_by_user_lessons = predecessors
        .iter()
        .map(|m| m.frontmatter.consumed_by_user_lessons)
        .fold(0_u32, |acc, c| acc.saturating_add(c));

    // 5. Embed + persist (.md + .vec + vector index).
    let mut emb = embedder
        .embed(ctx, std::slice::from_ref(&draft.content))
        .await?;
    let embedding = emb
        .pop()
        .ok_or_else(|| EngineError::Parse("embedder returned zero vectors".into()))?;
    let yaml = render_memory_yaml(&fm, &draft.content)?;
    let md_key = StorageKey::memory(ctx, fm.id.as_str());
    storage.put(&md_key, Bytes::from(yaml)).await?;
    storage
        .put(
            &vec_key(ctx, &fm.id),
            Bytes::from(embedding_to_bytes(&embedding)),
        )
        .await?;
    vector_index.insert(ctx, &fm.id, &embedding).await?;

    Ok(Memory::new(fm, draft.content).with_embedding(embedding))
}

/// Default top-k for the recall-replay verification probe in
/// [`consolidate`]. The compressed memory must surface within the
/// top-`k` results of EACH predecessor's representative query for the
/// consolidation to count as verified.
pub const DEFAULT_CONSOLIDATE_RECALL_K: usize = 5;

/// Outcome of a [`consolidate`] call. Engine-owned shape; the serve
/// layer translates it to the JSON-RPC wire form.
///
/// `verified` is the load-bearing safety signal: when `false`, NO
/// predecessor was deleted (`deleted` is empty) — the new compressed
/// memory `mc_id` (if minted) sits ALONGSIDE every predecessor so the
/// trace is never lost. The host is expected to surface a drift event
/// on `!verified`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConsolidateOutcome {
    /// The minted compressed memory's id, if `compress` succeeded.
    /// `None` only when compression itself failed/refused (no Mc
    /// exists; nothing was deleted).
    pub mc_id: Option<String>,
    /// Predecessor ids that were force-deleted. Non-empty ONLY when
    /// `verified == true`. Excludes any user-cited (immune) predecessor.
    pub deleted: Vec<String>,
    /// Predecessor ids KEPT because they are user-cited
    /// (`consumed_by_user_lessons > 0`) — immunity is honored even on
    /// the verified path. The compression chain (`derived_from`)
    /// still links them to `Mc`.
    pub kept_immune: Vec<String>,
    /// Whether the recall-replay gate passed for ALL predecessors AND
    /// compression succeeded. `false` ⇒ fail-closed (nothing deleted).
    pub verified: bool,
}

/// Atomic, fail-closed consolidation: compress a window into one
/// summary `Mc`, VERIFY that `Mc` preserves each predecessor's recall
/// (recall-replay), and ONLY THEN force-delete the non-immune
/// predecessors. This is universal memory INTEGRITY — every consumer
/// of the substrate needs safe-compression, so it lives in the engine
/// (race-free + correct-by-construction) rather than in any single
/// host's policy layer.
///
/// The safety contract (the same D2 the host previously enforced,
/// now guaranteed inside the engine):
///   1. **compress** mints `Mc` (`derived_from = predecessors`, summed
///      citation counters). Predecessors are NOT touched yet.
///   2. **verify (recall-replay):** for EACH predecessor, run a
///      semantic search keyed on that predecessor's own description;
///      `Mc` MUST appear within the top-`recall_k` hits. A single miss
///      → `verified = false`.
///   3. **gated delete:** only when verified, force-delete each
///      predecessor whose `consumed_by_user_lessons == 0`. User-cited
///      predecessors are KEPT (immunity) even though `force` would
///      bypass the store guard — the consolidate fn does the immunity
///      check ITSELF before deleting, so `force=true` here never
///      orphans a user citation.
///
/// **Fail-closed:** any error (compress, search, storage) OR any
/// verify-miss results in deleting NOTHING. The returned outcome
/// carries the `Mc` id (if minted) so the host can surface a drift
/// event and keep `Mc` alongside the originals.
///
/// Returns `Err` only when compression itself fails for a reason the
/// host must distinguish (insufficient input, cycle, scope mismatch,
/// I/O). A verify-miss is NOT an error — it is a valid
/// `verified: false` outcome (the window simply isn't safe to
/// consolidate yet).
#[allow(clippy::too_many_arguments)] // mirrors `compress`; the deps are fundamental
pub async fn consolidate(
    ctx: &Context,
    storage: &dyn Storage,
    llm: &dyn LlmClient,
    embedder: &dyn Embedder,
    vector_index: &dyn VectorIndex,
    ids: Vec<MemoryId>,
    config: &CompressionConfig,
    recall_k: usize,
    now: DateTime<Utc>,
) -> Result<ConsolidateOutcome, EngineError> {
    // Capture + dedupe the predecessor id set up front. `compress`
    // dedupes internally, but we need the same canonical set for the
    // verify + delete phases so the three steps reason about identical
    // windows.
    let mut predecessor_ids: Vec<MemoryId> = ids;
    {
        let mut seen: std::collections::HashSet<MemoryId> = std::collections::HashSet::new();
        predecessor_ids.retain(|id| seen.insert(id.clone()));
    }

    // 1. Compress → mint Mc. Any compression error propagates (the host
    //    distinguishes insufficient-input / cycle / scope-mismatch);
    //    NOTHING is deleted because we haven't reached the delete phase.
    let mc = compress(
        ctx,
        storage,
        llm,
        embedder,
        vector_index,
        CompressionWindow::Ids(predecessor_ids.clone()),
        config,
        now,
    )
    .await?;
    let mc_id = mc.frontmatter.id.clone();

    // 2. Recall-replay verification. For each predecessor, the engine
    //    re-queries the store with that predecessor's representative
    //    text (its description). Mc must surface in the top-k — proof
    //    the compressed summary preserves the predecessor's recall
    //    BEFORE we delete the original. A storage/search error here is
    //    fail-closed: treat as a verify miss (delete nothing). NOTE:
    //    the predecessors still exist at this point (compress does not
    //    delete), so the query naturally returns the predecessor too;
    //    we only assert Mc's PRESENCE in the top-k, not its rank.
    //
    //    `recall_k == 0` means "Mc must be within the top-0", which is
    //    impossible to satisfy — short-circuit to fail-closed (the gate
    //    can never pass, and a 0 must not slip through the vector
    //    index's tombstone over-fetch which can return hits even for
    //    k=0 after earlier deletions left tombstones).
    let mut verified = recall_k > 0;
    for pid in &predecessor_ids {
        if !verified {
            break;
        }
        let pred = match get_by_id(ctx, storage, pid).await {
            Ok(Some(m)) => m,
            // Predecessor vanished or unreadable → cannot prove recall
            // is preserved → fail closed.
            Ok(None) | Err(_) => {
                verified = false;
                break;
            }
        };
        let query = MemoryQuery::Text(recall_query_for(&pred));
        let hits = match super::store::search(
            ctx,
            storage,
            embedder,
            vector_index,
            &query,
            recall_k,
            0, // body preview unused by the gate
            None,
        )
        .await
        {
            Ok(h) => h,
            Err(_) => {
                verified = false;
                break;
            }
        };
        if !hits.iter().any(|h| h.id == mc_id) {
            verified = false;
            break;
        }
    }

    // 3. Fail-closed: a verify miss deletes NOTHING. Mc lives alongside
    //    the predecessors; the host surfaces a drift event.
    if !verified {
        return Ok(ConsolidateOutcome {
            mc_id: Some(mc_id.as_str().to_string()),
            deleted: Vec::new(),
            kept_immune: Vec::new(),
            verified: false,
        });
    }

    // 4. Gated delete. Verified ⇒ force-delete each NON-immune
    //    predecessor. The immunity check is done HERE (not delegated to
    //    `delete`, which `force=true` would bypass): a predecessor with
    //    `consumed_by_user_lessons > 0` is KEPT. If any single delete
    //    errors, stop deleting further (the originals that remain are
    //    safe — Mc + derived_from still preserve the trace) and report
    //    what was deleted so far; the operation stays consistent.
    let mut deleted: Vec<String> = Vec::new();
    let mut kept_immune: Vec<String> = Vec::new();
    for pid in &predecessor_ids {
        // Re-load to read the AUTHORITATIVE immunity counter right
        // before the irreversible delete (don't trust a stale read).
        let immune = match get_by_id(ctx, storage, pid).await {
            Ok(Some(m)) => m.frontmatter.consumed_by_user_lessons > 0,
            // Already gone / unreadable → treat as immune-safe (skip):
            // never force-delete something we can't confirm is non-immune.
            Ok(None) | Err(_) => true,
        };
        if immune {
            kept_immune.push(pid.as_str().to_string());
            continue;
        }
        match super::store::delete(ctx, storage, vector_index, pid, true).await {
            Ok(()) => deleted.push(pid.as_str().to_string()),
            // A delete failure is non-fatal: keep the predecessor (it's
            // still safe; Mc preserves the trace) and record it as kept.
            Err(_) => kept_immune.push(pid.as_str().to_string()),
        }
    }

    Ok(ConsolidateOutcome {
        mc_id: Some(mc_id.as_str().to_string()),
        deleted,
        kept_immune,
        verified: true,
    })
}

/// Build the recall-replay query text for a predecessor. Uses the
/// predecessor's description (the most representative single-line
/// summary the store holds) — falls back to a body-prefix when the
/// description is empty so the probe is never an empty query.
fn recall_query_for(pred: &Memory) -> String {
    let desc = pred.frontmatter.description.trim();
    if !desc.is_empty() {
        return desc.to_string();
    }
    pred.content.trim().chars().take(200).collect()
}

async fn resolve_window(
    ctx: &Context,
    storage: &dyn Storage,
    window: CompressionWindow,
) -> Result<Vec<MemoryId>, EngineError> {
    match window {
        CompressionWindow::Ids(ids) => Ok(ids),
        CompressionWindow::Predicate(predicate) => {
            let mut out: Vec<MemoryId> = Vec::new();
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
                    Err(e) => {
                        warn!(
                            key = %key, error = %e,
                            "compress: skipping unparseable memory during window resolution"
                        );
                        continue;
                    }
                };
                if predicate(&fm) {
                    out.push(fm.id);
                }
            }
            Ok(out)
        }
    }
}

fn fill_template(predecessors: &[Memory]) -> String {
    let mut block = String::new();
    for mem in predecessors {
        block.push_str(&format!("--- MEMORY {} ---\n", mem.frontmatter.id));
        block.push_str("description: ");
        block.push_str(&mem.frontmatter.description);
        block.push('\n');
        block.push_str("content:\n");
        block.push_str(mem.content.trim());
        block.push_str("\n\n");
    }
    COMPRESSION_PROMPT_TEMPLATE.replace("{MEMORIES_BLOCK}", block.trim())
}

fn build_schema() -> Value {
    json!({
        "type": "object",
        "oneOf": [
            {
                "type": "object",
                "required": ["description", "content"],
                "properties": {
                    "description": { "type": "string" },
                    "content": { "type": "string" }
                }
            },
            {
                "type": "object",
                "required": ["error"],
                "properties": { "error": { "type": "string" } }
            }
        ]
    })
}

/// Explicit refusal-key check BEFORE attempting CompressedDraft
/// parse (Phase D audit A-M4 pattern).
fn discriminate_compress_output(parsed: Value) -> Result<CompressedDraft, EngineError> {
    if parsed.get("error").is_some() {
        return Err(EngineError::CompressionInsufficientInput);
    }
    serde_json::from_value::<CompressedDraft>(parsed)
        .map_err(|e| EngineError::from(LlmError::InvalidOutput(format!("compress parse: {e}"))))
}

/// D-Cx7 parse-time validation. Char-count (not bytes) per S141.
fn validate_compressed_invariants(
    draft: &CompressedDraft,
    predecessor_ids: &[MemoryId],
) -> Result<(), EngineError> {
    if draft.description.trim().is_empty() {
        return Err(EngineError::from(LlmError::ValidationFailed(
            "description empty after trim".into(),
        )));
    }
    if draft.content.trim().is_empty() {
        return Err(EngineError::from(LlmError::ValidationFailed(
            "content empty after trim".into(),
        )));
    }
    let desc_chars = draft.description.chars().count();
    if desc_chars > MAX_DESCRIPTION_CHARS {
        return Err(EngineError::from(LlmError::ValidationFailed(format!(
            "description length {desc_chars} > cap {MAX_DESCRIPTION_CHARS}"
        ))));
    }
    let content_chars = draft.content.chars().count();
    if content_chars > MAX_CONTENT_CHARS {
        return Err(EngineError::from(LlmError::ValidationFailed(format!(
            "content length {content_chars} > cap {MAX_CONTENT_CHARS}"
        ))));
    }
    if predecessor_ids.is_empty() {
        return Err(EngineError::from(LlmError::ValidationFailed(
            "derived_from cannot be empty for compressed memory".into(),
        )));
    }
    if predecessor_ids.len() > MAX_DERIVED_FROM_LEN {
        return Err(EngineError::from(LlmError::ValidationFailed(format!(
            "derived_from len {} > cap {}",
            predecessor_ids.len(),
            MAX_DERIVED_FROM_LEN
        ))));
    }
    Ok(())
}

/// Mint a `MemoryId` for the new compressed memory. Format:
/// `mem-c-<16-hex-of-(timestamp-millis ⊕ DefaultHasher(predecessors))>`.
/// The `-c-` infix marks it as compressed (purely cosmetic; the
/// engine identifies via `derived_from.is_empty()`).
///
/// Hashing the predecessor ids prevents collisions when two
/// compressions are issued at the same wall-clock millisecond (which
/// happens trivially in tests using a fixed `now`, and is plausible
/// in production under burst host-driven compression).
fn mint_compressed_id(now: DateTime<Utc>, predecessors: &[MemoryId]) -> MemoryId {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    for id in predecessors {
        id.as_str().hash(&mut hasher);
    }
    let pred_hash = hasher.finish();
    let ms = now.timestamp_millis() as u64;
    let composite = ms.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(pred_hash);
    MemoryId::new(format!("mem-c-{composite:016x}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::Context;
    use crate::engine::embedding::MockEmbedder;
    use crate::engine::llm::{Generation, MockLlmClient};
    use crate::engine::memory::store::insert;
    use crate::engine::storage::MemoryStorage;
    use crate::engine::vector::HnswVectorIndex;
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

    fn success_generation(json_str: &str) -> Generation {
        Generation::new(json_str).with_parsed(serde_json::from_str(json_str).unwrap())
    }

    async fn seed_memories(
        storage: &Arc<dyn Storage>,
        vector_index: &HnswVectorIndex,
        descriptions: &[(&str, &str, &str)],
    ) -> Vec<MemoryId> {
        let mut ids = Vec::new();
        for (id_str, desc, body) in descriptions {
            let id = MemoryId::new(*id_str);
            let emb = MockEmbedder::new(4).with_response(vec![unit_vec(4, 0)]);
            insert(
                &ctx(),
                storage.as_ref(),
                &emb,
                vector_index,
                id.clone(),
                *desc,
                *body,
                now_t(),
            )
            .await
            .unwrap();
            ids.push(id);
        }
        ids
    }

    #[tokio::test]
    async fn compress_happy_path_creates_derived_memory() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        let ids = seed_memories(
            &storage,
            &vector_index,
            &[
                ("mem-raw00001", "first raw", "first body content"),
                ("mem-raw00002", "second raw", "second body content"),
            ],
        )
        .await;
        let llm = MockLlmClient::default().with_response(success_generation(
            r#"{"description": "summary of two memories", "content": "compressed body"}"#,
        ));
        let embedder = MockEmbedder::new(4).with_response(vec![unit_vec(4, 1)]);
        let mc = compress(
            &ctx(),
            storage.as_ref(),
            &llm,
            &embedder,
            &vector_index,
            CompressionWindow::Ids(ids.clone()),
            &CompressionConfig::default(),
            now_t(),
        )
        .await
        .unwrap();
        assert_eq!(mc.frontmatter.description, "summary of two memories");
        assert_eq!(mc.content, "compressed body");
        assert_eq!(mc.frontmatter.derived_from, ids);
        assert!(mc.is_compressed());
        assert!(mc.embedding.is_some());
        // Persisted under its minted id.
        assert!(
            crate::engine::memory::store::get_by_id(&ctx(), storage.as_ref(), &mc.frontmatter.id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn compress_sums_citation_counters_from_predecessors() {
        use crate::engine::memory::store::increment_citation_count;
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        let ids = seed_memories(
            &storage,
            &vector_index,
            &[
                ("mem-cit00001", "a", "body a"),
                ("mem-cit00002", "b", "body b"),
            ],
        )
        .await;
        // Counter on M1 = 2, M2 = 3 → Mc should be 5.
        for _ in 0..2 {
            increment_citation_count(&ctx(), storage.as_ref(), &ids[0])
                .await
                .unwrap();
        }
        for _ in 0..3 {
            increment_citation_count(&ctx(), storage.as_ref(), &ids[1])
                .await
                .unwrap();
        }
        let llm = MockLlmClient::default().with_response(success_generation(
            r#"{"description": "s", "content": "c"}"#,
        ));
        let embedder = MockEmbedder::new(4).with_response(vec![unit_vec(4, 1)]);
        let mc = compress(
            &ctx(),
            storage.as_ref(),
            &llm,
            &embedder,
            &vector_index,
            CompressionWindow::Ids(ids),
            &CompressionConfig::default(),
            now_t(),
        )
        .await
        .unwrap();
        assert_eq!(mc.frontmatter.consumed_by_user_lessons, 5);
    }

    #[tokio::test]
    async fn compress_refusal_sentinel_surfaces_as_insufficient_input() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        let ids = seed_memories(&storage, &vector_index, &[("mem-thn00001", "x", "y")]).await;
        let llm = MockLlmClient::default()
            .with_response(success_generation(r#"{"error": "insufficient_input"}"#));
        let embedder = MockEmbedder::new(4);
        let r = compress(
            &ctx(),
            storage.as_ref(),
            &llm,
            &embedder,
            &vector_index,
            CompressionWindow::Ids(ids),
            &CompressionConfig::default(),
            now_t(),
        )
        .await;
        assert!(matches!(r, Err(EngineError::CompressionInsufficientInput)));
    }

    #[tokio::test]
    async fn compress_mixed_shape_with_error_treated_as_refusal() {
        // Phase D A-M4 cross-phase regression — explicit error-key
        // check wins over coincidentally-valid fields.
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        let ids = seed_memories(&storage, &vector_index, &[("mem-mxd00001", "x", "y")]).await;
        let llm = MockLlmClient::default().with_response(success_generation(
            r#"{"error": "insufficient_input", "description": "fake", "content": "also fake"}"#,
        ));
        let embedder = MockEmbedder::new(4);
        let r = compress(
            &ctx(),
            storage.as_ref(),
            &llm,
            &embedder,
            &vector_index,
            CompressionWindow::Ids(ids),
            &CompressionConfig::default(),
            now_t(),
        )
        .await;
        assert!(matches!(r, Err(EngineError::CompressionInsufficientInput)));
    }

    #[tokio::test]
    async fn compress_empty_window_refuses_gracefully() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        let llm = MockLlmClient::default();
        let embedder = MockEmbedder::new(4);
        let r = compress(
            &ctx(),
            storage.as_ref(),
            &llm,
            &embedder,
            &vector_index,
            CompressionWindow::Ids(vec![]),
            &CompressionConfig::default(),
            now_t(),
        )
        .await;
        assert!(matches!(r, Err(EngineError::CompressionInsufficientInput)));
    }

    #[tokio::test]
    async fn compress_rejects_too_long_description() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        let ids = seed_memories(&storage, &vector_index, &[("mem-vld00001", "x", "y")]).await;
        let over_cap = "x".repeat(201);
        let json = format!(r#"{{"description": "{over_cap}", "content": "c"}}"#);
        let llm = MockLlmClient::default().with_response(success_generation(&json));
        let embedder = MockEmbedder::new(4);
        let r = compress(
            &ctx(),
            storage.as_ref(),
            &llm,
            &embedder,
            &vector_index,
            CompressionWindow::Ids(ids),
            &CompressionConfig::default(),
            now_t(),
        )
        .await;
        match r {
            Err(EngineError::Llm(LlmError::ValidationFailed(msg))) => {
                assert!(msg.contains("description"), "{msg}");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn compress_with_predicate_window_picks_matching_memories() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        let _ids = seed_memories(
            &storage,
            &vector_index,
            &[
                ("mem-prd00001", "include-me-1", "a"),
                ("mem-prd00002", "include-me-2", "b"),
                ("mem-prd00003", "skip-me", "c"),
            ],
        )
        .await;
        let predicate: PrunePredicate = Box::new(|fm| fm.description.starts_with("include-me"));
        let llm = MockLlmClient::default().with_response(success_generation(
            r#"{"description": "s", "content": "c"}"#,
        ));
        let embedder = MockEmbedder::new(4).with_response(vec![unit_vec(4, 1)]);
        let mc = compress(
            &ctx(),
            storage.as_ref(),
            &llm,
            &embedder,
            &vector_index,
            CompressionWindow::Predicate(predicate),
            &CompressionConfig::default(),
            now_t(),
        )
        .await
        .unwrap();
        assert_eq!(mc.frontmatter.derived_from.len(), 2);
        assert!(
            mc.frontmatter
                .derived_from
                .iter()
                .all(|i| { i.as_str() == "mem-prd00001" || i.as_str() == "mem-prd00002" })
        );
    }

    #[tokio::test]
    async fn compress_predecessor_not_found_errors() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        let llm = MockLlmClient::default();
        let embedder = MockEmbedder::new(4);
        let r = compress(
            &ctx(),
            storage.as_ref(),
            &llm,
            &embedder,
            &vector_index,
            CompressionWindow::Ids(vec![MemoryId::new("mem-noexist1")]),
            &CompressionConfig::default(),
            now_t(),
        )
        .await;
        assert!(matches!(r, Err(EngineError::Parse(_))));
    }

    #[test]
    fn build_schema_has_oneof_with_two_options() {
        let s = build_schema();
        let one_of = s.get("oneOf").and_then(|v| v.as_array()).expect("oneOf");
        assert_eq!(one_of.len(), 2);
    }

    #[test]
    fn fill_template_substitutes_memories_block() {
        // Build a stub Memory by reaching through the public API.
        let fm = MemoryFrontmatter::new(MemoryId::new("mem-tpl00001"), "desc", now_t());
        let mem = Memory::new(fm, "body content");
        let p = fill_template(&[mem]);
        assert!(p.contains("--- MEMORY mem-tpl00001 ---"));
        assert!(p.contains("description: desc"));
        assert!(p.contains("body content"));
        assert!(!p.contains("{MEMORIES_BLOCK}"));
    }

    #[test]
    fn mint_compressed_id_format() {
        let id = mint_compressed_id(now_t(), &[]);
        assert!(id.as_str().starts_with("mem-c-"));
        assert_eq!(id.as_str().len(), "mem-c-".len() + 16);
    }

    #[test]
    fn mint_compressed_id_differs_for_different_predecessor_sets() {
        // Same `now`, different predecessor sets → different ids
        // (avoids collisions in recursive/burst compression).
        let id_a = mint_compressed_id(now_t(), &[MemoryId::new("mem-a")]);
        let id_b = mint_compressed_id(now_t(), &[MemoryId::new("mem-b")]);
        assert_ne!(
            id_a, id_b,
            "minted ids must differ when predecessor sets differ"
        );
    }

    // -----------------------------------------------------------------
    // `consolidate` — the atomic verify+gated-delete op. These tests
    // own the embedding vectors end-to-end so the recall-replay gate is
    // deterministic: a query vector colinear with Mc's vector surfaces
    // Mc (verify passes); an orthogonal Mc with k=1 hides it (verify
    // fails). The store immunity counter drives the keep-vs-delete fork.
    // -----------------------------------------------------------------

    /// Seed a memory at a CHOSEN embedding vector (each insert uses its
    /// own one-shot embedder so the vector lands in the index exactly).
    async fn seed_at(
        storage: &Arc<dyn Storage>,
        vector_index: &HnswVectorIndex,
        id: &str,
        desc: &str,
        body: &str,
        vec: Vec<f32>,
    ) -> MemoryId {
        let id = MemoryId::new(id);
        let emb = MockEmbedder::new(4).with_response(vec![vec]);
        insert(
            &ctx(),
            storage.as_ref(),
            &emb,
            vector_index,
            id.clone(),
            desc,
            body,
            now_t(),
        )
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn consolidate_verified_deletes_non_immune_predecessors() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        // Two non-immune predecessors, both at axis-0.
        let m1 = seed_at(
            &storage,
            &vector_index,
            "mem-con00001",
            "alpha topic",
            "body a",
            unit_vec(4, 0),
        )
        .await;
        let m2 = seed_at(
            &storage,
            &vector_index,
            "mem-con00002",
            "beta topic",
            "body b",
            unit_vec(4, 0),
        )
        .await;

        let llm = MockLlmClient::default().with_response(success_generation(
            r#"{"description": "merged alpha+beta", "content": "compressed body"}"#,
        ));
        // consolidate's embedder: 1st embed = Mc content (axis-0, so it
        // sits with the predecessors); then 1 embed per predecessor's
        // recall query (axis-0). All colinear → Mc surfaces → verified.
        let embedder = MockEmbedder::new(4)
            .with_response(vec![unit_vec(4, 0)]) // Mc
            .with_response(vec![unit_vec(4, 0)]) // m1 recall query
            .with_response(vec![unit_vec(4, 0)]); // m2 recall query

        let out = consolidate(
            &ctx(),
            storage.as_ref(),
            &llm,
            &embedder,
            &vector_index,
            vec![m1.clone(), m2.clone()],
            &CompressionConfig::default(),
            DEFAULT_CONSOLIDATE_RECALL_K,
            now_t(),
        )
        .await
        .unwrap();

        assert!(out.verified, "all-colinear recall must verify");
        assert!(out.mc_id.is_some());
        let mut deleted = out.deleted.clone();
        deleted.sort();
        assert_eq!(
            deleted,
            vec!["mem-con00001".to_string(), "mem-con00002".to_string()]
        );
        assert!(out.kept_immune.is_empty());
        // Predecessors actually gone; Mc present.
        assert!(
            get_by_id(&ctx(), storage.as_ref(), &m1)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            get_by_id(&ctx(), storage.as_ref(), &m2)
                .await
                .unwrap()
                .is_none()
        );
        let mc = get_by_id(&ctx(), storage.as_ref(), &MemoryId::new(out.mc_id.unwrap()))
            .await
            .unwrap();
        assert!(mc.is_some(), "Mc must survive");
    }

    #[tokio::test]
    async fn consolidate_keeps_user_cited_predecessor_even_when_verified() {
        use crate::engine::memory::store::increment_citation_count;
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        let m1 = seed_at(
            &storage,
            &vector_index,
            "mem-imm00001",
            "cited topic",
            "body a",
            unit_vec(4, 0),
        )
        .await;
        let m2 = seed_at(
            &storage,
            &vector_index,
            "mem-imm00002",
            "uncited topic",
            "body b",
            unit_vec(4, 0),
        )
        .await;
        // m1 is user-cited → immune; m2 is not.
        increment_citation_count(&ctx(), storage.as_ref(), &m1)
            .await
            .unwrap();

        let llm = MockLlmClient::default().with_response(success_generation(
            r#"{"description": "merged", "content": "compressed"}"#,
        ));
        let embedder = MockEmbedder::new(4)
            .with_response(vec![unit_vec(4, 0)]) // Mc
            .with_response(vec![unit_vec(4, 0)]) // m1 recall query
            .with_response(vec![unit_vec(4, 0)]); // m2 recall query

        let out = consolidate(
            &ctx(),
            storage.as_ref(),
            &llm,
            &embedder,
            &vector_index,
            vec![m1.clone(), m2.clone()],
            &CompressionConfig::default(),
            DEFAULT_CONSOLIDATE_RECALL_K,
            now_t(),
        )
        .await
        .unwrap();

        assert!(out.verified);
        assert_eq!(
            out.deleted,
            vec!["mem-imm00002".to_string()],
            "only non-immune deleted"
        );
        assert_eq!(
            out.kept_immune,
            vec!["mem-imm00001".to_string()],
            "user-cited kept"
        );
        // Immune predecessor still present; non-immune gone.
        assert!(
            get_by_id(&ctx(), storage.as_ref(), &m1)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            get_by_id(&ctx(), storage.as_ref(), &m2)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn consolidate_recall_replay_fail_deletes_nothing() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        // Predecessors at axis-0.
        let m1 = seed_at(
            &storage,
            &vector_index,
            "mem-fal00001",
            "alpha",
            "body a",
            unit_vec(4, 0),
        )
        .await;
        let m2 = seed_at(
            &storage,
            &vector_index,
            "mem-fal00002",
            "beta",
            "body b",
            unit_vec(4, 0),
        )
        .await;

        let llm = MockLlmClient::default().with_response(success_generation(
            r#"{"description": "merged", "content": "compressed"}"#,
        ));
        // Mc embeds ORTHOGONAL (axis-1); recall query is axis-0. With
        // k=1 the top hit is a predecessor (cosine 1.0), Mc (cosine 0.0)
        // is excluded → verify miss → fail closed.
        let embedder = MockEmbedder::new(4)
            .with_response(vec![unit_vec(4, 1)]) // Mc orthogonal
            .with_response(vec![unit_vec(4, 0)]); // m1 recall query (loop breaks on first miss)

        let out = consolidate(
            &ctx(),
            storage.as_ref(),
            &llm,
            &embedder,
            &vector_index,
            vec![m1.clone(), m2.clone()],
            &CompressionConfig::default(),
            1, // recall_k = 1 so Mc must be the TOP hit to pass
            now_t(),
        )
        .await
        .unwrap();

        assert!(!out.verified, "orthogonal Mc must fail recall-replay");
        assert!(out.deleted.is_empty(), "fail-closed: delete NOTHING");
        assert!(out.kept_immune.is_empty());
        assert!(
            out.mc_id.is_some(),
            "Mc still minted (kept alongside predecessors)"
        );
        // Both predecessors intact.
        assert!(
            get_by_id(&ctx(), storage.as_ref(), &m1)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            get_by_id(&ctx(), storage.as_ref(), &m2)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn consolidate_recall_k_zero_is_fail_closed() {
        // recall_k = 0 can never verify (Mc in top-0 is impossible) →
        // fail-closed regardless of how recall would rank. Guards the
        // vector-index tombstone over-fetch quirk from leaking a pass.
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        let m1 = seed_at(
            &storage,
            &vector_index,
            "mem-zk000001",
            "alpha",
            "a",
            unit_vec(4, 0),
        )
        .await;
        let llm = MockLlmClient::default().with_response(success_generation(
            r#"{"description": "merged", "content": "compressed"}"#,
        ));
        let embedder = MockEmbedder::new(4)
            .with_response(vec![unit_vec(4, 0)]) // Mc colinear (would pass at k>=1)
            .with_response(vec![unit_vec(4, 0)]); // recall query (never consulted)
        let out = consolidate(
            &ctx(),
            storage.as_ref(),
            &llm,
            &embedder,
            &vector_index,
            vec![m1.clone()],
            &CompressionConfig::default(),
            0, // recall_k = 0
            now_t(),
        )
        .await
        .unwrap();
        assert!(!out.verified, "k=0 must fail closed");
        assert!(out.deleted.is_empty());
        assert!(out.mc_id.is_some());
        assert!(
            get_by_id(&ctx(), storage.as_ref(), &m1)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn consolidate_compress_refusal_propagates_no_delete() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        let m1 = seed_at(
            &storage,
            &vector_index,
            "mem-ref00001",
            "x",
            "y",
            unit_vec(4, 0),
        )
        .await;
        // LLM refuses → CompressionInsufficientInput propagates; nothing
        // is minted or deleted.
        let llm = MockLlmClient::default()
            .with_response(success_generation(r#"{"error": "insufficient_input"}"#));
        let embedder = MockEmbedder::new(4);
        let r = consolidate(
            &ctx(),
            storage.as_ref(),
            &llm,
            &embedder,
            &vector_index,
            vec![m1.clone()],
            &CompressionConfig::default(),
            DEFAULT_CONSOLIDATE_RECALL_K,
            now_t(),
        )
        .await;
        assert!(matches!(r, Err(EngineError::CompressionInsufficientInput)));
        // Predecessor untouched.
        assert!(
            get_by_id(&ctx(), storage.as_ref(), &m1)
                .await
                .unwrap()
                .is_some()
        );
    }
}
