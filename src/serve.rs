//! JSON-RPC 2.0 over stdio — programmatic engine access for host
//! adapters (opensquid MCP server, future TS/Python launchers).
//!
//! Protocol: line-delimited JSON-RPC 2.0 on stdin/stdout. Diagnostics
//! go to stderr. One Tokio multi-thread runtime drives the whole
//! session; engine state (Context + Storage) is initialized once at
//! startup and shared across all requests.
//!
//! Methods (v1):
//! - `ping`              — health check; returns `{ok: true}`
//! - `lesson.create`     — write a new lesson at `pending/<id>.md`
//! - `lesson.recall`     — text-match search across lessons
//! - `lesson.promote`    — gate-check + transition to `promoted/`
//! - `lesson.discard`    — transition to `discarded/` (immunity-respecting)
//! - `memory.create`     — embed + persist a raw memory (accepts optional `scope`, `origin`)
//! - `memory.search`     — semantic recall (accepts `include_body`, `scope_filter`)
//! - `memory.get`        — fetch a memory by id (returns FULL content + scope + origin)
//! - `memory.update`     — mutate description/content/scope; re-embeds on content change
//! - `memory.delete`     — `forget` (force=true required to bypass user-immunity)
//!
//! Manifest assembly + skill/persona/team ops land in a follow-on
//! serve cycle.

use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use bytes::Bytes;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::warn;

use crate::engine::context::Context;
use crate::engine::embedding::{Embedder, OpenAiCompatibleEmbedder};
use crate::engine::error::EngineError;
use crate::engine::lessons::gate::PromotionConfig;
use crate::engine::lessons::loader::get_by_id as load_lesson;
use crate::engine::lessons::transitions::{
    capture_feedback as transitions_capture_feedback, discard, promote,
    supersede as transitions_supersede, FeedbackPolarity,
};
use crate::engine::memory::{
    delete as memory_delete, get_by_id as memory_get_by_id, hybrid_search as memory_hybrid_search,
    insert_with_provenance as memory_insert_with_provenance, rehydrate_vector_index,
    search as memory_search, text_search as memory_text_search, update as memory_update, MemoryId,
    MemoryOrigin, MemoryQuery, MemoryScope, MemoryScopeFilter,
};
use crate::engine::paths;
use crate::engine::scoring::score_text_match;
use crate::engine::storage::filesystem::LocalFsStorage;
use crate::engine::storage::{Storage, StorageKey};
use crate::engine::vector::{HnswVectorIndex, VectorIndex};
use crate::engine::yaml::writer::serialize_lesson_frontmatter;
use crate::engine::yaml::{combine_frontmatter, split_frontmatter_normalized};
use crate::engine::yaml::{
    reader::parse_lesson_frontmatter, Authorship, LessonFrontmatter, LessonStatus,
};

// ---- JSON-RPC wire types -------------------------------------------

#[derive(Deserialize)]
struct Request {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct Response {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Serialize)]
struct RpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

fn ok(id: Option<Value>, result: Value) -> Response {
    Response {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

fn err(id: Option<Value>, code: i32, message: impl Into<String>) -> Response {
    Response {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(RpcError {
            code,
            message: message.into(),
            data: None,
        }),
    }
}

fn err_with_data(
    id: Option<Value>,
    code: i32,
    message: impl Into<String>,
    data: Value,
) -> Response {
    Response {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(RpcError {
            code,
            message: message.into(),
            data: Some(data),
        }),
    }
}

// ---- Entry point ---------------------------------------------------

/// Run the serve loop. Returns when stdin EOFs (the parent closed the
/// pipe). Errors only on initialization failures; per-request errors
/// surface to the client via JSON-RPC error responses.
pub async fn run() -> Result<()> {
    let ctx = Context::single_user_local();
    let home = paths::loop_home().context("resolving loop_home")?;
    paths::ensure_loop_dirs().context("ensuring loop dirs")?;
    let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(home));

    // Embedder + vector index for memory ops. Defaults to local
    // Ollama running Qwen3-Embedding-4B per the architecture decision;
    // env vars override (see OpenAiCompatibleEmbedder::from_env).
    let embedder = OpenAiCompatibleEmbedder::from_env()
        .context("constructing embedder (OPENSQUID_EMBEDDER_* env)")?;
    let dims = embedder.dimensions();
    let embedder: Arc<dyn Embedder> = Arc::new(embedder);
    let vector_index: Arc<dyn VectorIndex> = Arc::new(HnswVectorIndex::new(dims));

    // Rehydrate the HNSW index from on-disk `.vec` sidecars. The
    // index is in-memory; without this step, memories persisted by
    // a previous engine session remain on disk but disappear from
    // `memory.search` results — the canonical "restart wipes recall"
    // bug. Cross-host sharing (Claude Code + Desktop + IDE plugins
    // hitting the same `~/.opensquid/` store) depends on every fresh
    // engine spawn rebuilding the index from disk.
    match rehydrate_vector_index(&ctx, storage.as_ref(), vector_index.as_ref(), dims).await {
        Ok(stats) => {
            eprintln!(
                "[loop-engine serve] rehydrated {} memories (scanned {}, skipped {} missing-vec, {} parse-err)",
                stats.inserted,
                stats.scanned,
                stats.skipped_missing_vec,
                stats.skipped_parse_error,
            );
        }
        Err(e) => {
            eprintln!("[loop-engine serve] rehydrate failed (continuing with empty index): {e:#}");
        }
    }

    eprintln!(
        "[loop-engine serve] ready on stdio (lessons: create/recall/promote/discard; memories: create/search/get; dims={dims})"
    );

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin).lines();

    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = process_line(
            &line,
            &ctx,
            storage.as_ref(),
            embedder.as_ref(),
            vector_index.as_ref(),
        )
        .await;
        let json = serde_json::to_string(&response)
            .unwrap_or_else(|e| format!(r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"response serialize failed: {e}"}}}}"#));
        stdout.write_all(json.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }

    Ok(())
}

async fn process_line(
    line: &str,
    ctx: &Context,
    storage: &dyn Storage,
    embedder: &dyn Embedder,
    vector_index: &dyn VectorIndex,
) -> Response {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => return err(None, -32700, format!("parse error: {e}")),
    };
    if req.jsonrpc != "2.0" {
        return err(req.id, -32600, "jsonrpc must be \"2.0\"");
    }
    match dispatch(
        &req.method,
        req.params,
        ctx,
        storage,
        embedder,
        vector_index,
    )
    .await
    {
        Ok(value) => ok(req.id, value),
        Err(DispatchError::MethodNotFound) => {
            err(req.id, -32601, format!("method not found: {}", req.method))
        }
        Err(DispatchError::InvalidParams(msg)) => {
            err(req.id, -32602, format!("invalid params: {msg}"))
        }
        Err(DispatchError::PromotionBlocked(reasons)) => err_with_data(
            req.id,
            -32000,
            "promotion blocked",
            json!({ "reasons": reasons }),
        ),
        Err(DispatchError::UserLessonImmune(id)) => err_with_data(
            req.id,
            -32001,
            "user-authored lesson is eviction-immune",
            json!({ "lesson_id": id }),
        ),
        Err(DispatchError::NotFound(id)) => {
            err_with_data(req.id, -32002, "not found", json!({ "id": id }))
        }
        Err(DispatchError::UserMemoryImmune { id, cited_by }) => err_with_data(
            req.id,
            -32003,
            "user-cited memory is eviction-immune",
            json!({ "memory_id": id, "cited_by": cited_by }),
        ),
        Err(DispatchError::SupersedeBlocked(reason)) => err_with_data(
            req.id,
            -32004,
            "supersede blocked",
            json!({ "reason": reason }),
        ),
        Err(DispatchError::Other(e)) => err(req.id, -32603, format!("internal: {e:#}")),
    }
}

// ---- Dispatcher ----------------------------------------------------

enum DispatchError {
    MethodNotFound,
    InvalidParams(String),
    NotFound(String),
    PromotionBlocked(Vec<String>),
    UserLessonImmune(String),
    UserMemoryImmune { id: String, cited_by: u32 },
    SupersedeBlocked(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for DispatchError {
    fn from(e: anyhow::Error) -> Self {
        DispatchError::Other(e)
    }
}

async fn dispatch(
    method: &str,
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
    embedder: &dyn Embedder,
    vector_index: &dyn VectorIndex,
) -> std::result::Result<Value, DispatchError> {
    match method {
        "ping" => Ok(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") })),
        "lesson.create" => lesson_create(params, ctx, storage).await,
        "lesson.recall" => lesson_recall(params, ctx, storage).await,
        "lesson.promote" => lesson_promote(params, ctx, storage).await,
        "lesson.discard" => lesson_discard(params, ctx, storage).await,
        "lesson.list" => lesson_list(params, ctx, storage).await,
        "lesson.capture_feedback" => lesson_capture_feedback(params, ctx, storage).await,
        "lesson.supersede" => lesson_supersede(params, ctx, storage).await,
        "memory.create" => memory_create(params, ctx, storage, embedder, vector_index).await,
        "memory.search" => memory_search_method(params, ctx, storage, embedder, vector_index).await,
        "memory.get" => memory_get(params, ctx, storage).await,
        "memory.list" => memory_list(params, ctx, storage).await,
        "memory.update" => memory_update_method(params, ctx, storage, embedder, vector_index).await,
        "memory.delete" => memory_delete_method(params, ctx, storage, vector_index).await,
        _ => Err(DispatchError::MethodNotFound),
    }
}

// ---- Handlers ------------------------------------------------------

#[derive(Deserialize)]
struct LessonCreateParams {
    description: String,
    body: String,
    #[serde(default)]
    evidence: Vec<String>,
    #[serde(default)]
    authored_by: Option<String>,
    /// v1.1: codex id when `authored_by == "pack"`. Ignored otherwise.
    /// Required when `authored_by == "pack"` (validated below).
    #[serde(default)]
    pack_id: Option<String>,
    /// v1.2: opaque per-pack lesson identifier. When present alongside
    /// `pack_id` on a Pack-authored create, the engine performs an
    /// UPSERT — looks up the existing lesson by `(pack_id, external_id)`
    /// and reuses its engine `id` if found. None falls through to the
    /// legacy mint-fresh path for backwards compat with pre-v1.2 callers.
    /// Only meaningful when `authored_by == "pack"`; ignored otherwise.
    /// Preserves engine-id stability across pack re-installs so
    /// downstream consumers (system-prompt indexes, search caches)
    /// don't see new rows on every refresh.
    #[serde(default)]
    external_id: Option<String>,
    /// v1.1: seed directly as promoted, bypassing the wedge gate.
    /// Only allowed when `authored_by == "pack"` — the trust comes
    /// from user-installing the codex (Pack provenance = user-equivalent
    /// authorship). Default false. Rejected if Pack auth missing.
    #[serde(default)]
    seed_as_promoted: bool,
}

async fn lesson_create(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
) -> std::result::Result<Value, DispatchError> {
    let p: LessonCreateParams =
        serde_json::from_value(params).map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    if p.description.trim().is_empty() || p.body.trim().is_empty() {
        return Err(DispatchError::InvalidParams(
            "description and body required".into(),
        ));
    }
    let authored_by = parse_authorship(p.authored_by.as_deref());
    // v1.1: Pack-authored lessons must carry the codex id.
    if matches!(authored_by, Authorship::Pack) && p.pack_id.as_deref().unwrap_or("").is_empty() {
        return Err(DispatchError::InvalidParams(
            "pack_id required when authored_by = \"pack\"".into(),
        ));
    }
    // v1.1: seed_as_promoted only valid for Pack authorship.
    if p.seed_as_promoted && !matches!(authored_by, Authorship::Pack) {
        return Err(DispatchError::InvalidParams(
            "seed_as_promoted requires authored_by = \"pack\"".into(),
        ));
    }
    let pack_id = if matches!(authored_by, Authorship::Pack) {
        p.pack_id.clone()
    } else {
        None
    };
    let external_id = if matches!(authored_by, Authorship::Pack) {
        p.external_id.clone()
    } else {
        None
    };

    // v1.2 upsert path: when a Pack-authored create supplies both
    // `pack_id` and `external_id`, look up an existing lesson under
    // that key and reuse its engine `id` + status. Preserves engine-id
    // stability across pack re-installs so downstream consumers
    // (system-prompt indexes, search caches) don't see new rows on
    // every refresh. Discarded lessons are skipped — user-initiated
    // discards must stick (see `find_pack_lesson` doc).
    let existing = match (pack_id.as_deref(), external_id.as_deref()) {
        (Some(p_id), Some(ext_id)) => {
            crate::engine::lessons::loader::find_pack_lesson(ctx, storage, p_id, ext_id)
                .await
                .map_err(|e| DispatchError::Other(anyhow!("upsert lookup failed: {e}")))?
        }
        _ => None,
    };

    let now_iso = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let was_upsert = existing.is_some();
    let (id, created_at, status, status_dir, updated_at) = match existing {
        Some(loaded) => {
            // Reuse existing engine id + preserve current status so we
            // don't yank a promoted lesson back to pending or undo a
            // user-initiated activate. created_at stays original;
            // updated_at marks the upsert.
            (
                loaded.frontmatter.id.clone(),
                loaded.frontmatter.created_at.clone(),
                loaded.frontmatter.status,
                loaded.status_dir.clone(),
                Some(now_iso.clone()),
            )
        }
        None => {
            // Fresh mint — original v1.1 behavior.
            let (st, st_dir) = if p.seed_as_promoted {
                (LessonStatus::Promoted, "promoted".to_string())
            } else {
                (LessonStatus::Pending, "pending".to_string())
            };
            (new_lesson_id(), now_iso.clone(), st, st_dir, None)
        }
    };

    let fm = LessonFrontmatter {
        id: id.clone(),
        description: p.description.clone(),
        status,
        created_at: created_at.clone(),
        causal_narrative: build_narrative(&p.evidence, authored_by, &created_at),
        target_skill: None,
        source_feedback_ids: None,
        applied_count: 0,
        last_applied_at: None,
        thumbs_up_count: 0,
        thumbs_down_count: 0,
        external_signal_sources: vec![],
        applied_session_ids: vec![],
        promotion_eligible_at: None,
        superseded_by: None,
        superseded_at: None,
        ingest_provenance: None,
        authored_by,
        pack_id: pack_id.clone(),
        external_id: external_id.clone(),
        updated_at,
    };

    let yaml = serialize_lesson_frontmatter(&fm);
    let content = combine_frontmatter(&yaml, &p.body);
    let key = StorageKey::lesson(ctx, &status_dir, &id);
    storage
        .put(&key, Bytes::from(content))
        .await
        .map_err(|e| DispatchError::Other(anyhow!("storage put failed: {e}")))?;

    let mut response = serde_json::Map::new();
    response.insert("id".into(), Value::String(id));
    response.insert("status".into(), Value::String(status_dir.clone()));
    response.insert(
        "authored_by".into(),
        Value::String(authorship_str(authored_by).into()),
    );
    if let Some(pid) = pack_id {
        response.insert("pack_id".into(), Value::String(pid));
    }
    if let Some(ext_id) = external_id {
        response.insert("external_id".into(), Value::String(ext_id));
    }
    response.insert("created_at".into(), Value::String(created_at));
    response.insert("updated".into(), Value::Bool(was_upsert));
    Ok(Value::Object(response))
}

#[derive(Deserialize)]
struct LessonRecallParams {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    5
}

async fn lesson_recall(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
) -> std::result::Result<Value, DispatchError> {
    let p: LessonRecallParams =
        serde_json::from_value(params).map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    if p.query.trim().is_empty() {
        return Err(DispatchError::InvalidParams("query required".into()));
    }

    // Iterate the 5 status dirs (skip discarded for recall). Best-
    // effort: per-key parse failures warn + skip.
    let statuses = ["pending", "active", "promoted", "superseded"];
    let mut results: Vec<(f32, Value)> = Vec::new();
    for status in statuses {
        let prefix = StorageKey::lesson_status_prefix(ctx, status);
        let keys = storage
            .list(&prefix)
            .await
            .map_err(|e| DispatchError::Other(anyhow!("storage list failed: {e}")))?;
        for k in keys {
            let bytes = match storage.get(&k).await {
                Ok(Some(b)) => b,
                _ => continue,
            };
            let content = match std::str::from_utf8(&bytes) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let split = match split_frontmatter_normalized(content) {
                Ok(s) => s,
                Err(e) => {
                    warn!(key = %k, error = %e, "recall: skip bad frontmatter");
                    continue;
                }
            };
            let fm = match parse_lesson_frontmatter(&split.yaml) {
                Ok(fm) => fm,
                Err(e) => {
                    warn!(key = %k, error = %e, "recall: skip unparseable");
                    continue;
                }
            };
            let sim = score_text_match(&p.query, &fm.description, &split.body);
            if sim > 0.0 {
                results.push((
                    sim,
                    json!({
                        "kind": "lesson",
                        "id": fm.id,
                        "description": fm.description,
                        "status": status,
                        "body_preview": preview(&split.body, 240),
                        "similarity": (sim * 1000.0).round() / 1000.0,
                        "applied_count": fm.applied_count,
                    }),
                ));
            }
        }
    }
    results.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(p.limit);
    let returned: Vec<Value> = results.into_iter().map(|(_, v)| v).collect();
    Ok(json!({
        "query": p.query,
        "returned": returned.len(),
        "results": returned,
    }))
}

#[derive(Deserialize)]
struct LessonPromoteParams {
    id: String,
}

async fn lesson_promote(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
) -> std::result::Result<Value, DispatchError> {
    let p: LessonPromoteParams =
        serde_json::from_value(params).map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    // Probe existence first so we surface NotFound cleanly.
    if load_lesson(ctx, storage, &p.id)
        .await
        .map_err(|e| DispatchError::Other(anyhow!("load failed: {e}")))?
        .is_none()
    {
        return Err(DispatchError::NotFound(p.id));
    }
    match promote(ctx, storage, &p.id, &PromotionConfig::default(), Utc::now()).await {
        Ok(loaded) => Ok(json!({
            "ok": true,
            "id": p.id,
            "gate": "passed",
            "status": "promoted",
            "from": loaded.status_dir,
        })),
        Err(EngineError::PromotionBlocked { reasons }) => Err(DispatchError::PromotionBlocked(
            reasons.iter().map(|r| r.to_string()).collect(),
        )),
        Err(EngineError::LessonNotFound { id }) => Err(DispatchError::NotFound(id)),
        Err(e) => Err(DispatchError::Other(anyhow!("promote failed: {e}"))),
    }
}

#[derive(Deserialize)]
struct LessonDiscardParams {
    id: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    force: bool,
}

async fn lesson_discard(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
) -> std::result::Result<Value, DispatchError> {
    let p: LessonDiscardParams =
        serde_json::from_value(params).map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    match discard(ctx, storage, &p.id, p.reason.clone(), p.force, Utc::now()).await {
        Ok(loaded) => Ok(json!({
            "ok": true,
            "id": p.id,
            "status": "discarded",
            "from": loaded.status_dir,
            "reason": p.reason,
        })),
        Err(EngineError::UserLessonImmune { id }) => Err(DispatchError::UserLessonImmune(id)),
        Err(EngineError::LessonNotFound { id }) => Err(DispatchError::NotFound(id)),
        Err(e) => Err(DispatchError::Other(anyhow!("discard failed: {e}"))),
    }
}

// ---- v1.3: lesson.list / capture_feedback / supersede --------------

#[derive(Deserialize)]
struct LessonListParams {
    /// Restrict to specific status dirs. Default: all four non-discarded.
    #[serde(default)]
    statuses: Option<Vec<String>>,
    /// Page size. Default 50, capped at 500.
    #[serde(default)]
    limit: Option<usize>,
    /// Number of items to skip from the deterministic-sorted list.
    /// Default 0.
    #[serde(default)]
    offset: Option<usize>,
}

const DEFAULT_LIST_LIMIT: usize = 50;
const MAX_LIST_LIMIT: usize = 500;

async fn lesson_list(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
) -> std::result::Result<Value, DispatchError> {
    let p: LessonListParams =
        serde_json::from_value(params).map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    let limit = p.limit.unwrap_or(DEFAULT_LIST_LIMIT).min(MAX_LIST_LIMIT);
    let offset = p.offset.unwrap_or(0);
    let statuses: Vec<&str> = match &p.statuses {
        Some(v) if !v.is_empty() => {
            for s in v {
                if !paths::LESSON_STATUS_DIRS.contains(&s.as_str()) {
                    return Err(DispatchError::InvalidParams(format!(
                        "unknown status '{s}'; expected one of {:?}",
                        paths::LESSON_STATUS_DIRS
                    )));
                }
            }
            v.iter().map(String::as_str).collect()
        }
        _ => paths::LESSON_STATUS_DIRS
            .iter()
            .filter(|s| **s != "discarded")
            .copied()
            .collect(),
    };

    // Collect all lessons across requested statuses. Sort deterministically
    // by (status, id) so paginated callers get stable order.
    let mut rows: Vec<Value> = Vec::new();
    for status in &statuses {
        let prefix = StorageKey::lesson_status_prefix(ctx, status);
        let keys = storage
            .list(&prefix)
            .await
            .map_err(|e| DispatchError::Other(anyhow!("storage list failed: {e}")))?;
        for k in keys {
            let bytes = match storage.get(&k).await {
                Ok(Some(b)) => b,
                _ => continue,
            };
            let content = match std::str::from_utf8(&bytes) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let split = match split_frontmatter_normalized(content) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let fm = match parse_lesson_frontmatter(&split.yaml) {
                Ok(fm) => fm,
                Err(_) => continue,
            };
            rows.push(json!({
                "id": fm.id,
                "description": fm.description,
                "status": status,
                "authored_by": authorship_str(fm.authored_by),
                "pack_id": fm.pack_id,
                "external_id": fm.external_id,
                "applied_count": fm.applied_count,
                "thumbs_up_count": fm.thumbs_up_count,
                "thumbs_down_count": fm.thumbs_down_count,
                "created_at": fm.created_at,
                "updated_at": fm.updated_at,
            }));
        }
    }
    // Stable order: status-then-id (alphabetical).
    rows.sort_by(|a, b| {
        let sa = a.get("status").and_then(Value::as_str).unwrap_or("");
        let sb = b.get("status").and_then(Value::as_str).unwrap_or("");
        let by_status = sa.cmp(sb);
        if by_status != std::cmp::Ordering::Equal {
            return by_status;
        }
        let ia = a.get("id").and_then(Value::as_str).unwrap_or("");
        let ib = b.get("id").and_then(Value::as_str).unwrap_or("");
        ia.cmp(ib)
    });

    let total = rows.len();
    let page: Vec<Value> = rows.into_iter().skip(offset).take(limit).collect();
    Ok(json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        "returned": page.len(),
        "results": page,
    }))
}

#[derive(Deserialize)]
struct LessonCaptureFeedbackParams {
    id: String,
    polarity: String,
    #[serde(default)]
    source_signal_id: Option<String>,
}

async fn lesson_capture_feedback(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
) -> std::result::Result<Value, DispatchError> {
    let p: LessonCaptureFeedbackParams =
        serde_json::from_value(params).map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    let polarity = match p.polarity.as_str() {
        "thumbs_up" | "up" | "+" => FeedbackPolarity::ThumbsUp,
        "thumbs_down" | "down" | "-" => FeedbackPolarity::ThumbsDown,
        other => {
            return Err(DispatchError::InvalidParams(format!(
                "polarity must be 'thumbs_up' or 'thumbs_down', got '{other}'"
            )))
        }
    };
    let signal_id = p
        .source_signal_id
        .clone()
        .or_else(|| Some(format!("rpc-{}", new_lesson_id())));
    match transitions_capture_feedback(ctx, storage, &p.id, polarity, signal_id, Utc::now()).await {
        Ok(loaded) => Ok(json!({
            "ok": true,
            "id": loaded.frontmatter.id,
            "status": loaded.status_dir,
            "thumbs_up_count": loaded.frontmatter.thumbs_up_count,
            "thumbs_down_count": loaded.frontmatter.thumbs_down_count,
            "external_signal_sources": loaded.frontmatter.external_signal_sources,
        })),
        Err(EngineError::LessonNotFound { id }) => Err(DispatchError::NotFound(id)),
        Err(e) => Err(DispatchError::Other(anyhow!(
            "capture_feedback failed: {e}"
        ))),
    }
}

#[derive(Deserialize)]
struct LessonSupersedeParams {
    old_id: String,
    new_id: String,
    #[serde(default)]
    force: bool,
}

async fn lesson_supersede(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
) -> std::result::Result<Value, DispatchError> {
    let p: LessonSupersedeParams =
        serde_json::from_value(params).map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    match transitions_supersede(ctx, storage, &p.old_id, &p.new_id, p.force, Utc::now()).await {
        Ok(loaded) => Ok(json!({
            "ok": true,
            "old_id": p.old_id,
            "new_id": p.new_id,
            "old_status": loaded.status_dir,
        })),
        Err(EngineError::LessonNotFound { id }) => Err(DispatchError::NotFound(id)),
        Err(EngineError::UserLessonImmune { id }) => Err(DispatchError::UserLessonImmune(id)),
        Err(EngineError::LessonSupersedeInvalid { reason, .. }) => {
            Err(DispatchError::SupersedeBlocked(reason.to_string()))
        }
        Err(e) => Err(DispatchError::Other(anyhow!("supersede failed: {e}"))),
    }
}

// ---- Helpers -------------------------------------------------------

fn new_lesson_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u32;
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let v = (nanos ^ counter).wrapping_mul(0x9E3779B1);
    format!("les-{v:08x}")
}

fn parse_authorship(s: Option<&str>) -> Authorship {
    match s {
        Some("user") => Authorship::User,
        Some("pack") => Authorship::Pack,
        _ => Authorship::Llm,
    }
}

fn authorship_str(a: Authorship) -> &'static str {
    match a {
        Authorship::User => "user",
        Authorship::Pack => "pack",
        _ => "agent",
    }
}

/// Build a minimal CausalNarrative when evidence is provided. Without
/// evidence we leave `causal_narrative: None` so the gate blocks
/// promotion (the wedge invariant — no narrative → no graduation).
fn build_narrative(
    evidence: &[String],
    authored_by: Authorship,
    now_iso: &str,
) -> Option<crate::engine::yaml::CausalNarrative> {
    if evidence.is_empty() {
        return None;
    }
    use crate::engine::yaml::{CausalNarrative, Confidence, EvidenceRef, GeneratedBy};
    Some(CausalNarrative {
        trigger: "user-supplied".into(),
        failure_mode: "user-supplied".into(),
        correction: "user-supplied".into(),
        confidence: Confidence::Inferred,
        evidence_refs: evidence
            .iter()
            .map(|e| {
                if e.starts_with("mem-") {
                    EvidenceRef::Memory(crate::engine::memory::MemoryId::new(e.clone()))
                } else {
                    EvidenceRef::Quote(e.clone())
                }
            })
            .collect(),
        generated_by: match authored_by {
            Authorship::User => GeneratedBy::User,
            _ => GeneratedBy::Llm,
        },
        generated_at: now_iso.to_string(),
    })
}

// ---- Memory handlers ----------------------------------------------

#[derive(Deserialize)]
struct MemoryCreateParams {
    description: String,
    content: String,
    #[serde(default)]
    authored_by: Option<String>,
    /// Phase F D-F8 wire-up: optional scope tag. Wire shape matches
    /// `MemoryScope` serde — `"user"`, `"global"`, `{"team":"id"}`,
    /// `{"skill":"id"}`, `{"project":"id"}`. Defaults to `User` when
    /// absent (matches engine default).
    #[serde(default)]
    scope: Option<MemoryScope>,
    /// Phase G D-G1 (v0.4) wire-up: optional provenance metadata. All
    /// fields inside are optional — hosts populate what they can
    /// detect. Wire shape mirrors [`MemoryOrigin`] serde.
    #[serde(default)]
    origin: Option<MemoryOrigin>,
}

async fn memory_create(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
    embedder: &dyn Embedder,
    vector_index: &dyn VectorIndex,
) -> std::result::Result<Value, DispatchError> {
    let p: MemoryCreateParams =
        serde_json::from_value(params).map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    if p.description.trim().is_empty() || p.content.trim().is_empty() {
        return Err(DispatchError::InvalidParams(
            "description and content required".into(),
        ));
    }
    let _authored_by = parse_authorship(p.authored_by.as_deref());
    let id = new_memory_id();
    let now = chrono::Utc::now();
    let scope = p.scope.unwrap_or_default();
    let mem = memory_insert_with_provenance(
        ctx,
        storage,
        embedder,
        vector_index,
        MemoryId::new(id.clone()),
        p.description,
        p.content,
        now,
        scope,
        p.origin,
    )
    .await
    .map_err(|e| DispatchError::Other(anyhow!("memory.insert failed: {e}")))?;
    Ok(json!({
        "id": mem.frontmatter.id.as_str(),
        "description": mem.frontmatter.description,
        "created_at": mem.frontmatter.created_at,
        "scope": mem.frontmatter.scope,
        "origin": mem.frontmatter.origin,
    }))
}

#[derive(Deserialize)]
struct MemorySearchParams {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
    /// v0.3.1: when `true`, return the full memory body instead of a
    /// 240-char preview. The host (opensquid) toggles this when an
    /// agent needs to re-anchor on a long memory after drift.
    #[serde(default)]
    include_body: bool,
    /// v0.3.1: optional scope filter — restricts results to memories
    /// whose `MemoryScope` satisfies the filter. Defaults to no filter
    /// (returns all scopes visible to the caller).
    #[serde(default)]
    scope_filter: Option<ScopeFilterWire>,
    /// v0.5: which search path to run. Defaults to `Semantic` for
    /// back-compat with v0.3.1+ callers. `Text` runs the new
    /// text-match scan; `Hybrid` runs both and RRF-merges. The
    /// hybrid path is what opensquid's `recall` defaults to in v0.5
    /// — it fixes the "Gianna" false-negative (semantic 0.486 < 0.5
    /// threshold) by surfacing the description's substring match.
    #[serde(default)]
    mode: Option<SearchMode>,
    /// v0.5: per-sub-search similarity floor. Applied to RAW scores
    /// (cosine for semantic, token+substring for text) BEFORE the
    /// hybrid RRF merge — RRF scores are in a different range and
    /// can't share the threshold meaningfully. Default `0.0` (no
    /// filtering). opensquid's recall passes its `min_similarity`
    /// here so the v0.4 "decision-makable signal" UX survives the
    /// hybrid transition.
    #[serde(default)]
    min_similarity: Option<f32>,
}

/// v0.5 search-path selector. Wire serde: `"semantic"` (default),
/// `"text"`, `"hybrid"`. Maps to [`crate::engine::memory::search`],
/// [`crate::engine::memory::text_search`], and
/// [`crate::engine::memory::hybrid_search`] respectively.
#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum SearchMode {
    #[default]
    Semantic,
    Text,
    Hybrid,
}

/// JSON-wire shape for `MemoryScopeFilter`. The engine's enum doesn't
/// derive `Deserialize` directly because the variants want
/// `&'static str` discriminants and `Vec<MemoryScope>` collections —
/// the wire form is a discriminated object that translates cleanly.
///
/// Shape:
/// - `{"kind": "exact", "scope": <MemoryScope>}`
/// - `{"kind": "kind", "kind_name": "user" | "team" | "skill" | "project" | "global"}`
/// - `{"kind": "any_of", "scopes": [<MemoryScope>...]}`
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ScopeFilterWire {
    Exact { scope: MemoryScope },
    Kind { kind_name: String },
    AnyOf { scopes: Vec<MemoryScope> },
}

impl ScopeFilterWire {
    fn into_engine(self) -> Result<MemoryScopeFilter, DispatchError> {
        match self {
            Self::Exact { scope } => Ok(MemoryScopeFilter::Exact(scope)),
            Self::AnyOf { scopes } => Ok(MemoryScopeFilter::AnyOf(scopes)),
            Self::Kind { kind_name } => {
                // The engine's `Kind` variant wants a `&'static str`
                // to keep the discriminator allocation-free. Translate
                // the wire string against the known set.
                let s: &'static str = match kind_name.as_str() {
                    "user" => "user",
                    "team" => "team",
                    "skill" => "skill",
                    "project" => "project",
                    "global" => "global",
                    other => {
                        return Err(DispatchError::InvalidParams(format!(
                            "unknown scope kind: {other}"
                        )));
                    }
                };
                Ok(MemoryScopeFilter::Kind(s))
            }
        }
    }
}

async fn memory_search_method(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
    embedder: &dyn Embedder,
    vector_index: &dyn VectorIndex,
) -> std::result::Result<Value, DispatchError> {
    let p: MemorySearchParams =
        serde_json::from_value(params).map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    if p.query.trim().is_empty() {
        return Err(DispatchError::InvalidParams("query required".into()));
    }
    let preview_len = if p.include_body { usize::MAX } else { 240 };
    let scope_filter = p.scope_filter.map(|w| w.into_engine()).transpose()?;
    let mode = p.mode.unwrap_or_default();
    let min_similarity = p.min_similarity.unwrap_or(0.0).clamp(0.0, 1.0);

    // v0.5: dispatch by mode. Default `Semantic` matches v0.3.1+
    // behavior; `Text` is the new linear-scan token+substring path;
    // `Hybrid` runs both and RRF-merges (the path opensquid's recall
    // defaults to). The threshold filter applies to RAW scores
    // (cosine / token+substring) BEFORE any RRF — for Hybrid the
    // filter is enforced inside `hybrid_search` per-sub-list.
    let hits = match mode {
        SearchMode::Semantic => {
            let mut results = memory_search(
                ctx,
                storage,
                embedder,
                vector_index,
                &MemoryQuery::Text(p.query.clone()),
                p.limit,
                preview_len,
                scope_filter.as_ref(),
            )
            .await
            .map_err(|e| DispatchError::Other(anyhow!("memory.search (semantic) failed: {e}")))?;
            results.retain(|r| r.similarity >= min_similarity);
            results
        }
        SearchMode::Text => {
            let mut results = memory_text_search(
                ctx,
                storage,
                &p.query,
                p.limit,
                preview_len,
                scope_filter.as_ref(),
            )
            .await
            .map_err(|e| DispatchError::Other(anyhow!("memory.search (text) failed: {e}")))?;
            results.retain(|r| r.similarity >= min_similarity);
            results
        }
        SearchMode::Hybrid => memory_hybrid_search(
            ctx,
            storage,
            embedder,
            vector_index,
            &p.query,
            p.limit,
            preview_len,
            scope_filter.as_ref(),
            min_similarity,
        )
        .await
        .map_err(|e| DispatchError::Other(anyhow!("memory.search (hybrid) failed: {e}")))?,
    };

    let results: Vec<Value> = hits
        .into_iter()
        .map(|h| {
            // Build manually so an absent `source` (pre-v0.5 refs)
            // omits the JSON key entirely rather than emitting
            // `"source": null` — the `serde(skip_serializing_if)`
            // contract belongs at the wire layer, but `json!` macro
            // doesn't honor field-level serde attributes.
            let mut obj = serde_json::Map::new();
            obj.insert("kind".into(), Value::String("memory".into()));
            obj.insert("id".into(), Value::String(h.id.as_str().to_string()));
            obj.insert("description".into(), Value::String(h.description));
            obj.insert("body_preview".into(), Value::String(h.body_preview));
            obj.insert(
                "similarity".into(),
                serde_json::json!((h.similarity * 1000.0).round() / 1000.0),
            );
            if let Some(src) = h.source {
                obj.insert(
                    "source".into(),
                    serde_json::to_value(src).expect("HitSource serde never fails"),
                );
            }
            Value::Object(obj)
        })
        .collect();
    Ok(json!({
        "query": p.query,
        "returned": results.len(),
        "results": results,
    }))
}

#[derive(Deserialize)]
struct MemoryGetParams {
    id: String,
}

/// `memory.get` — fetch a memory by id, returning the FULL content
/// (no preview truncation). Companion to `memory.search`, which
/// returns previews. The intended flow:
///  1. `memory.search` surfaces top-k hits with previews + similarity
///  2. Caller spots a hit whose preview looks load-bearing
///  3. `memory.get` returns the full content for re-anchoring
async fn memory_get(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
) -> std::result::Result<Value, DispatchError> {
    let p: MemoryGetParams =
        serde_json::from_value(params).map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    if p.id.trim().is_empty() {
        return Err(DispatchError::InvalidParams("id required".into()));
    }
    let mem_id = MemoryId::new(p.id.clone());
    match memory_get_by_id(ctx, storage, &mem_id).await {
        Ok(Some(mem)) => Ok(json!({
            "id": mem.frontmatter.id.as_str(),
            "description": mem.frontmatter.description,
            "content": mem.content,
            "created_at": mem.frontmatter.created_at,
            "scope": mem.frontmatter.scope,
            "origin": mem.frontmatter.origin,
        })),
        Ok(None) => Err(DispatchError::NotFound(p.id)),
        Err(e) => Err(DispatchError::Other(anyhow!("memory.get failed: {e}"))),
    }
}

#[derive(Deserialize)]
struct MemoryListParams {
    /// Optional scope filter — same wire shape as memory.search.
    #[serde(default)]
    scope_filter: Option<ScopeFilterWire>,
    /// Page size. Default 50, capped at 500.
    #[serde(default)]
    limit: Option<usize>,
    /// Number of items to skip from the deterministic-sorted list.
    /// Default 0.
    #[serde(default)]
    offset: Option<usize>,
}

/// `memory.list` — paginated enumeration of all memories. Filter-
/// optional via `scope_filter`. Returns frontmatter-shape rows (id,
/// description, scope, origin, created_at, updated_at,
/// consumed_by_user_lessons) but NOT body — use `memory.get` for
/// the full content of any single hit.
async fn memory_list(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
) -> std::result::Result<Value, DispatchError> {
    let p: MemoryListParams =
        serde_json::from_value(params).map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    let limit = p.limit.unwrap_or(DEFAULT_LIST_LIMIT).min(MAX_LIST_LIMIT);
    let offset = p.offset.unwrap_or(0);
    let scope_filter = match p.scope_filter {
        Some(w) => Some(w.into_engine()?),
        None => None,
    };

    let prefix = StorageKey::memories_prefix(ctx);
    let keys = storage
        .list(&prefix)
        .await
        .map_err(|e| DispatchError::Other(anyhow!("storage list failed: {e}")))?;

    let mut rows: Vec<Value> = Vec::new();
    for key in keys {
        // Drive off .md frontmatter files; .vec sidecars are companion
        // data that the list path doesn't surface.
        let key_str = key.as_str();
        if !key_str.ends_with(".md") {
            continue;
        }
        let Some(fname) = key_str.rsplit('/').next() else {
            continue;
        };
        let Some(id_str) = fname.strip_suffix(".md") else {
            continue;
        };
        let mem_id = MemoryId::new(id_str.to_string());
        let mem = match memory_get_by_id(ctx, storage, &mem_id).await {
            Ok(Some(m)) => m,
            _ => continue,
        };
        // Apply scope filter if requested.
        if let Some(filter) = &scope_filter {
            if !filter.matches(&mem.frontmatter.scope) {
                continue;
            }
        }
        rows.push(json!({
            "id": mem.frontmatter.id.as_str(),
            "description": mem.frontmatter.description,
            "scope": mem.frontmatter.scope,
            "origin": mem.frontmatter.origin,
            "created_at": mem.frontmatter.created_at,
            "updated_at": mem.frontmatter.updated_at,
            "consumed_by_user_lessons": mem.frontmatter.consumed_by_user_lessons,
        }));
    }
    // Stable order: id ascending. Memory ids are ULID-shaped so
    // alphabetical == chronological for practical purposes.
    rows.sort_by(|a, b| {
        let ia = a.get("id").and_then(Value::as_str).unwrap_or("");
        let ib = b.get("id").and_then(Value::as_str).unwrap_or("");
        ia.cmp(ib)
    });

    let total = rows.len();
    let page: Vec<Value> = rows.into_iter().skip(offset).take(limit).collect();
    Ok(json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        "returned": page.len(),
        "results": page,
    }))
}

#[derive(Deserialize)]
struct MemoryUpdateParams {
    id: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    scope: Option<MemoryScope>,
}

/// `memory.update` — mutate description, content, and/or scope on an
/// existing memory. Identity (`id`, `created_at`, citation counter,
/// `derived_from`, `origin`) is always preserved. Re-embeds on
/// content change; description/scope-only edits skip the embed path.
/// Returns the updated frontmatter shape (no body); call `memory.get`
/// to re-read the new full content.
async fn memory_update_method(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
    embedder: &dyn Embedder,
    vector_index: &dyn VectorIndex,
) -> std::result::Result<Value, DispatchError> {
    let p: MemoryUpdateParams =
        serde_json::from_value(params).map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    if p.id.trim().is_empty() {
        return Err(DispatchError::InvalidParams("id required".into()));
    }
    if p.description.is_none() && p.content.is_none() && p.scope.is_none() {
        return Err(DispatchError::InvalidParams(
            "at least one of description, content, scope must be supplied".into(),
        ));
    }
    let now = chrono::Utc::now();
    let mem_id = MemoryId::new(p.id.clone());
    match memory_update(
        ctx,
        storage,
        embedder,
        vector_index,
        &mem_id,
        p.description,
        p.content,
        p.scope,
        now,
    )
    .await
    {
        Ok(Some(mem)) => Ok(json!({
            "ok": true,
            "id": mem.frontmatter.id.as_str(),
            "description": mem.frontmatter.description,
            "created_at": mem.frontmatter.created_at,
            "updated_at": mem.frontmatter.updated_at,
            "scope": mem.frontmatter.scope,
            "origin": mem.frontmatter.origin,
        })),
        Ok(None) => Err(DispatchError::NotFound(p.id)),
        Err(e) => Err(DispatchError::Other(anyhow!("memory.update failed: {e}"))),
    }
}

#[derive(Deserialize)]
struct MemoryDeleteParams {
    id: String,
    /// Bypass user-immunity. `false` (the default) returns a
    /// structured `UserMemoryImmune` error if the memory is cited by
    /// a user-authored lesson. `true` is the explicit "yes I really
    /// mean it" override — the user-initiated `forget` path.
    #[serde(default)]
    force: bool,
}

/// `memory.delete` — the `forget` operation. Routes to
/// [`crate::engine::memory::delete`] which already encodes the
/// user-immunity invariant. `force = true` is the user-initiated
/// override; `force = false` (default) is the safe engine-initiated
/// path.
async fn memory_delete_method(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
    vector_index: &dyn VectorIndex,
) -> std::result::Result<Value, DispatchError> {
    let p: MemoryDeleteParams =
        serde_json::from_value(params).map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    if p.id.trim().is_empty() {
        return Err(DispatchError::InvalidParams("id required".into()));
    }
    let mem_id = MemoryId::new(p.id.clone());
    match memory_delete(ctx, storage, vector_index, &mem_id, p.force).await {
        Ok(()) => Ok(json!({
            "ok": true,
            "id": p.id,
            "forced": p.force,
        })),
        Err(EngineError::UserMemoryImmune { id, cited_by }) => {
            Err(DispatchError::UserMemoryImmune { id, cited_by })
        }
        Err(e) => Err(DispatchError::Other(anyhow!("memory.delete failed: {e}"))),
    }
}

fn new_memory_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u32;
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let v = (nanos ^ counter).wrapping_mul(0x9E3779B1);
    format!("mem-{v:08x}")
}

fn preview(body: &str, max: usize) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_wire(s: &str) -> ScopeFilterWire {
        serde_json::from_str(s).expect("valid wire JSON")
    }

    #[test]
    fn scope_filter_wire_exact_user() {
        let w = parse_wire(r#"{"kind":"exact","scope":"user"}"#);
        let f = w.into_engine().unwrap();
        assert!(f.matches(&MemoryScope::User));
        assert!(!f.matches(&MemoryScope::Global));
    }

    #[test]
    fn scope_filter_wire_exact_project() {
        let w = parse_wire(r#"{"kind":"exact","scope":{"project":"loop"}}"#);
        let f = w.into_engine().unwrap();
        assert!(f.matches(&MemoryScope::Project("loop".into())));
        assert!(!f.matches(&MemoryScope::Project("other".into())));
        assert!(!f.matches(&MemoryScope::User));
    }

    #[test]
    fn scope_filter_wire_kind_known_discriminators() {
        for kind in ["user", "team", "skill", "project", "global"] {
            let json = format!(r#"{{"kind":"kind","kind_name":"{kind}"}}"#);
            let w = parse_wire(&json);
            w.into_engine().unwrap_or_else(|_| {
                panic!("known discriminator {kind} should convert");
            });
        }
    }

    #[test]
    fn scope_filter_wire_kind_matches_any_team() {
        let w = parse_wire(r#"{"kind":"kind","kind_name":"team"}"#);
        let f = w.into_engine().unwrap();
        assert!(f.matches(&MemoryScope::Team("acme".into())));
        assert!(f.matches(&MemoryScope::Team("other".into())));
        assert!(!f.matches(&MemoryScope::User));
    }

    #[test]
    fn scope_filter_wire_kind_unknown_errors() {
        let w = parse_wire(r#"{"kind":"kind","kind_name":"nonsense"}"#);
        let err = w.into_engine().expect_err("unknown kind must error");
        match err {
            DispatchError::InvalidParams(msg) => {
                assert!(
                    msg.contains("nonsense"),
                    "msg should name the offending kind: {msg}"
                );
            }
            other => panic!("expected InvalidParams, got: {other:?}"),
        }
    }

    #[test]
    fn scope_filter_wire_any_of_matches_set() {
        let w = parse_wire(r#"{"kind":"any_of","scopes":["user",{"project":"loop"}]}"#);
        let f = w.into_engine().unwrap();
        assert!(f.matches(&MemoryScope::User));
        assert!(f.matches(&MemoryScope::Project("loop".into())));
        assert!(!f.matches(&MemoryScope::Project("other".into())));
        assert!(!f.matches(&MemoryScope::Global));
    }

    // Allow {dbg/Debug} for DispatchError so the panic messages above
    // can interpolate the variant. Cheap to add given the limited
    // surface; lives behind cfg(test) to keep the prod binary clean.
    impl std::fmt::Debug for DispatchError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::MethodNotFound => f.write_str("MethodNotFound"),
                Self::InvalidParams(s) => write!(f, "InvalidParams({s})"),
                Self::NotFound(s) => write!(f, "NotFound({s})"),
                Self::PromotionBlocked(rs) => write!(f, "PromotionBlocked({rs:?})"),
                Self::UserLessonImmune(s) => write!(f, "UserLessonImmune({s})"),
                Self::UserMemoryImmune { id, cited_by } => {
                    write!(f, "UserMemoryImmune({id}, cited_by={cited_by})")
                }
                Self::SupersedeBlocked(reason) => write!(f, "SupersedeBlocked({reason})"),
                Self::Other(e) => write!(f, "Other({e:#})"),
            }
        }
    }
}
