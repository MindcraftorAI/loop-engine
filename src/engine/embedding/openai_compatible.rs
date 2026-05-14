//! OpenAI-compatible Embedder — production impl.
//!
//! Covers OpenAI cloud (text-embedding-3-small/large), Ollama (local
//! Qwen3-Embedding-4B per `project_loop_embedder_choice` memory),
//! TEI (Hugging Face Text Embeddings Inference), and LM Studio. All
//! four speak the same `/v1/embeddings` endpoint shape:
//!
//!   POST {base_url}/embeddings
//!   { "model": "<model>", "input": ["text", ...] }
//!
//!   200 OK
//!   { "data": [ { "embedding": [f32; N] }, ... ] }
//!
//! Differences across providers (auth header presence, dimensions,
//! batch limits) are configured at construction.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::engine::context::Context;
use crate::engine::embedding::{sealed, Embedder, EmbeddingError};

/// HTTP timeout for a single batch. Conservative — local Ollama on a
/// laptop can take ~1s per text on a cold model; bumped to 60s to
/// absorb cold-start without false timeouts.
const HTTP_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleEmbedder {
    base_url: String,
    model: String,
    api_key: Option<String>,
    dimensions: usize,
    client: reqwest::Client,
}

impl OpenAiCompatibleEmbedder {
    /// Generic constructor. `base_url` is the endpoint prefix that
    /// `/embeddings` appends to (e.g. `http://localhost:11434/v1` for
    /// Ollama, `https://api.openai.com/v1` for OpenAI cloud).
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
        dimensions: usize,
    ) -> Result<Self, EmbeddingError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .map_err(EmbeddingError::transport)?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            api_key,
            dimensions,
            client,
        })
    }

    /// Recommended default: local Ollama running Qwen3-Embedding-4B
    /// (Apache 2.0, 2560 dims) per `project_loop_embedder_choice`.
    pub fn ollama_qwen3_4b() -> Result<Self, EmbeddingError> {
        Self::new(
            "http://localhost:11434/v1",
            "qwen3-embedding:4b",
            None,
            2560,
        )
    }

    /// Construct from environment with the Qwen-via-Ollama defaults.
    /// Overrides:
    /// - `OPENSQUID_EMBEDDER_URL` (default `http://localhost:11434/v1`)
    /// - `OPENSQUID_EMBEDDER_MODEL` (default `qwen3-embedding:4b`)
    /// - `OPENSQUID_EMBEDDER_DIMENSIONS` (default `2560`)
    /// - `OPENSQUID_EMBEDDER_API_KEY` (default none — Ollama doesn't
    ///   need auth)
    pub fn from_env() -> Result<Self, EmbeddingError> {
        let url = std::env::var("OPENSQUID_EMBEDDER_URL")
            .unwrap_or_else(|_| "http://localhost:11434/v1".to_string());
        let model = std::env::var("OPENSQUID_EMBEDDER_MODEL")
            .unwrap_or_else(|_| "qwen3-embedding:4b".to_string());
        let dimensions: usize = std::env::var("OPENSQUID_EMBEDDER_DIMENSIONS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2560);
        let api_key = std::env::var("OPENSQUID_EMBEDDER_API_KEY").ok();
        Self::new(url, model, api_key, dimensions)
    }
}

impl sealed::Sealed for OpenAiCompatibleEmbedder {}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbeddingItem>,
}

#[derive(Deserialize)]
struct EmbeddingItem {
    embedding: Vec<f32>,
}

#[async_trait]
impl Embedder for OpenAiCompatibleEmbedder {
    async fn embed(
        &self,
        _ctx: &Context,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!("{}/embeddings", self.base_url);
        let mut req = self.client.post(&url).json(&EmbedRequest {
            model: &self.model,
            input: texts,
        });
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req.send().await.map_err(EmbeddingError::transport)?;
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(EmbeddingError::RateLimited);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(EmbeddingError::InvalidOutput(format!(
                "HTTP {status}: {body}"
            )));
        }
        let parsed: EmbedResponse = resp.json().await.map_err(EmbeddingError::transport)?;
        if parsed.data.len() != texts.len() {
            return Err(EmbeddingError::InvalidOutput(format!(
                "expected {} embeddings, got {}",
                texts.len(),
                parsed.data.len()
            )));
        }
        for (i, item) in parsed.data.iter().enumerate() {
            if item.embedding.len() != self.dimensions {
                return Err(EmbeddingError::InvalidOutput(format!(
                    "index {i}: vector len {} != configured dim {}",
                    item.embedding.len(),
                    self.dimensions
                )));
            }
        }
        Ok(parsed.data.into_iter().map(|i| i.embedding).collect())
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}
