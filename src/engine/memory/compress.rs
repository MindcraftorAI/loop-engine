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
use serde_json::{json, Value};
use tracing::warn;

use crate::engine::context::Context;
use crate::engine::embedding::Embedder;
use crate::engine::error::EngineError;
use crate::engine::llm::{GenerateRequest, LlmClient, LlmError, ResponseFormat};
use crate::engine::memory::id::MemoryId;
use crate::engine::memory::store::{
    embedding_to_bytes, get_by_id, parse_memory_file, render_memory_yaml, vec_key,
};
use crate::engine::memory::{Memory, MemoryFrontmatter, PrunePredicate};
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
        assert!(crate::engine::memory::store::get_by_id(
            &ctx(),
            storage.as_ref(),
            &mc.frontmatter.id
        )
        .await
        .unwrap()
        .is_some());
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
        assert!(mc
            .frontmatter
            .derived_from
            .iter()
            .all(|i| { i.as_str() == "mem-prd00001" || i.as_str() == "mem-prd00002" }));
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
}
