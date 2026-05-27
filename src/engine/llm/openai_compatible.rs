//! OpenAI-compatible LlmClient — production impl.
//!
//! Mirrors [`crate::engine::embedding::openai_compatible`]: one in-crate
//! production adapter against the `/v1/chat/completions` endpoint shape
//! that OpenAI cloud, Ollama (local, e.g. `qwen2.5`), LM Studio, and
//! vLLM all speak:
//!
//!   POST {base_url}/chat/completions
//!   { "model": "<MODEL>", "messages": [ {role, content}, ... ],
//!     "max_tokens": N, "temperature": T,
//!     "response_format": { "type": "json_object" } }   // for JsonSchema
//!
//!   200 OK
//!   { "choices": [ { "message": { "content": "..." },
//!                    "finish_reason": "stop" } ],
//!     "usage": { "prompt_tokens": N, "completion_tokens": M } }
//!
//! Differences across providers (auth header presence, structured-output
//! support) are configured at construction / handled below.
//!
//! **JsonSchema handling**: not every provider honors the full
//! `json_schema` response_format. For maximal compatibility we request
//! `{ "type": "json_object" }` (Ollama + OpenAI both honor it) AND
//! append a "respond with JSON matching this schema" instruction to the
//! system prompt, then parse the content into `Generation::parsed`. A
//! provider that returns non-JSON content for a JsonSchema request
//! surfaces as `LlmError::InvalidOutput` at the engine boundary.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::engine::context::Context;
use crate::engine::llm::{
    FinishReason, GenerateRequest, Generation, LlmClient, LlmError, ResponseFormat, TokenUsage,
    sealed,
};

/// HTTP timeout for a single generation. Generous — a local 14B/32B
/// model on a laptop can take many seconds, especially cold.
const HTTP_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleLlm {
    base_url: String,
    model: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl OpenAiCompatibleLlm {
    /// Generic constructor. `base_url` is the endpoint prefix that
    /// `/chat/completions` appends to (e.g. `http://localhost:11434/v1`
    /// for Ollama, `https://api.openai.com/v1` for OpenAI cloud).
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Result<Self, LlmError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .map_err(LlmError::transport)?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            api_key,
            client,
        })
    }

    /// Construct from environment, mirroring
    /// [`crate::engine::embedding::OpenAiCompatibleEmbedder::from_env`]:
    /// - `OPENSQUID_LLM_URL` (default `http://localhost:11434/v1`)
    /// - `OPENSQUID_LLM_MODEL` (default `qwen2.5:14b-instruct-q4_K_M`)
    /// - `OPENSQUID_LLM_API_KEY` (default none — Ollama needs no auth)
    pub fn from_env() -> Result<Self, LlmError> {
        let url = std::env::var("OPENSQUID_LLM_URL")
            .unwrap_or_else(|_| "http://localhost:11434/v1".to_string());
        let model = std::env::var("OPENSQUID_LLM_MODEL")
            .unwrap_or_else(|_| "qwen2.5:14b-instruct-q4_K_M".to_string());
        let api_key = std::env::var("OPENSQUID_LLM_API_KEY").ok();
        Self::new(url, model, api_key)
    }
}

impl sealed::Sealed for OpenAiCompatibleLlm {}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    max_tokens: usize,
    temperature: f32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<Value>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    usage: Option<ChatUsage>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct ChatUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
}

fn map_finish_reason(raw: Option<&str>) -> FinishReason {
    match raw {
        Some("stop") | None => FinishReason::Stop,
        Some("length") => FinishReason::Length,
        Some("content_filter") => FinishReason::ContentFilter,
        Some(other) => FinishReason::Other(other.to_string()),
    }
}

#[async_trait]
impl LlmClient for OpenAiCompatibleLlm {
    async fn generate(
        &self,
        _ctx: &Context,
        request: &GenerateRequest,
    ) -> Result<Generation, LlmError> {
        // Translate the response format. For JsonSchema we request
        // json_object mode AND fold the schema into the system prompt
        // (broadest compatibility — Ollama doesn't reliably honor the
        // full json_schema response_format yet).
        let (response_format, schema_instruction): (Option<Value>, Option<String>) =
            match &request.response_format {
                ResponseFormat::Text => (None, None),
                ResponseFormat::JsonSchema { schema, name } => (
                    Some(json!({ "type": "json_object" })),
                    Some(format!(
                        "Respond ONLY with a single JSON object (no prose, no code fences) \
                         matching this JSON schema named \"{name}\":\n{schema}"
                    )),
                ),
            };

        // Compose the system prompt: caller system + optional schema
        // instruction.
        let system = match (&request.system, &schema_instruction) {
            (Some(s), Some(instr)) => Some(format!("{s}\n\n{instr}")),
            (Some(s), None) => Some(s.clone()),
            (None, Some(instr)) => Some(instr.clone()),
            (None, None) => None,
        };

        let mut messages: Vec<ChatMessage> = Vec::new();
        if let Some(s) = &system {
            messages.push(ChatMessage {
                role: "system",
                content: s,
            });
        }
        messages.push(ChatMessage {
            role: "user",
            content: &request.prompt,
        });

        let model = request.model.as_deref().unwrap_or(&self.model);
        let body = ChatRequest {
            model,
            messages,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            stop: request.stop_sequences.clone(),
            response_format,
        };

        let url = format!("{}/chat/completions", self.base_url);
        let mut req = self.client.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req.send().await.map_err(LlmError::transport)?;
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(LlmError::RateLimited);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(LlmError::InvalidOutput(format!("HTTP {status}: {text}")));
        }

        let parsed_resp: ChatResponse = resp.json().await.map_err(LlmError::transport)?;
        let choice = parsed_resp
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| LlmError::InvalidOutput("no choices in response".into()))?;
        let content = choice.message.content.unwrap_or_default();

        let usage = parsed_resp.usage.map(|u| {
            TokenUsage::new(
                u.prompt_tokens.unwrap_or(0),
                u.completion_tokens.unwrap_or(0),
            )
        });

        let mut generation = Generation::new(content.clone())
            .with_finish_reason(map_finish_reason(choice.finish_reason.as_deref()));
        if let Some(u) = usage {
            generation = generation.with_usage(u);
        }

        // For a JsonSchema request, parse the content into `parsed`.
        // A model that returned non-JSON surfaces as InvalidOutput.
        if matches!(request.response_format, ResponseFormat::JsonSchema { .. }) {
            let value: Value = serde_json::from_str(content.trim()).map_err(|e| {
                LlmError::InvalidOutput(format!("expected JSON content, got parse error: {e}"))
            })?;
            generation = generation.with_parsed(value);
        }

        Ok(generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_defaults_to_local_ollama() {
        // Don't assert on the live env; just that construction succeeds
        // with the documented defaults when the vars are absent.
        // (Other tests in the suite may set these — read + restore.)
        let prior_url = std::env::var("OPENSQUID_LLM_URL").ok();
        let prior_model = std::env::var("OPENSQUID_LLM_MODEL").ok();
        unsafe {
            std::env::remove_var("OPENSQUID_LLM_URL");
            std::env::remove_var("OPENSQUID_LLM_MODEL");
        }
        let llm = OpenAiCompatibleLlm::from_env().expect("from_env");
        assert_eq!(llm.base_url, "http://localhost:11434/v1");
        assert_eq!(llm.model, "qwen2.5:14b-instruct-q4_K_M");
        unsafe {
            if let Some(u) = prior_url {
                std::env::set_var("OPENSQUID_LLM_URL", u);
            }
            if let Some(m) = prior_model {
                std::env::set_var("OPENSQUID_LLM_MODEL", m);
            }
        }
    }

    #[test]
    fn new_trims_trailing_slash() {
        let llm = OpenAiCompatibleLlm::new("http://x/v1/", "m", None).unwrap();
        assert_eq!(llm.base_url, "http://x/v1");
    }

    #[test]
    fn map_finish_reason_known_and_unknown() {
        assert_eq!(map_finish_reason(Some("stop")), FinishReason::Stop);
        assert_eq!(map_finish_reason(None), FinishReason::Stop);
        assert_eq!(map_finish_reason(Some("length")), FinishReason::Length);
        assert!(matches!(
            map_finish_reason(Some("weird")),
            FinishReason::Other(_)
        ));
    }
}
