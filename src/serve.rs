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
//!
//! Memory, manifest assembly, and skill/persona/team ops land in
//! a follow-on serve cycle once we wire an embedder.

use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use bytes::Bytes;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::warn;

use crate::engine::context::Context;
use crate::engine::error::EngineError;
use crate::engine::lessons::gate::PromotionConfig;
use crate::engine::lessons::loader::get_by_id as load_lesson;
use crate::engine::lessons::transitions::{discard, promote};
use crate::engine::paths;
use crate::engine::storage::filesystem::LocalFsStorage;
use crate::engine::storage::{Storage, StorageKey};
use crate::engine::yaml::writer::serialize_lesson_frontmatter;
use crate::engine::yaml::{combine_frontmatter, split_frontmatter_normalized};
use crate::engine::yaml::{reader::parse_lesson_frontmatter, Authorship, LessonFrontmatter, LessonStatus};

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

fn err_with_data(id: Option<Value>, code: i32, message: impl Into<String>, data: Value) -> Response {
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

    eprintln!("[loop-engine serve] ready on stdio (4 lesson methods + ping)");

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin).lines();

    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = process_line(&line, &ctx, storage.as_ref()).await;
        let json = serde_json::to_string(&response)
            .unwrap_or_else(|e| format!(r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"response serialize failed: {e}"}}}}"#));
        stdout.write_all(json.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }

    Ok(())
}

async fn process_line(line: &str, ctx: &Context, storage: &dyn Storage) -> Response {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => return err(None, -32700, format!("parse error: {e}")),
    };
    if req.jsonrpc != "2.0" {
        return err(req.id, -32600, "jsonrpc must be \"2.0\"");
    }
    match dispatch(&req.method, req.params, ctx, storage).await {
        Ok(value) => ok(req.id, value),
        Err(DispatchError::MethodNotFound) => err(req.id, -32601, format!("method not found: {}", req.method)),
        Err(DispatchError::InvalidParams(msg)) => err(req.id, -32602, format!("invalid params: {msg}")),
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
        Err(DispatchError::NotFound(id)) => err_with_data(
            req.id,
            -32002,
            "not found",
            json!({ "id": id }),
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
) -> std::result::Result<Value, DispatchError> {
    match method {
        "ping" => Ok(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") })),
        "lesson.create" => lesson_create(params, ctx, storage).await,
        "lesson.recall" => lesson_recall(params, ctx, storage).await,
        "lesson.promote" => lesson_promote(params, ctx, storage).await,
        "lesson.discard" => lesson_discard(params, ctx, storage).await,
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
}

async fn lesson_create(
    params: Value,
    ctx: &Context,
    storage: &dyn Storage,
) -> std::result::Result<Value, DispatchError> {
    let p: LessonCreateParams = serde_json::from_value(params)
        .map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    if p.description.trim().is_empty() || p.body.trim().is_empty() {
        return Err(DispatchError::InvalidParams("description and body required".into()));
    }
    let authored_by = parse_authorship(p.authored_by.as_deref());
    let id = new_lesson_id();
    let created_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);

    let fm = LessonFrontmatter {
        id: id.clone(),
        description: p.description.clone(),
        status: LessonStatus::Pending,
        created_at: created_at.clone(),
        causal_narrative: build_narrative(&p.evidence, authored_by, &created_at),
        target_skill: None,
        source_feedback_ids: None,
        applied_count: 0,
        last_applied_at: None,
        thumbs_up_count: 0,
        thumbs_down_count: 0,
        external_signal_sources: vec![],
        promotion_eligible_at: None,
        superseded_by: None,
        superseded_at: None,
        ingest_provenance: None,
        authored_by,
        updated_at: None,
    };

    let yaml = serialize_lesson_frontmatter(&fm);
    let content = combine_frontmatter(&yaml, &p.body);
    let key = StorageKey::lesson(ctx, "pending", &id);
    storage
        .put(&key, Bytes::from(content))
        .await
        .map_err(|e| DispatchError::Other(anyhow!("storage put failed: {e}")))?;

    Ok(json!({
        "id": id,
        "status": "pending",
        "authored_by": authorship_str(authored_by),
        "created_at": created_at,
    }))
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
    let p: LessonRecallParams = serde_json::from_value(params)
        .map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
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
            let haystack = format!("{} {}", fm.description, split.body);
            let sim = score(&p.query, &haystack);
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
    let p: LessonPromoteParams = serde_json::from_value(params)
        .map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
    // Probe existence first so we surface NotFound cleanly.
    if load_lesson(ctx, storage, &p.id)
        .await
        .map_err(|e| DispatchError::Other(anyhow!("load failed: {e}")))?
        .is_none()
    {
        return Err(DispatchError::NotFound(p.id));
    }
    match promote(
        ctx,
        storage,
        &p.id,
        &PromotionConfig::default(),
        Utc::now(),
    )
    .await
    {
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
    let p: LessonDiscardParams = serde_json::from_value(params)
        .map_err(|e| DispatchError::InvalidParams(e.to_string()))?;
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
        _ => Authorship::Llm,
    }
}

fn authorship_str(a: Authorship) -> &'static str {
    match a {
        Authorship::User => "user",
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

// ---- Naive text-match scoring -------------------------------------

fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .filter(|t| t.len() > 1)
        .map(|s| s.to_string())
        .collect()
}

fn score(query: &str, haystack: &str) -> f32 {
    let q_tokens: std::collections::HashSet<_> = tokenize(query).into_iter().collect();
    if q_tokens.is_empty() {
        return 0.0;
    }
    let h_tokens: std::collections::HashSet<_> = tokenize(haystack).into_iter().collect();
    let overlap = q_tokens.iter().filter(|t| h_tokens.contains(*t)).count() as f32;
    let token_score = overlap / q_tokens.len() as f32;
    let substring_bonus = if haystack.to_lowercase().contains(&query.to_lowercase()) {
        0.3
    } else {
        0.0
    };
    (token_score + substring_bonus).min(1.0)
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
