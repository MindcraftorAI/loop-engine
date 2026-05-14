//! `LlmClient::generate` input shape.
//!
//! Phase D D-D5: minimal provider-agnostic request — single `prompt`
//! plus optional `system`, `#[non_exhaustive]` for forward-compat. The
//! engine's consumers (narrative, future skill eval) are single-shot
//! artifact producers; multi-turn `messages: Vec<Message>` is out of
//! scope and would balloon the SemVer surface.

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GenerateRequest {
    /// User-role content. Always present.
    pub prompt: String,
    /// Optional system prompt. Adapters translate to provider shape
    /// (Anthropic `system`, OpenAI `messages[role=system]`, ...).
    pub system: Option<String>,
    /// Hard cap on generated tokens — defends against runaway models
    /// burning budget.
    pub max_tokens: usize,
    /// Sampling temperature. Engine consumers default to `0.0` for
    /// structured outputs; allow per-request override.
    pub temperature: f32,
    /// Stop sequences. Adapters that support them translate; adapters
    /// that don't MAY ignore (NOT error).
    pub stop_sequences: Vec<String>,
    /// Structured-output request shape. See [`ResponseFormat`].
    pub response_format: ResponseFormat,
    /// Per-call model override. `None` = adapter uses its configured
    /// default. Engine never opens a `model()` accessor on the trait.
    pub model: Option<String>,
}

impl Default for GenerateRequest {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            system: None,
            max_tokens: 1024,
            temperature: 0.0,
            stop_sequences: Vec::new(),
            response_format: ResponseFormat::Text,
            model: None,
        }
    }
}

/// Structured-output shape for [`GenerateRequest::response_format`].
///
/// Phase D D-D5: enum (not boolean flag) so the schema travels with
/// the request — adapters can translate to provider-specific
/// structured-output APIs (Anthropic `response_format`, OpenAI
/// `response_format = json_schema`) or fall back to in-prompt
/// instruction.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ResponseFormat {
    /// Plain text response. `Generation::text` populated; `parsed`
    /// is `None`.
    Text,
    /// Provider should attempt structured JSON output matching the
    /// supplied schema. Adapters that can't honor MAY fall back to
    /// "ask for JSON in the prompt" OR return
    /// [`super::LlmError::UnsupportedFeature`].
    JsonSchema {
        /// JSON Schema (draft 2020-12 or sub-set). Adapter passes
        /// through to the provider OR translates.
        schema: serde_json::Value,
        /// Human-readable schema name, for debugging / provider
        /// metadata fields.
        name: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_text_response_with_zero_temp() {
        let r = GenerateRequest::default();
        assert!(matches!(r.response_format, ResponseFormat::Text));
        assert_eq!(r.temperature, 0.0);
        assert_eq!(r.max_tokens, 1024);
        assert!(r.system.is_none());
        assert!(r.stop_sequences.is_empty());
    }

    #[test]
    fn json_schema_response_format_carries_schema() {
        let schema = serde_json::json!({"type": "object"});
        let rf = ResponseFormat::JsonSchema {
            schema: schema.clone(),
            name: "TestShape".into(),
        };
        if let ResponseFormat::JsonSchema { schema: s, name } = rf {
            assert_eq!(s, schema);
            assert_eq!(name, "TestShape");
        } else {
            panic!("expected JsonSchema");
        }
    }
}
