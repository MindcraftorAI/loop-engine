//! `LlmClient::generate` output shape.
//!
//! Phase D D-D6: single `Generation` struct, NO streaming, token usage
//! surfaced via `Option<TokenUsage>`. Streaming = additive future
//! method (`generate_stream`), not Phase D.

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Generation {
    /// Always populated. For `ResponseFormat::JsonSchema` requests this
    /// is the raw model output (which adapters MUST also populate even
    /// when `parsed` is set — useful for debugging / audit trails).
    pub text: String,
    /// Populated when the request used `ResponseFormat::JsonSchema`
    /// AND the adapter successfully extracted structured output.
    /// `None` for `ResponseFormat::Text` OR when the adapter couldn't
    /// honor structured output (callers see this as
    /// [`super::LlmError::InvalidOutput`] at the engine boundary).
    pub parsed: Option<serde_json::Value>,
    /// Why generation stopped. `Length` indicates `max_tokens` cap hit
    /// — engine consumers should treat as a soft signal that the
    /// output may be truncated.
    pub finish_reason: FinishReason,
    /// Provider-reported token usage. `None` when the provider doesn't
    /// expose usage (older APIs, on-prem models). Engine does NOT
    /// aggregate or log; monolith adapters do that downstream.
    pub usage: Option<TokenUsage>,
}

/// Why a `Generation` stopped producing tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FinishReason {
    /// Natural completion (model emitted EOS, hit a stop sequence,
    /// finished the structured-output schema).
    Stop,
    /// Hit `max_tokens` — output may be truncated.
    Length,
    /// Provider's content filter blocked the response.
    ContentFilter,
    /// Provider-specific reason that doesn't map cleanly to the above.
    /// Carrying the raw string preserves provider info without forcing
    /// the engine to enumerate every adapter's vocabulary.
    Other(String),
}

/// Provider-reported token usage for one generation call.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_reason_other_carries_payload() {
        let r = FinishReason::Other("provider-quirk".into());
        match r {
            FinishReason::Other(s) => assert_eq!(s, "provider-quirk"),
            _ => panic!("expected Other"),
        }
    }

    #[test]
    fn token_usage_eq_works() {
        let a = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
        };
        let b = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
        };
        assert_eq!(a, b);
    }
}
