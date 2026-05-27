//! JSON-RPC 2.0 endpoint — programmatic engine access for host
//! adapters (opensquid MCP server, future TS/Python launchers).
//!
//! Two transport flavors share the same line-framed JSON-RPC 2.0
//! dispatch:
//!
//! - **stdio** (default): one engine subprocess per host. Reads
//!   requests from stdin, writes responses to stdout, diagnostics to
//!   stderr. Lifetime = one host session.
//! - **Unix domain socket** (`serve(Some(path))`): long-running
//!   daemon. One engine process serves many concurrent connections
//!   across opensquid hooks + sessions. State is built once at
//!   startup (Context + Storage + Embedder + HNSW VectorIndex
//!   rehydrated from `.vec` sidecars) and shared across all spawned
//!   connection handlers via `Arc<ServeState>`. Per-connection
//!   `tokio::spawn` gives cross-connection concurrency — without it
//!   two concurrent acquires would serialize globally and the daemon
//!   would offer no win over per-process stdio spawn.
//!
//! Both modes share [`serve_one_connection`] — the only difference is
//! how the reader + writer are constructed.
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
//! - `memory.consolidate` — atomic safe-compression: compress → recall-replay verify → gated delete
//!
//! Manifest assembly + skill/persona/team ops land in a follow-on
//! serve cycle.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use bytes::Bytes;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader};
use tracing::warn;

use crate::engine::context::Context;
use crate::engine::embedding::{Embedder, OpenAiCompatibleEmbedder};
use crate::engine::error::EngineError;
use crate::engine::lessons::gate::PromotionConfig;
use crate::engine::lessons::loader::get_by_id as load_lesson;
use crate::engine::lessons::transitions::{
    FeedbackPolarity, capture_feedback as transitions_capture_feedback, discard, promote,
    supersede as transitions_supersede,
};
use crate::engine::llm::{LlmClient, OpenAiCompatibleLlm};
use crate::engine::memory::{
    CompressionConfig, CompressionWindow, DEFAULT_CONSOLIDATE_RECALL_K, MemoryId, MemoryOrigin,
    MemoryQuery, MemoryScope, MemoryScopeFilter, compress as memory_compress_fn,
    consolidate as memory_consolidate_fn, delete as memory_delete, get_by_id as memory_get_by_id,
    hybrid_search as memory_hybrid_search, insert_with_provenance as memory_insert_with_provenance,
    recompute_citation_counts as memory_recompute_citation_counts, rehydrate_vector_index,
    search as memory_search, text_search as memory_text_search, update as memory_update,
};
use crate::engine::paths;
use crate::engine::phase_ledger::{self, LedgerError, Phase};
use crate::engine::scoring::score_text_match;
use crate::engine::storage::filesystem::LocalFsStorage;
use crate::engine::storage::{Storage, StorageKey};
use crate::engine::vector::{HnswVectorIndex, VectorIndex};
use crate::engine::yaml::writer::serialize_lesson_frontmatter;
use crate::engine::yaml::{
    Authorship, LessonFrontmatter, LessonStatus, reader::parse_lesson_frontmatter,
};
use crate::engine::yaml::{combine_frontmatter, split_frontmatter_normalized};

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

/// Engine state shared across all connections in UDS mode.
///
/// Built once by [`build_state`] at startup; cloned via `Arc` into each
/// per-connection task. In stdio mode the same state is constructed
/// but lives behind a single borrow because there is only ever one
/// connection. Layout mirrors the legacy inline `run()` locals 1:1 to
/// keep the refactor behavior-preserving.
pub struct ServeState {
    ctx: Context,
    storage: Arc<dyn Storage>,
    embedder: Arc<dyn Embedder>,
    vector_index: Arc<dyn VectorIndex>,
    /// Optional LLM client for handlers that need generation (today:
    /// `memory.compress`). Built from env in [`build_state`]
    /// (`OpenAiCompatibleLlm::from_env`, defaulting to local Ollama).
    /// `None` only when construction fails (misconfiguration) or a test
    /// builds an LLM-less state — handlers that require an LLM then
    /// return a clear dispatch error rather than panicking, so the rest
    /// of the RPC surface stays available.
    llm: Option<Arc<dyn LlmClient>>,
}

/// Run the serve loop. Returns when the transport closes — stdio EOF
/// for `socket = None`, never for `Some(path)` (UDS accept loop runs
/// until process exit / fatal error). Errors only on initialization
/// failures; per-request errors surface via JSON-RPC error responses.
pub async fn run(socket: Option<PathBuf>) -> Result<()> {
    let state = build_state().await?;
    match socket {
        None => serve_stdio(state).await,
        Some(path) => serve_uds(state, path).await,
    }
}

/// Construct the shared engine state. Rehydrates the HNSW index from
/// on-disk `.vec` sidecars exactly once — failure to rehydrate is
/// non-fatal (logged + continue with an empty index) so a corrupt
/// sidecar can't prevent the daemon from coming up.
async fn build_state() -> Result<ServeState> {
    let ctx = Context::single_user_local();
    let home = paths::loop_home().context("resolving loop_home")?;
    paths::ensure_loop_dirs().context("ensuring loop dirs")?;
    let storage: Arc<dyn Storage> = Arc::new(LocalFsStorage::new(home));

    // Embedder + vector index for memory ops. Defaults to local
    // Ollama running Qwen3-Embedding-4B per the architecture decision;
    // env vars override (see OpenAiCompatibleEmbedder::from_env).
    let embedder = OpenAiCompatibleEmbedder::from_env()
        .context("constructing embedder (LOOP_EMBEDDER_* env)")?;
    let dims = embedder.dimensions();
    let embedder: Arc<dyn Embedder> = Arc::new(embedder);
    let vector_index: Arc<dyn VectorIndex> = Arc::new(HnswVectorIndex::new(dims));

    // Rehydrate the HNSW index from on-disk `.vec` sidecars. The
    // index is in-memory; without this step, memories persisted by
    // a previous engine session remain on disk but disappear from
    // `memory.search` results — the canonical "restart wipes recall"
    // bug. Cross-host sharing (Claude Code + Desktop + IDE plugins
    // hitting the same `~/.opensquid/` store) depends on every fresh
    // engine spawn rebuilding the index from disk. UDS mode does this
    // exactly ONCE at daemon startup; per-connection handlers reuse
    // the rehydrated index via the shared `Arc<ServeState>`.
    match rehydrate_vector_index(&ctx, storage.as_ref(), vector_index.as_ref(), dims).await {
        Ok(stats) => {
            eprintln!(
                "[loop-engine serve] rehydrated {} memories (scanned {}, skipped {} missing-vec, {} parse-err)",
                stats.inserted, stats.scanned, stats.skipped_missing_vec, stats.skipped_parse_error,
            );
        }
        Err(e) => {
            eprintln!("[loop-engine serve] rehydrate failed (continuing with empty index): {e:#}");
        }
    }

    // LLM client for handlers that need generation (today:
    // `memory.compress`). Mirrors the embedder's `from_env` — defaults
    // to local Ollama (`LOOP_LLM_*` overrides). Construction is
    // config-only (no network), so a failure here is a genuine
    // misconfiguration; we log + continue with `None` so the rest of the
    // RPC surface stays available (memory.compress alone degrades to a
    // clear "no LLM configured" error).
    let llm: Option<Arc<dyn LlmClient>> = match OpenAiCompatibleLlm::from_env() {
        Ok(client) => Some(Arc::new(client)),
        Err(e) => {
            eprintln!(
                "[loop-engine serve] LLM adapter unavailable (memory.compress disabled): {e}"
            );
            None
        }
    };

    Ok(ServeState {
        ctx,
        storage,
        embedder,
        vector_index,
        llm,
    })
}

/// Drive one JSON-RPC connection to completion. Generic over the
/// reader + writer so stdio and UDS share the exact same dispatch
/// loop — the only difference is the transport constructed at the
/// call site. Returns when the reader EOFs (stdin closed for stdio,
/// peer half-closed for UDS) or on write/serialize error.
async fn serve_one_connection<R, W>(mut reader: R, mut writer: W, state: &ServeState) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            // EOF: stdin closed (stdio) or peer half-closed (UDS).
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        let response = process_line(
            &line,
            &state.ctx,
            state.storage.as_ref(),
            state.embedder.as_ref(),
            state.vector_index.as_ref(),
            state.llm.as_deref(),
        )
        .await;
        let json = serde_json::to_string(&response)
            .unwrap_or_else(|e| format!(r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"response serialize failed: {e}"}}}}"#));
        writer.write_all(json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
}

async fn serve_stdio(state: ServeState) -> Result<()> {
    eprintln!(
        "[loop-engine serve] ready on stdio (lessons: create/recall/promote/discard; memories: create/search/get)"
    );
    let reader = BufReader::new(tokio::io::stdin());
    let writer = tokio::io::stdout();
    serve_one_connection(reader, writer, &state).await
}

/// Bind a Unix-domain-socket listener at `path` and serve connections
/// concurrently. Per-connection `tokio::spawn` is mandatory: without
/// it two concurrent acquires would serialize globally and the daemon
/// would offer no win over per-process stdio spawn (audit T.1.BB).
#[cfg(unix)]
async fn serve_uds(state: ServeState, path: PathBuf) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;

    // Clean up a stale socket file from a previous engine that crashed
    // without unlinking. Refuse to delete anything that isn't a socket
    // — defends against a path-collision footgun where a user points
    // `--socket` at a regular file or directory.
    if path.exists() {
        let meta = std::fs::metadata(&path)
            .with_context(|| format!("stat existing path {}", path.display()))?;
        if meta.file_type().is_socket() {
            std::fs::remove_file(&path)
                .with_context(|| format!("removing stale socket {}", path.display()))?;
        } else {
            anyhow::bail!(
                "--socket path {} exists and is not a socket; refusing to remove",
                path.display()
            );
        }
    }
    // Ensure the parent dir exists (matches opensquid singleton's
    // `~/.opensquid/loop-engine.sock` convention — `~/.opensquid` is
    // created by `paths::ensure_loop_dirs` above, but a caller passing
    // an arbitrary path may not have created it).
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir for {}", path.display()))?;
    }

    let listener = tokio::net::UnixListener::bind(&path)
        .with_context(|| format!("binding UDS at {}", path.display()))?;
    eprintln!(
        "[loop-engine serve] ready on UDS at {} (lessons: create/recall/promote/discard; memories: create/search/get)",
        path.display()
    );

    let state = Arc::new(state);
    loop {
        let (stream, _addr) = listener
            .accept()
            .await
            .context("UDS accept failed (listener closed?)")?;
        let state = state.clone();
        // CRITICAL per T.1.BB — per-connection spawn for cross-
        // connection concurrency. Within a single connection,
        // serve_one_connection still processes sequentially (one
        // request → one response → next request).
        tokio::spawn(async move {
            let (read_half, write_half) = stream.into_split();
            let reader = BufReader::new(read_half);
            if let Err(e) = serve_one_connection(reader, write_half, &state).await {
                warn!(error = %e, "UDS connection serve loop ended with error");
            }
        });
    }
}

/// Windows fallback: UDS is Unix-only. Named-pipe support is tracked
/// as a follow-up (the opensquid singleton throws a clear error on
/// `process.platform === 'win32'` for symmetry).
#[cfg(not(unix))]
async fn serve_uds(_state: ServeState, _path: PathBuf) -> Result<()> {
    anyhow::bail!(
        "loop-engine: --socket UDS mode is not supported on Windows yet; \
         use `loop-engine serve` (stdio) until named-pipe support lands"
    )
}

async fn process_line(
    line: &str,
    ctx: &Context,
    storage: &dyn Storage,
    embedder: &dyn Embedder,
    vector_index: &dyn VectorIndex,
    llm: Option<&dyn LlmClient>,
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
        llm,
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
    llm: Option<&dyn LlmClient>,
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
        "manifest.assemble" => {
            manifest_assemble(params, ctx, storage, embedder, vector_index).await
        }
        "memory.update" => memory_update_method(params, ctx, storage, embedder, vector_index).await,
        "memory.delete" => memory_delete_method(params, ctx, storage, vector_index).await,
        "memory.compress" => {
            memory_compress_method(params, ctx, storage, embedder, vector_index, llm).await
        }
        "memory.consolidate" => {
            memory_consolidate_method(params, ctx, storage, embedder, vector_index, llm).await
        }
        "memory.recompute_citations" => {
            memory_recompute_citations_method(params, ctx, storage).await
        }
        "task.log_phase" => task_log_phase(params, ctx, storage).await,
        "task.get_ledger" => task_get_ledger(params, ctx, storage).await,
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
        Ok(loaded) => {
            // Surface the promoted lesson's cited MEMORY ids (the
            // `EvidenceRef::Memory` evidence refs). A host uses these to
            // nominate the cited memories as compression candidates —
            // compression rides the SAME wedge cadence (compression =
            // lesson-formation). Quote evidence refs are excluded; only
            // typed memory references are relevant.
            let cited_memory_ids = cited_memory_ids(loaded.frontmatter.causal_narrative.as_ref());
            Ok(json!({
                "ok": true,
                "id": p.id,
                "gate": "passed",
                "status": "promoted",
                "from": loaded.status_dir,
                "cited_memory_ids": cited_memory_ids,
            }))
        }
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
            )));
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

/// Extract the cited MEMORY ids from a lesson's causal narrative — the
/// `EvidenceRef::Memory` evidence refs (typed memory references), in
/// order. `Quote` refs are skipped; `None` narrative → empty. A host
/// uses these to nominate the cited memories as compression candidates.
fn cited_memory_ids(narrative: Option<&crate::engine::yaml::CausalNarrative>) -> Vec<String> {
    narrative
        .map(|cn| {
            cn.evidence_refs
                .iter()
                .filter_map(|r| match r {
                    crate::engine::yaml::EvidenceRef::Memory(id) => Some(id.as_str().to_string()),
                    crate::engine::yaml::EvidenceRef::Quote(_) => None,
                })
                .collect()
        })
        .unwrap_or_default()
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
            // Surfaced so a host can enforce the user-immunity invariant
            // BEFORE issuing a force-delete (force bypasses the engine
            // guard). `derived_from` lets the host reason about the
            // compression chain (e.g. distinguish a compressed memory
            // from a raw one).
            "consumed_by_user_lessons": mem.frontmatter.consumed_by_user_lessons,
            "derived_from": mem.frontmatter.derived_from
                .iter()
                .map(MemoryId::as_str)
                .collect::<Vec<_>>(),
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
        if let Some(filter) = &scope_filter
            && !filter.matches(&mem.frontmatter.scope)
        {
            continue;
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

#[derive(Deserialize)]
struct MemoryCompressParams {
    /// Explicit window — the memory ids to compress into one summary.
    /// The host (which owns the satisfaction probe + candidate
    /// collection) decides the window; the engine never auto-selects.
    ids: Vec<String>,
    /// Max LLM output tokens. Defaults to `CompressionConfig::default`.
    #[serde(default)]
    max_tokens: Option<usize>,
    /// Sampling temperature. Defaults to `CompressionConfig::default`.
    #[serde(default)]
    temperature: Option<f32>,
}

/// `memory.compress` — pure exposure of the existing [`compress`] lib
/// fn. Resolves the explicit id window, invokes the LLM summarizer,
/// embeds + persists the new compressed memory `Mc` (with
/// `derived_from = [ids...]` + summed citation counters), and returns
/// its identity. Does NOT delete predecessors — that is the host's
/// verified, gated terminal step (D-Cx4); the engine only mints `Mc`.
///
/// Requires an LLM. A daemon built without a configured adapter
/// returns a clear error (the engine crate ships no production
/// adapter; see [`ServeState::llm`]).
///
/// [`compress`]: crate::engine::memory::compress
async fn memory_compress_method(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
    embedder: &dyn Embedder,
    vector_index: &dyn VectorIndex,
    llm: Option<&dyn LlmClient>,
) -> std::result::Result<Value, DispatchError> {
    let p: MemoryCompressParams =
        serde_json::from_value(params).map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    if p.ids.is_empty() {
        return Err(DispatchError::InvalidParams(
            "ids required (non-empty)".into(),
        ));
    }
    let llm = llm.ok_or_else(|| {
        DispatchError::Other(anyhow!(
            "memory.compress requires an LLM adapter, but none is configured on this engine"
        ))
    })?;

    let window = CompressionWindow::Ids(p.ids.iter().map(|s| MemoryId::new(s.as_str())).collect());
    let mut config = CompressionConfig::default();
    if let Some(mt) = p.max_tokens {
        config.max_tokens = mt;
    }
    if let Some(t) = p.temperature {
        config.temperature = t;
    }

    match memory_compress_fn(
        ctx,
        storage,
        llm,
        embedder,
        vector_index,
        window,
        &config,
        Utc::now(),
    )
    .await
    {
        Ok(mc) => Ok(json!({
            "id": mc.frontmatter.id.as_str(),
            "description": mc.frontmatter.description,
            "derived_from": mc.frontmatter.derived_from
                .iter()
                .map(MemoryId::as_str)
                .collect::<Vec<_>>(),
            "consumed_by_user_lessons": mc.frontmatter.consumed_by_user_lessons,
        })),
        // The LLM judged the window too thin/contradictory to summarize,
        // OR the window resolved empty. A graceful no-op signal, not a
        // defect — surface it as InvalidParams so the host can skip the
        // window without treating it as an engine fault.
        Err(EngineError::CompressionInsufficientInput) => Err(DispatchError::InvalidParams(
            "compression refused: insufficient input for the supplied window".into(),
        )),
        Err(e) => Err(DispatchError::Other(anyhow!("memory.compress failed: {e}"))),
    }
}

#[derive(Deserialize)]
struct MemoryConsolidateParams {
    /// Explicit window — the memory ids to consolidate. The host
    /// decides the window (satisfaction probe + candidate collection);
    /// the engine never auto-selects.
    ids: Vec<String>,
    /// Max LLM output tokens for the compression step. Defaults to
    /// `CompressionConfig::default`.
    #[serde(default)]
    max_tokens: Option<usize>,
    /// Sampling temperature for the compression step. Defaults to
    /// `CompressionConfig::default`.
    #[serde(default)]
    temperature: Option<f32>,
    /// Recall-replay top-k. The compressed memory must surface within
    /// the top-`recall_k` for EACH predecessor's representative query.
    /// Defaults to [`DEFAULT_CONSOLIDATE_RECALL_K`].
    #[serde(default)]
    recall_k: Option<usize>,
}

/// `memory.consolidate` — the atomic, fail-closed safe-compression op
/// (universal memory integrity). Compresses the window into one
/// summary `Mc`, VERIFIES via recall-replay that `Mc` preserves each
/// predecessor's recall, and ONLY THEN force-deletes the NON-immune
/// predecessors (user-cited ones are kept). Any error or verify-miss
/// → deletes NOTHING and returns `verified: false` with the minted
/// `Mc` id so the host can keep `Mc` alongside the originals + emit a
/// drift event.
///
/// This is the engine-side home of the D2 safety contract (verify +
/// immunity + fail-closed) — previously enforced across multiple
/// host RPC round-trips, now race-free inside one engine call.
///
/// Requires an LLM (compression step). Wire result:
/// `{ mc_id, deleted: [...], kept_immune: [...], verified: bool }`.
///
/// [`consolidate`]: crate::engine::memory::consolidate
async fn memory_consolidate_method(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
    embedder: &dyn Embedder,
    vector_index: &dyn VectorIndex,
    llm: Option<&dyn LlmClient>,
) -> std::result::Result<Value, DispatchError> {
    let p: MemoryConsolidateParams =
        serde_json::from_value(params).map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    if p.ids.is_empty() {
        return Err(DispatchError::InvalidParams(
            "ids required (non-empty)".into(),
        ));
    }
    let llm = llm.ok_or_else(|| {
        DispatchError::Other(anyhow!(
            "memory.consolidate requires an LLM adapter, but none is configured on this engine"
        ))
    })?;

    let ids: Vec<MemoryId> = p.ids.iter().map(|s| MemoryId::new(s.as_str())).collect();
    let mut config = CompressionConfig::default();
    if let Some(mt) = p.max_tokens {
        config.max_tokens = mt;
    }
    if let Some(t) = p.temperature {
        config.temperature = t;
    }
    let recall_k = p.recall_k.unwrap_or(DEFAULT_CONSOLIDATE_RECALL_K);

    match memory_consolidate_fn(
        ctx,
        storage,
        llm,
        embedder,
        vector_index,
        ids,
        &config,
        recall_k,
        Utc::now(),
    )
    .await
    {
        Ok(outcome) => Ok(json!({
            "mc_id": outcome.mc_id,
            "deleted": outcome.deleted,
            "kept_immune": outcome.kept_immune,
            "verified": outcome.verified,
        })),
        // The window was too thin/contradictory to summarize, OR
        // resolved empty — a graceful no-op signal, not a defect.
        // Surface as InvalidParams so the host skips the window without
        // treating it as an engine fault. (Mirrors memory.compress.)
        Err(EngineError::CompressionInsufficientInput) => Err(DispatchError::InvalidParams(
            "consolidation refused: insufficient input for the supplied window".into(),
        )),
        Err(e) => Err(DispatchError::Other(anyhow!(
            "memory.consolidate failed: {e}"
        ))),
    }
}

/// `memory.recompute_citations` — pure exposure of
/// [`recompute_citation_counts`]. Walks all lessons + the
/// predecessor→compressor index, repairs every memory's
/// `consumed_by_user_lessons` counter to ground truth, and reports
/// drift. A host runs this after a batch of compressions / deletions
/// to confirm citation integrity survived the chain.
///
/// [`recompute_citation_counts`]: crate::engine::memory::recompute_citation_counts
async fn memory_recompute_citations_method(
    _params: Value,
    ctx: &Context,
    storage: &dyn Storage,
) -> std::result::Result<Value, DispatchError> {
    let stats = memory_recompute_citation_counts(ctx, storage)
        .await
        .map_err(|e| DispatchError::Other(anyhow!("memory.recompute_citations failed: {e}")))?;
    Ok(json!({
        "lessons_scanned": stats.lessons_scanned,
        "memories_recomputed": stats.memories_recomputed,
        "counters_repaired": stats.counters_repaired,
        "orphan_citations": stats.orphan_citations,
    }))
}

// ---- v1.4: manifest.assemble ---------------------------------------

#[derive(Deserialize)]
struct ManifestAssembleParams {
    /// Statuses to include. Default ["active"] (TS parity).
    #[serde(default)]
    statuses: Option<Vec<String>>,
    /// Max lessons to return. Default 5.
    #[serde(default)]
    lesson_limit: Option<usize>,
    /// Body preview char count. Default 200.
    #[serde(default)]
    body_preview_len: Option<usize>,
    /// Run the wedge gate per lesson + attach `gate` field. Default true.
    #[serde(default = "default_annotate_with_gate")]
    annotate_with_gate: bool,
    /// Bump `applied_count` + `last_applied_at` per surfaced lesson.
    /// Default true (TS parity). Set false for strictly read-only callers.
    #[serde(default = "default_record_applied")]
    record_applied: bool,
    /// Text memory query. When present + embedder is reachable, populates
    /// the `memories` section. Defaults to no memory search.
    #[serde(default)]
    memory_query: Option<String>,
    /// Max memories to return when `memory_query` populated. Default 5.
    #[serde(default)]
    memory_limit: Option<usize>,
    /// Optional scope filter on the memory section. Same wire shape as
    /// `memory.search`.
    #[serde(default)]
    memory_scope_filter: Option<ScopeFilterWire>,
}

fn default_annotate_with_gate() -> bool {
    true
}
fn default_record_applied() -> bool {
    true
}

/// `manifest.assemble` — central RAG-style assembly. Returns the agent's
/// "system context payload": active lessons (deterministic-sorted, gate-
/// annotated, applied_count bumped) + an optional memory recall for the
/// current task. This is what a host like Hermes calls to get "what
/// rules apply right now" in one shot, instead of stitching together
/// `lesson.list` + `memory.search`.
///
/// Skill / persona / team active sections ship as empty arrays in v1.4
/// — the SessionState plumbing for activation IDs is deferred to a
/// later release (Route A from the v0.5c pre-research). Out-of-scope
/// fields stay forward-compatible: callers wildcard-match.
async fn manifest_assemble(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
    embedder: &dyn Embedder,
    vector_index: &dyn VectorIndex,
) -> std::result::Result<Value, DispatchError> {
    use crate::engine::manifest::{AssembleConfig, assemble};
    use crate::engine::memory::MemoryQuery as EngineMemoryQuery;
    use crate::engine::yaml::LessonStatus as EngineLessonStatus;

    let p: ManifestAssembleParams =
        serde_json::from_value(params).map_err(|e| DispatchError::InvalidParams(e.to_string()))?;

    let statuses = match p.statuses {
        Some(v) if !v.is_empty() => {
            let mut out = Vec::with_capacity(v.len());
            for s in v {
                let parsed = match s.as_str() {
                    "pending" => EngineLessonStatus::Pending,
                    "active" => EngineLessonStatus::Active,
                    "promoted" => EngineLessonStatus::Promoted,
                    "discarded" => EngineLessonStatus::Discarded,
                    "superseded" => EngineLessonStatus::Superseded,
                    other => {
                        return Err(DispatchError::InvalidParams(format!(
                            "unknown lesson status '{other}'"
                        )));
                    }
                };
                out.push(parsed);
            }
            out
        }
        _ => vec![EngineLessonStatus::Active],
    };

    let scope_filter = match p.memory_scope_filter {
        Some(w) => Some(w.into_engine()?),
        None => None,
    };

    let memory_query = p.memory_query.and_then(|q| {
        let trimmed = q.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(EngineMemoryQuery::Text(trimmed))
        }
    });

    let defaults = AssembleConfig::default();
    let config = AssembleConfig {
        statuses,
        lesson_limit: p.lesson_limit.unwrap_or(defaults.lesson_limit),
        body_preview_len: p.body_preview_len.unwrap_or(defaults.body_preview_len),
        annotate_with_gate: p.annotate_with_gate,
        record_applied: p.record_applied,
        promotion_config: defaults.promotion_config,
        memory_query,
        memory_limit: p.memory_limit.unwrap_or(defaults.memory_limit),
        memory_scope_filter: scope_filter,
    };

    let manifest = assemble(
        ctx,
        storage,
        Some(embedder),
        Some(vector_index),
        None, // SessionState — deferred to a later release
        &config,
        Utc::now(),
    )
    .await
    .map_err(|e| DispatchError::Other(anyhow!("manifest.assemble failed: {e}")))?;

    Ok(serialize_manifest(&manifest))
}

/// Hand-serialize the manifest because the engine's `Manifest` family
/// is intentionally NOT serde-derived (every counter would become a
/// SemVer hinge). Wire-shape decisions live here and are reviewable.
fn serialize_manifest(m: &crate::engine::manifest::Manifest) -> Value {
    let active_lessons: Vec<Value> = m
        .active_lessons
        .iter()
        .map(|l| {
            let mut obj = serde_json::Map::new();
            obj.insert("id".into(), Value::String(l.id.clone()));
            obj.insert("description".into(), Value::String(l.description.clone()));
            obj.insert(
                "status".into(),
                Value::String(l.status.as_str().to_string()),
            );
            obj.insert("body_preview".into(), Value::String(l.body_preview.clone()));
            obj.insert("applied_count".into(), json!(l.applied_count));
            obj.insert(
                "last_applied_at".into(),
                l.last_applied_at
                    .map(|t| Value::String(t.to_rfc3339()))
                    .unwrap_or(Value::Null),
            );
            obj.insert(
                "target_skill".into(),
                l.target_skill
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            if let Some(gate) = &l.gate {
                obj.insert("gate".into(), serialize_gate_decision(gate));
            }
            Value::Object(obj)
        })
        .collect();

    let memories: Vec<Value> = m
        .memories
        .iter()
        .map(|mref| {
            json!({
                "id": mref.id.as_str(),
                "description": mref.description,
                "body_preview": mref.body_preview,
                "similarity": (mref.similarity * 1000.0).round() / 1000.0,
            })
        })
        .collect();

    json!({
        "active_lessons": active_lessons,
        "memories": memories,
        "active_skills": [],
        "active_personas": [],
        "active_teams": [],
        "assembly_stats": {
            "assembled_at": m.assembly_stats.assembled_at.to_rfc3339(),
            "total_listed": m.assembly_stats.total_listed,
            "skipped_count": m.assembly_stats.skipped_count,
            "gate_skip_count": m.assembly_stats.gate_skip_count,
            "record_applied_failures": m.assembly_stats.record_applied_failures,
            "memories_returned": m.assembly_stats.memories_returned,
            "memory_search_failures": m.assembly_stats.memory_search_failures,
            "session_section_skips": m.assembly_stats.session_section_skips,
        },
    })
}

/// Render a [`GateDecision`] as `{kind, reason_count}`. Non-exhaustive
/// enum from the engine — full per-reason serialization is deferred to
/// a follow-up release; the summary tag is enough for callers to render
/// "wedge passed N checks" / "wedge blocked on N reasons" without us
/// committing to a per-variant wire shape today.
fn serialize_gate_decision(g: &crate::engine::lessons::gate::GateDecision) -> Value {
    use crate::engine::lessons::gate::GateDecision;
    match g {
        GateDecision::Promote { reasons } => json!({
            "kind": "promote",
            "reason_count": reasons.len(),
        }),
        GateDecision::Block { reasons } => json!({
            "kind": "block",
            "reason_count": reasons.len(),
        }),
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

// ---------------------------------------------------------------------
// Phase ledger handlers (v0.5)
//
// The engine stores per-(task, phase) entries. Consumers use the
// ledger to gate downstream operations on workflow phase coverage —
// e.g. "block `git commit` if audit + post_research haven't been
// logged for the active task." The engine itself doesn't know what
// "active task" means; consumers supply task_id as an opaque string.
//
// Pre-#166 these handlers also required a `session_id`, but writes
// (MCP server PID) and reads (Claude Code session UUID) used
// different id surfaces — the ledger was effectively unreadable
// across the two. Dropped in favor of per-task scoping.
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct TaskLogPhaseParams {
    task_id: String,
    /// Snake-case phase identifier — must match one of the seven values
    /// `Phase` recognizes (`pre_research`, `learn`, `code`, `test`,
    /// `audit`, `post_research`, `fix`). Stringly-typed at the wire so
    /// older callers with extra phases don't break compile; rejected
    /// at parse with `InvalidParams`.
    phase: String,
    #[serde(default)]
    note: Option<String>,
}

async fn task_log_phase(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
) -> std::result::Result<Value, DispatchError> {
    let p: TaskLogPhaseParams =
        serde_json::from_value(params).map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    let phase = Phase::parse(&p.phase).ok_or_else(|| {
        DispatchError::InvalidParams(format!(
            "unknown phase {:?} — expected one of pre_research, learn, code, test, audit, post_research, fix",
            p.phase
        ))
    })?;
    let written = phase_ledger::log_phase(ctx, storage, &p.task_id, phase, p.note.as_deref())
        .await
        .map_err(ledger_to_dispatch)?;
    Ok(json!({
        "ok": true,
        "task_id": p.task_id,
        "phase": phase.as_str(),
        // false on idempotent re-log (entry already existed) so callers
        // can distinguish "first time" from "noop".
        "newly_recorded": written,
    }))
}

#[derive(Deserialize)]
struct TaskGetLedgerParams {
    task_id: String,
}

async fn task_get_ledger(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
) -> std::result::Result<Value, DispatchError> {
    let p: TaskGetLedgerParams =
        serde_json::from_value(params).map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    let entries = phase_ledger::get_ledger(ctx, storage, &p.task_id)
        .await
        .map_err(ledger_to_dispatch)?;
    let phases: Vec<&'static str> = entries.iter().map(|e| e.phase.as_str()).collect();
    let entry_json: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "phase": e.phase.as_str(),
                "logged_at": e.logged_at,
                "note": e.note,
            })
        })
        .collect();
    Ok(json!({
        "task_id": p.task_id,
        "phases_logged": phases,
        "entries": entry_json,
    }))
}

fn ledger_to_dispatch(err: LedgerError) -> DispatchError {
    match err {
        LedgerError::InvalidId { .. } | LedgerError::NoteTooLong { .. } => {
            DispatchError::InvalidParams(err.to_string())
        }
        LedgerError::Storage(e) => DispatchError::Other(anyhow!(e)),
        LedgerError::MalformedEntry { .. } => DispatchError::Other(anyhow!(err.to_string())),
    }
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
    fn cited_memory_ids_extracts_only_memory_refs_in_order() {
        // build_narrative maps `mem-`-prefixed evidence to
        // EvidenceRef::Memory, everything else to Quote.
        let narrative = build_narrative(
            &[
                "mem-aaa00001".to_string(),
                "a free-text quote".to_string(),
                "mem-bbb00002".to_string(),
            ],
            Authorship::User,
            "2026-05-27T00:00:00Z",
        );
        let ids = cited_memory_ids(narrative.as_ref());
        assert_eq!(ids, vec!["mem-aaa00001", "mem-bbb00002"]);
    }

    #[test]
    fn cited_memory_ids_empty_for_no_narrative_or_no_memory_refs() {
        assert!(cited_memory_ids(None).is_empty());
        let quotes_only = build_narrative(
            &["just a quote".to_string()],
            Authorship::User,
            "2026-05-27T00:00:00Z",
        );
        assert!(cited_memory_ids(quotes_only.as_ref()).is_empty());
    }

    // ---------------------------------------------------------------------
    // CMP.1 — `memory.compress` + `memory.recompute_citations` dispatch
    // arms. Exercise the RPC path end-to-end through `dispatch()` with a
    // `MockLlmClient` injected (the engine ships no production adapter).
    // ---------------------------------------------------------------------

    async fn seed_memory(
        storage: &Arc<dyn Storage>,
        vector_index: &HnswVectorIndex,
        embedder: &MockEmbedder,
        id: &str,
        desc: &str,
        body: &str,
    ) {
        crate::engine::memory::store::insert(
            &Context::single_user_local(),
            storage.as_ref(),
            embedder,
            vector_index,
            MemoryId::new(id),
            desc,
            body,
            Utc::now(),
        )
        .await
        .expect("seed insert");
    }

    #[tokio::test]
    async fn memory_compress_dispatch_returns_mc_with_derived_from_and_summed_citations() {
        use crate::engine::llm::{Generation, MockLlmClient};
        use crate::engine::memory::store::increment_citation_count;

        let ctx = Context::single_user_local();
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        let embedder = MockEmbedder::new(4).with_response(vec![vec![1.0, 0.0, 0.0, 0.0]]);
        seed_memory(
            &storage,
            &vector_index,
            &embedder,
            "mem-rpc00001",
            "first",
            "body one",
        )
        .await;
        seed_memory(
            &storage,
            &vector_index,
            &embedder,
            "mem-rpc00002",
            "second",
            "body two",
        )
        .await;
        // M1 cited twice, M2 cited once → Mc should sum to 3.
        for _ in 0..2 {
            increment_citation_count(&ctx, storage.as_ref(), &MemoryId::new("mem-rpc00001"))
                .await
                .unwrap();
        }
        increment_citation_count(&ctx, storage.as_ref(), &MemoryId::new("mem-rpc00002"))
            .await
            .unwrap();

        let llm = MockLlmClient::default().with_response(
            Generation::new(r#"{"description":"gist","content":"compressed"}"#).with_parsed(
                serde_json::from_str(r#"{"description":"gist","content":"compressed"}"#).unwrap(),
            ),
        );

        let result = dispatch(
            "memory.compress",
            json!({ "ids": ["mem-rpc00001", "mem-rpc00002"] }),
            &ctx,
            storage.as_ref(),
            &embedder,
            &vector_index,
            Some(&llm),
        )
        .await
        .expect("memory.compress should succeed via dispatch");

        let derived: Vec<String> = result["derived_from"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(derived, vec!["mem-rpc00001", "mem-rpc00002"]);
        assert_eq!(result["consumed_by_user_lessons"], json!(3));
        assert!(
            result["id"].as_str().unwrap().starts_with("mem-c-"),
            "Mc id should carry the compressed infix"
        );
    }

    #[tokio::test]
    async fn memory_compress_dispatch_missing_ids_is_invalid_params() {
        let ctx = Context::single_user_local();
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        let embedder = MockEmbedder::new(4);
        let err = dispatch(
            "memory.compress",
            json!({}),
            &ctx,
            storage.as_ref(),
            &embedder,
            &vector_index,
            None,
        )
        .await
        .expect_err("missing ids must fail");
        assert!(matches!(err, DispatchError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn memory_compress_dispatch_without_llm_errors_clearly() {
        let ctx = Context::single_user_local();
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        let embedder = MockEmbedder::new(4);
        let err = dispatch(
            "memory.compress",
            json!({ "ids": ["mem-rpc00001"] }),
            &ctx,
            storage.as_ref(),
            &embedder,
            &vector_index,
            None,
        )
        .await
        .expect_err("no LLM configured must fail");
        match err {
            DispatchError::Other(e) => assert!(
                e.to_string().contains("LLM"),
                "error should name the missing LLM: {e}"
            ),
            other => panic!("expected Other(no-LLM), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn memory_recompute_citations_dispatch_returns_stats() {
        let ctx = Context::single_user_local();
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        let embedder = MockEmbedder::new(4);
        // Empty store → a clean sweep with zero drift.
        let result = dispatch(
            "memory.recompute_citations",
            json!({}),
            &ctx,
            storage.as_ref(),
            &embedder,
            &vector_index,
            None,
        )
        .await
        .expect("memory.recompute_citations should succeed");
        assert_eq!(result["counters_repaired"], json!(0));
        assert_eq!(result["orphan_citations"], json!(0));
        assert!(result["lessons_scanned"].is_number());
    }

    // ---------------------------------------------------------------------
    // CMP.4 (revised) — `memory.consolidate` dispatch arm. The atomic
    // verify+gated-delete op now lives in the engine. These tests drive
    // it through `dispatch()` and assert the D2 contract end-to-end:
    // verified → non-immune deleted / immune kept; verify-miss →
    // fail-closed (delete nothing, verified:false).
    // ---------------------------------------------------------------------

    /// Seed a memory at a chosen embedding vector (one-shot embedder per
    /// insert so the index entry is exactly that vector).
    async fn seed_at(
        storage: &Arc<dyn Storage>,
        vector_index: &HnswVectorIndex,
        id: &str,
        desc: &str,
        body: &str,
        vec: Vec<f32>,
    ) {
        let emb = MockEmbedder::new(4).with_response(vec![vec]);
        crate::engine::memory::store::insert(
            &Context::single_user_local(),
            storage.as_ref(),
            &emb,
            vector_index,
            MemoryId::new(id),
            desc,
            body,
            Utc::now(),
        )
        .await
        .expect("seed insert");
    }

    fn axis(dim: usize, ax: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; dim];
        v[ax] = 1.0;
        v
    }

    #[tokio::test]
    async fn memory_consolidate_dispatch_verified_deletes_non_immune() {
        use crate::engine::llm::{Generation, MockLlmClient};

        let ctx = Context::single_user_local();
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        seed_at(
            &storage,
            &vector_index,
            "mem-cns00001",
            "alpha",
            "a",
            axis(4, 0),
        )
        .await;
        seed_at(
            &storage,
            &vector_index,
            "mem-cns00002",
            "beta",
            "b",
            axis(4, 0),
        )
        .await;

        let llm = MockLlmClient::default().with_response(
            Generation::new(r#"{"description":"gist","content":"compressed"}"#).with_parsed(
                serde_json::from_str(r#"{"description":"gist","content":"compressed"}"#).unwrap(),
            ),
        );
        // Mc colinear with predecessors; recall queries colinear too →
        // Mc surfaces → verified.
        let embedder = MockEmbedder::new(4)
            .with_response(vec![axis(4, 0)])
            .with_response(vec![axis(4, 0)])
            .with_response(vec![axis(4, 0)]);

        let result = dispatch(
            "memory.consolidate",
            json!({ "ids": ["mem-cns00001", "mem-cns00002"] }),
            &ctx,
            storage.as_ref(),
            &embedder,
            &vector_index,
            Some(&llm),
        )
        .await
        .expect("memory.consolidate should succeed via dispatch");

        assert_eq!(result["verified"], json!(true));
        let mut deleted: Vec<String> = result["deleted"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        deleted.sort();
        assert_eq!(deleted, vec!["mem-cns00001", "mem-cns00002"]);
        assert!(result["kept_immune"].as_array().unwrap().is_empty());
        assert!(result["mc_id"].as_str().unwrap().starts_with("mem-c-"));
    }

    #[tokio::test]
    async fn memory_consolidate_dispatch_verify_fail_deletes_nothing() {
        use crate::engine::llm::{Generation, MockLlmClient};

        let ctx = Context::single_user_local();
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        seed_at(
            &storage,
            &vector_index,
            "mem-cnf00001",
            "alpha",
            "a",
            axis(4, 0),
        )
        .await;
        seed_at(
            &storage,
            &vector_index,
            "mem-cnf00002",
            "beta",
            "b",
            axis(4, 0),
        )
        .await;

        let llm = MockLlmClient::default().with_response(
            Generation::new(r#"{"description":"gist","content":"compressed"}"#).with_parsed(
                serde_json::from_str(r#"{"description":"gist","content":"compressed"}"#).unwrap(),
            ),
        );
        // Mc orthogonal; recall_k=1 → Mc not the top hit → verify miss.
        let embedder = MockEmbedder::new(4)
            .with_response(vec![axis(4, 1)])
            .with_response(vec![axis(4, 0)]);

        let result = dispatch(
            "memory.consolidate",
            json!({ "ids": ["mem-cnf00001", "mem-cnf00002"], "recall_k": 1 }),
            &ctx,
            storage.as_ref(),
            &embedder,
            &vector_index,
            Some(&llm),
        )
        .await
        .expect("dispatch returns Ok with verified:false (not an error)");

        assert_eq!(result["verified"], json!(false));
        assert!(result["deleted"].as_array().unwrap().is_empty());
        assert!(result["kept_immune"].as_array().unwrap().is_empty());
        assert!(result["mc_id"].as_str().unwrap().starts_with("mem-c-"));
    }

    #[tokio::test]
    async fn memory_consolidate_dispatch_missing_ids_is_invalid_params() {
        let ctx = Context::single_user_local();
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        let embedder = MockEmbedder::new(4);
        let err = dispatch(
            "memory.consolidate",
            json!({}),
            &ctx,
            storage.as_ref(),
            &embedder,
            &vector_index,
            None,
        )
        .await
        .expect_err("missing ids must fail");
        assert!(matches!(err, DispatchError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn memory_consolidate_dispatch_without_llm_errors_clearly() {
        let ctx = Context::single_user_local();
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let vector_index = HnswVectorIndex::new(4);
        let embedder = MockEmbedder::new(4);
        let err = dispatch(
            "memory.consolidate",
            json!({ "ids": ["mem-cns00001"] }),
            &ctx,
            storage.as_ref(),
            &embedder,
            &vector_index,
            None,
        )
        .await
        .expect_err("no LLM configured must fail");
        match err {
            DispatchError::Other(e) => assert!(
                e.to_string().contains("LLM"),
                "error should name the missing LLM: {e}"
            ),
            other => panic!("expected Other(no-LLM), got {other:?}"),
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

    // ---------------------------------------------------------------------
    // Phase ledger RPC handler tests
    //
    // Exercises the round-trip through the wire layer: serde_json::Value
    // params in, JSON Value response out. Storage is in-memory (no
    // tmpdir / no daemon spawn). Covers happy-path log + readback,
    // unknown phase, invalid id rejection, and idempotent re-log.
    // ---------------------------------------------------------------------

    use crate::engine::storage::MemoryStorage;

    fn test_ctx_and_storage() -> (Context, MemoryStorage) {
        (Context::single_user_local(), MemoryStorage::default())
    }

    #[tokio::test]
    async fn task_log_phase_records_and_reads_back() {
        let (ctx, storage) = test_ctx_and_storage();
        let params = json!({
            "task_id": "task-127",
            "phase": "audit",
            "note": "13 retroactive findings",
        });
        let resp = task_log_phase(params, &ctx, &storage).await.unwrap();
        assert_eq!(resp["ok"], json!(true));
        assert_eq!(resp["newly_recorded"], json!(true));
        assert_eq!(resp["phase"], json!("audit"));

        let ledger = task_get_ledger(json!({"task_id": "task-127"}), &ctx, &storage)
            .await
            .unwrap();
        let phases = ledger["phases_logged"].as_array().unwrap();
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0], json!("audit"));
        let entries = ledger["entries"].as_array().unwrap();
        assert_eq!(entries[0]["note"], json!("13 retroactive findings"));
    }

    #[tokio::test]
    async fn task_log_phase_idempotent_relog() {
        let (ctx, storage) = test_ctx_and_storage();
        let params = json!({
            "task_id": "task-127",
            "phase": "code",
        });
        let first = task_log_phase(params.clone(), &ctx, &storage)
            .await
            .unwrap();
        assert_eq!(first["newly_recorded"], json!(true));

        // Re-logging the same phase is a noop: returns ok with
        // newly_recorded=false, original entry preserved.
        let second = task_log_phase(params, &ctx, &storage).await.unwrap();
        assert_eq!(second["ok"], json!(true));
        assert_eq!(second["newly_recorded"], json!(false));

        let ledger = task_get_ledger(json!({"task_id": "task-127"}), &ctx, &storage)
            .await
            .unwrap();
        assert_eq!(ledger["phases_logged"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn task_log_phase_rejects_unknown_phase() {
        let (ctx, storage) = test_ctx_and_storage();
        let err = task_log_phase(
            json!({
                "task_id": "t1",
                "phase": "review",
            }),
            &ctx,
            &storage,
        )
        .await
        .expect_err("unknown phase must fail");
        assert!(matches!(err, DispatchError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn task_log_phase_rejects_path_traversal_in_task_id() {
        let (ctx, storage) = test_ctx_and_storage();
        let err = task_log_phase(
            json!({
                "task_id": "../etc/passwd",
                "phase": "audit",
            }),
            &ctx,
            &storage,
        )
        .await
        .expect_err("dotdot must fail");
        assert!(matches!(err, DispatchError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn task_get_ledger_empty_for_unknown_task() {
        let (ctx, storage) = test_ctx_and_storage();
        let ledger = task_get_ledger(json!({"task_id": "never-logged"}), &ctx, &storage)
            .await
            .unwrap();
        assert_eq!(ledger["phases_logged"].as_array().unwrap().len(), 0);
        assert_eq!(ledger["entries"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn task_log_phase_multiple_phases_distinct_files() {
        let (ctx, storage) = test_ctx_and_storage();
        for phase in [
            "pre_research",
            "learn",
            "code",
            "test",
            "audit",
            "post_research",
            "fix",
        ] {
            task_log_phase(
                json!({
                    "task_id": "t1",
                    "phase": phase,
                }),
                &ctx,
                &storage,
            )
            .await
            .unwrap();
        }
        let ledger = task_get_ledger(json!({"task_id": "t1"}), &ctx, &storage)
            .await
            .unwrap();
        assert_eq!(ledger["phases_logged"].as_array().unwrap().len(), 7);
    }

    // Audit HIGH fix: prefix-list collision between sibling task_ids
    // in MemoryStorage (which uses `starts_with`). Without the trailing
    // slash in `phase_ledger_task_prefix`, a query for "t1" would
    // silently include entries from "t1-extra". This test would have
    // caught the bug pre-commit.
    #[tokio::test]
    async fn task_get_ledger_isolates_sibling_task_ids() {
        let (ctx, storage) = test_ctx_and_storage();
        task_log_phase(json!({"task_id": "t1", "phase": "audit"}), &ctx, &storage)
            .await
            .unwrap();
        task_log_phase(
            json!({"task_id": "t1-extra", "phase": "code"}),
            &ctx,
            &storage,
        )
        .await
        .unwrap();
        let ledger = task_get_ledger(json!({"task_id": "t1"}), &ctx, &storage)
            .await
            .unwrap();
        let phases = ledger["phases_logged"].as_array().unwrap();
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0], json!("audit"));
    }

    // (#166) Deleted `task_get_ledger_isolates_sessions`: the ledger is
    // now per-task, not per-(session, task). Cross-session collision on
    // the same task_id is INTENDED — a task that spans multiple sessions
    // (e.g. after `/resume`) must accumulate phases across them, which
    // is the whole reason the session_id segment was dropped.

    // Audit MED fix: get_ledger must sort by logged_at (deterministic
    // chronological order regardless of backend incidental ordering).
    #[tokio::test]
    async fn task_get_ledger_sorts_by_logged_at() {
        let (ctx, storage) = test_ctx_and_storage();
        let log_order = [
            "pre_research",
            "code",
            "audit",
            "fix",
            "learn",
            "post_research",
            "test",
        ];
        for phase in log_order {
            task_log_phase(json!({"task_id": "t1", "phase": phase}), &ctx, &storage)
                .await
                .unwrap();
            // Sleep 2ms so timestamps differentiate (RFC3339 ms precision).
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        let ledger = task_get_ledger(json!({"task_id": "t1"}), &ctx, &storage)
            .await
            .unwrap();
        let phases: Vec<&str> = ledger["phases_logged"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(phases, log_order.to_vec());
    }

    // Audit MED fix: note length capped (prevent unbounded growth).
    #[tokio::test]
    async fn task_log_phase_rejects_oversized_note() {
        let (ctx, storage) = test_ctx_and_storage();
        let huge = "a".repeat(16 * 1024 + 1);
        let err = task_log_phase(
            json!({
                "task_id": "t1",
                "phase": "audit",
                "note": huge,
            }),
            &ctx,
            &storage,
        )
        .await
        .expect_err("oversized note must fail");
        assert!(matches!(err, DispatchError::InvalidParams(_)));
    }

    // ---------------------------------------------------------------------
    // T.4 — transport tests for `serve_one_connection` + UDS path.
    //
    // The dispatch loop now lives in a generic `serve_one_connection<R, W>`
    // shared by stdio and UDS. These tests exercise both transport
    // shapes against a synthetic `ServeState` (in-memory storage +
    // `MockEmbedder` + empty HNSW index) so we don't depend on a live
    // Ollama or filesystem `.opensquid` for the unit suite.
    //
    // Coverage:
    //  1. `stdio_regression_ping_roundtrip` — refactor regression guard.
    //     The previous inline stdio loop was: read line → process_line →
    //     write json + '\n' → flush. Drive the same shape through a
    //     `tokio::io::duplex` pair and assert byte-identical
    //     ping output (shape, no trailing junk).
    //  2. `uds_ping_roundtrip` — bind a UDS in a tempdir, connect, send
    //     ping, assert ok+version.
    //  3. `uds_concurrent_connections_no_id_crosstalk` — open two
    //     connections, send pings with distinct ids in interleaved
    //     order, assert each connection only sees its own response
    //     (per-connection `tokio::spawn` invariant).
    //  4. `uds_refuses_non_socket_path` — bind path pointing at a
    //     regular file → returns the structured "refusing to remove"
    //     error rather than silently nuking the file.
    //  5. `uds_cleans_stale_socket_file` — pre-create a socket file
    //     (left over by a crashed engine) and verify bind succeeds
    //     (stale-cleanup path).
    // ---------------------------------------------------------------------

    use crate::engine::embedding::mock::MockEmbedder;
    use crate::engine::vector::HnswVectorIndex;

    /// Build a minimal `ServeState` for transport tests. Uses in-memory
    /// storage + a mock embedder so `ping` and `manifest.assemble` work
    /// without filesystem or network.
    fn test_serve_state() -> ServeState {
        let ctx = Context::single_user_local();
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder::new(8));
        let vector_index: Arc<dyn VectorIndex> = Arc::new(HnswVectorIndex::new(8));
        ServeState {
            ctx,
            storage,
            embedder,
            vector_index,
            llm: None,
        }
    }

    /// Refactor regression guard: drive `serve_one_connection` through
    /// an in-memory duplex pair and verify the stdio-shape response
    /// (line-framed JSON-RPC 2.0, exactly one '\n' terminator per
    /// response) is unchanged from the pre-T.4 inline loop.
    #[tokio::test]
    async fn stdio_regression_ping_roundtrip() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt as _, BufReader as TokioBufReader, duplex};

        let state = test_serve_state();

        // Two duplex pairs: one for the "client → server" direction,
        // one for "server → client". The serve loop reads from
        // `server_read` and writes to `server_write`; the test writes
        // requests on `client_write` and reads responses on
        // `client_read`.
        let (client_write, server_read) = duplex(4096);
        let (server_write, mut client_read) = duplex(4096);

        // Spawn the serve loop. It runs until the reader EOFs, which
        // happens when we drop `client_write` below.
        let serve_handle = tokio::spawn(async move {
            let reader = TokioBufReader::new(server_read);
            serve_one_connection(reader, server_write, &state).await
        });

        // Send a ping.
        let mut client_write = client_write;
        client_write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"params\":{}}\n")
            .await
            .unwrap();
        client_write.flush().await.unwrap();

        // Read one line of response — the legacy contract is "exactly
        // one JSON object terminated by '\n'."
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            client_read.read_exact(&mut byte).await.unwrap();
            buf.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        let line = std::str::from_utf8(&buf).unwrap().trim_end_matches('\n');
        let parsed: Value = serde_json::from_str(line).expect("response must be valid JSON");
        assert_eq!(parsed["jsonrpc"], json!("2.0"));
        assert_eq!(parsed["id"], json!(1));
        assert_eq!(parsed["result"]["ok"], json!(true));
        assert!(parsed["result"]["version"].is_string());

        // Drop the writer → server sees EOF → loop returns Ok.
        drop(client_write);
        let result = serve_handle.await.unwrap();
        result.expect("serve_one_connection should exit cleanly on EOF");
    }

    /// Pick an ephemeral UDS path under the test's tempdir. macOS limits
    /// the path to 104 bytes; `tempfile::tempdir` paths under `/var/...`
    /// are normally well under that, so we don't add a special guard.
    fn ephemeral_sock(dir: &std::path::Path, label: &str) -> PathBuf {
        // Keep filename short — macOS 104-byte limit.
        dir.join(format!("{label}.sock"))
    }

    /// UDS happy path: bind, connect, send ping, get response back.
    #[tokio::test]
    async fn uds_ping_roundtrip() {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader as TokioBufReader};
        use tokio::net::UnixStream;

        let tmp = tempfile::tempdir().unwrap();
        let sock_path = ephemeral_sock(tmp.path(), "ping");

        let state = test_serve_state();
        let sock_for_serve = sock_path.clone();
        let serve_handle = tokio::spawn(async move { serve_uds(state, sock_for_serve).await });

        // Wait for the listener to appear (bind happens in serve_uds
        // before accept; the path's existence is the readiness signal
        // — same primitive the opensquid singleton uses).
        let start = std::time::Instant::now();
        while !sock_path.exists() {
            if start.elapsed() > std::time::Duration::from_secs(5) {
                panic!("UDS path never appeared at {}", sock_path.display());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let stream = UnixStream::connect(&sock_path).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = TokioBufReader::new(read_half);
        write_half
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"ping\",\"params\":{}}\n")
            .await
            .unwrap();
        write_half.flush().await.unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["id"], json!(42));
        assert_eq!(parsed["result"]["ok"], json!(true));

        // Half-close the write side; the serve task's per-connection
        // spawn returns Ok and the listener keeps accepting. The
        // listener itself only stops on serve_handle abort.
        drop(write_half);
        drop(reader);

        serve_handle.abort();
    }

    /// Two concurrent connections each send pings with distinct ids;
    /// each connection must only see responses for its own id (the
    /// per-connection `tokio::spawn` invariant — no shared writer, no
    /// id crosstalk).
    #[tokio::test]
    async fn uds_concurrent_connections_no_id_crosstalk() {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader as TokioBufReader};
        use tokio::net::UnixStream;

        let tmp = tempfile::tempdir().unwrap();
        let sock_path = ephemeral_sock(tmp.path(), "concur");

        let state = test_serve_state();
        let sock_for_serve = sock_path.clone();
        let serve_handle = tokio::spawn(async move { serve_uds(state, sock_for_serve).await });

        // Wait for bind.
        let start = std::time::Instant::now();
        while !sock_path.exists() {
            if start.elapsed() > std::time::Duration::from_secs(5) {
                panic!("UDS path never appeared at {}", sock_path.display());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        async fn ping_with_id(path: &std::path::Path, id: u64) -> Value {
            let stream = UnixStream::connect(path).await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = TokioBufReader::new(read_half);
            let req = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"ping\",\"params\":{{}}}}\n"
            );
            write_half.write_all(req.as_bytes()).await.unwrap();
            write_half.flush().await.unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            serde_json::from_str(line.trim_end()).unwrap()
        }

        let (a, b) = tokio::join!(
            ping_with_id(&sock_path, 1001),
            ping_with_id(&sock_path, 2002),
        );
        assert_eq!(a["id"], json!(1001));
        assert_eq!(b["id"], json!(2002));
        assert_eq!(a["result"]["ok"], json!(true));
        assert_eq!(b["result"]["ok"], json!(true));

        serve_handle.abort();
    }

    /// Pointing `--socket` at a regular file must fail loudly, not
    /// silently delete the file. The check is `file_type().is_socket()`.
    #[cfg(unix)]
    #[tokio::test]
    async fn uds_refuses_non_socket_path() {
        let tmp = tempfile::tempdir().unwrap();
        let bad_path = tmp.path().join("not-a-socket.txt");
        std::fs::write(&bad_path, b"hello").unwrap();

        let state = test_serve_state();
        let err = serve_uds(state, bad_path.clone())
            .await
            .expect_err("regular file at --socket path must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not a socket"),
            "error should mention path is not a socket; got: {msg}"
        );
        // File must still exist — we refused to remove it.
        assert!(bad_path.exists(), "non-socket file must not be deleted");
    }

    /// A stale socket file (left over after an engine crash) should be
    /// unlinked and the new bind should succeed.
    #[cfg(unix)]
    #[tokio::test]
    async fn uds_cleans_stale_socket_file() {
        use tokio::net::UnixListener;

        let tmp = tempfile::tempdir().unwrap();
        let sock_path = ephemeral_sock(tmp.path(), "stale");

        // Pre-create a socket by binding + immediately dropping the
        // listener — the socket file persists until unlink.
        let pre_listener = UnixListener::bind(&sock_path).unwrap();
        drop(pre_listener);
        assert!(sock_path.exists(), "pre-test socket file should exist");

        // serve_uds should detect it as a socket, unlink, then bind.
        let state = test_serve_state();
        let sock_for_serve = sock_path.clone();
        let serve_handle = tokio::spawn(async move { serve_uds(state, sock_for_serve).await });

        // Wait for new bind to succeed (path will reappear after our
        // unlink + bind).
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(5) {
            if sock_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            sock_path.exists(),
            "serve_uds should rebind at the same path after cleaning stale file"
        );

        serve_handle.abort();
    }
}
