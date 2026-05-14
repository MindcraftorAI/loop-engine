//! Causal-narrative generation.
//!
//! Phase D D-D9: produces a [`CausalNarrative`] from a `NarrativeContext`
//! by calling an [`LlmClient`]. The prompt template + JSON schema live
//! IN the engine because the wedge invariants are baked into them
//! together — the gate (Phase B) catches violations at promotion time,
//! and parse-time validation (D-D10 defense-in-depth) catches them
//! immediately so we never persist a violating narrative.
//!
//! Side-effect free: produces the struct; caller persists via the
//! lesson loader / writer. Mirrors `gate::check_promotion_gate` purity
//! (NOT `manifest::assemble` which performs writes).
//!
//! The LLM may also REFUSE — return `{"error": "insufficient_context"}`
//! — when the inputs don't justify a concrete narrative. That surfaces
//! as `EngineError::NarrativeInsufficientContext`, distinct from
//! `EngineError::Llm(LlmError::ValidationFailed)` which is a defect in
//! the model output.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::engine::context::Context;
use crate::engine::error::EngineError;
use crate::engine::llm::{
    GenerateRequest, LlmClient, LlmError, ResponseFormat,
};
use crate::engine::yaml::{CausalNarrative, Confidence, GeneratedBy};

/// Inputs the caller assembles for [`generate`]. The engine doesn't
/// own session-window or transcript-extraction logic — the caller
/// (monolith MCP tool, future skill evaluator) provides them.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct NarrativeContext {
    /// Lesson description (from the candidate frontmatter). Required.
    pub description: String,
    /// Optional source feedback — the user message or event that
    /// produced the candidate sentiment signal. Helps the LLM ground
    /// `evidence_refs` in real text.
    pub source_feedback: Option<String>,
    /// Optional recent-session-window excerpt (~20 tool turns).
    /// Provides the corpus the LLM should quote in `evidence_refs`.
    pub transcript_excerpt: Option<String>,
}

impl NarrativeContext {
    /// Construct with only the required `description`. External crates
    /// (the monolith MCP adapter, tests outside the engine) need this
    /// because `#[non_exhaustive]` forbids struct-literal construction
    /// from outside the defining crate.
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            source_feedback: None,
            transcript_excerpt: None,
        }
    }

    /// Builder: attach source-feedback context.
    #[must_use]
    pub fn with_source_feedback(mut self, source_feedback: impl Into<String>) -> Self {
        self.source_feedback = Some(source_feedback.into());
        self
    }

    /// Builder: attach transcript-excerpt context.
    #[must_use]
    pub fn with_transcript_excerpt(mut self, transcript_excerpt: impl Into<String>) -> Self {
        self.transcript_excerpt = Some(transcript_excerpt.into());
        self
    }
}

/// Configuration knobs for [`generate`]. `Default` ships D-D9 values.
/// `#[non_exhaustive]` — future cycles add knobs without SemVer break.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct NarrativeConfig {
    /// Max LLM output tokens. Default 1024 (TS parity).
    pub max_tokens: usize,
    /// Sampling temperature. Default 0.0 — structured output should
    /// be deterministic.
    pub temperature: f32,
    /// Optional system-prompt override. `None` = use engine default
    /// (none — the wedge invariants are in the user prompt, not the
    /// system prompt). Per OQ-D2, the USER prompt template is NOT
    /// overridable in Phase D.
    pub system_override: Option<String>,
}

impl Default for NarrativeConfig {
    fn default() -> Self {
        Self {
            max_tokens: 1024,
            temperature: 0.0,
            system_override: None,
        }
    }
}

/// Per-field char-count caps. Engine-side defense-in-depth — the LLM
/// is told the limits in the prompt; we re-check them at parse time
/// (D-D10).
const MAX_TRIGGER_CHARS: usize = 140;
const MAX_FAILURE_MODE_CHARS: usize = 200;
const MAX_CORRECTION_CHARS: usize = 200;
const MAX_EVIDENCE_REF_CHARS: usize = 80;

/// Engine-side prompt template. Static so we don't allocate per call.
/// The wedge invariants are baked into the rules section — changing
/// this string IS a behavior change (the parse-time validator
/// enforces the same rules; keep them in sync).
const PROMPT_TEMPLATE: &str = "You are generating a CAUSAL NARRATIVE for a lesson captured during an AI coding-agent session. The narrative must explain WHY this lesson exists in terms of a concrete prior failure or friction, not WHAT the lesson says.

Inputs:
- lesson.description: {DESCRIPTION}
- source_feedback (if any): {SOURCE_FEEDBACK}
- recent_session_window: {TRANSCRIPT_EXCERPT}

Rules:
- Do NOT invent causation. If inputs show no concrete failure, set confidence=\"speculative\" and start failure_mode with \"Potential:\".
- Do NOT use praise words (\"great\", \"excellent\", \"successfully\") — sycophancy markers.
- If evidence_refs is empty, confidence MUST be \"speculative\" or \"inferred\" (the schema rejects \"observed\" with empty evidence_refs).
- trigger \u{2264}140 chars; failure_mode \u{2264}200 chars; correction \u{2264}200 chars; each evidence_ref \u{2264}80 chars.
- evidence_refs should be quoted snippets from the inputs above.

If the description is too generic to ground (any session would match), return {\"error\": \"insufficient_context\"} instead.";

#[derive(Debug, Deserialize)]
struct NarrativeDraft {
    trigger: String,
    failure_mode: String,
    correction: String,
    confidence: Confidence,
    #[serde(default)]
    evidence_refs: Vec<String>,
}

/// Discriminate between a successful narrative and a refusal sentinel.
///
/// Phase D audit A-M4 fix: explicit `error`-key check BEFORE attempting
/// to parse as a `NarrativeDraft`. The original implementation used
/// `serde(untagged)` over `{ Success, Refusal }`, which would silently
/// pick `Success` for a mixed-shape response (model returns BOTH the
/// narrative fields AND `error`) because the serde(untagged) walker
/// tries variants in declaration order and accepts the first that
/// deserializes. Refusal discrimination is the wedge-trust hinge —
/// "the model refused" must NEVER be mistaken for "the model gave a
/// valid narrative."
fn discriminate_output(
    parsed: serde_json::Value,
) -> Result<NarrativeDraft, EngineError> {
    // Refusal sentinel: presence of `error` is dispositive. Even if
    // the response ALSO contains valid-looking narrative fields, the
    // model self-marked the output as a refusal — respect that signal.
    if parsed.get("error").is_some() {
        return Err(EngineError::NarrativeInsufficientContext);
    }
    serde_json::from_value::<NarrativeDraft>(parsed).map_err(|e| {
        EngineError::from(LlmError::InvalidOutput(format!("narrative parse: {e}")))
    })
}

/// Generate a `CausalNarrative` for a lesson. Caller supplies the
/// [`LlmClient`] impl (monolith adapter in production; `MockLlmClient`
/// in tests).
///
/// Returns:
/// - `Ok(CausalNarrative)` on happy path. `generated_by =
///   GeneratedBy::Llm` and `generated_at = now.to_rfc3339()` are
///   stamped here.
/// - `Err(EngineError::NarrativeInsufficientContext)` when the LLM
///   returned the refusal sentinel (the inputs are too thin to
///   ground a narrative; not a defect).
/// - `Err(EngineError::Llm(LlmError::InvalidOutput))` when the LLM
///   output couldn't be parsed as `NarrativeLlmOutput`.
/// - `Err(EngineError::Llm(LlmError::ValidationFailed))` when the
///   parsed draft violates an engine-side invariant (length cap,
///   confidence/evidence-refs consistency) — defense-in-depth before
///   the gate (Phase B).
/// - `Err(EngineError::Llm(_))` on any other LLM error.
///
/// Side-effect free: does NOT persist the narrative; caller composes
/// with `lessons::write_lesson_file` or equivalent.
pub async fn generate(
    ctx: &Context,
    llm: &dyn LlmClient,
    narrative_ctx: &NarrativeContext,
    config: &NarrativeConfig,
    now: DateTime<Utc>,
) -> Result<CausalNarrative, EngineError> {
    let prompt = fill_template(narrative_ctx);
    let request = GenerateRequest {
        prompt,
        system: config.system_override.clone(),
        max_tokens: config.max_tokens,
        temperature: config.temperature,
        stop_sequences: Vec::new(),
        response_format: ResponseFormat::JsonSchema {
            schema: build_narrative_schema(),
            name: "CausalNarrativeOrRefusal".to_string(),
        },
        model: None,
    };

    let generation = llm.generate(ctx, &request).await.map_err(EngineError::from)?;

    let parsed = generation.parsed.ok_or_else(|| {
        EngineError::from(LlmError::InvalidOutput(
            "narrative: adapter produced no parsed output for JsonSchema request".into(),
        ))
    })?;

    // Audit A-M4 fix: explicit refusal discrimination by `error` key
    // presence, NOT serde(untagged) variant ordering. See
    // [`discriminate_output`].
    let draft = discriminate_output(parsed)?;

    validate_invariants(&draft)?;

    Ok(CausalNarrative {
        trigger: draft.trigger,
        failure_mode: draft.failure_mode,
        correction: draft.correction,
        confidence: draft.confidence,
        evidence_refs: draft.evidence_refs,
        generated_by: GeneratedBy::Llm,
        generated_at: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
}

fn fill_template(narrative_ctx: &NarrativeContext) -> String {
    PROMPT_TEMPLATE
        .replace("{DESCRIPTION}", &narrative_ctx.description)
        .replace(
            "{SOURCE_FEEDBACK}",
            narrative_ctx.source_feedback.as_deref().unwrap_or("(none)"),
        )
        .replace(
            "{TRANSCRIPT_EXCERPT}",
            narrative_ctx
                .transcript_excerpt
                .as_deref()
                .unwrap_or("(none)"),
        )
}

fn build_narrative_schema() -> Value {
    json!({
        "type": "object",
        "oneOf": [
            {
                "type": "object",
                "required": ["trigger", "failure_mode", "correction", "confidence"],
                "properties": {
                    "trigger": { "type": "string" },
                    "failure_mode": { "type": "string" },
                    "correction": { "type": "string" },
                    "confidence": { "type": "string", "enum": ["observed", "inferred", "speculative"] },
                    "evidence_refs": {
                        "type": "array",
                        "items": { "type": "string" }
                    }
                }
            },
            {
                "type": "object",
                "required": ["error"],
                "properties": {
                    "error": { "type": "string" }
                }
            }
        ]
    })
}

/// Engine-side defense-in-depth (D-D10). Re-enforces the rules the
/// prompt told the LLM to follow. char-count checks use `.chars()`
/// (not `.len()` / bytes) — S141.
fn validate_invariants(draft: &NarrativeDraft) -> Result<(), EngineError> {
    fn over(field: &str, value: &str, cap: usize) -> Option<String> {
        let n = value.chars().count();
        if n > cap {
            Some(format!("{field} length {n} > cap {cap}"))
        } else {
            None
        }
    }
    if let Some(msg) = over("trigger", &draft.trigger, MAX_TRIGGER_CHARS) {
        return Err(EngineError::from(LlmError::ValidationFailed(msg)));
    }
    if let Some(msg) = over("failure_mode", &draft.failure_mode, MAX_FAILURE_MODE_CHARS) {
        return Err(EngineError::from(LlmError::ValidationFailed(msg)));
    }
    if let Some(msg) = over("correction", &draft.correction, MAX_CORRECTION_CHARS) {
        return Err(EngineError::from(LlmError::ValidationFailed(msg)));
    }
    for (i, r) in draft.evidence_refs.iter().enumerate() {
        if let Some(msg) = over(&format!("evidence_refs[{i}]"), r, MAX_EVIDENCE_REF_CHARS) {
            return Err(EngineError::from(LlmError::ValidationFailed(msg)));
        }
    }
    // Wedge invariant: observed REQUIRES non-empty evidence_refs.
    // (Phase B gate enforces this too; we catch at parse-time so we
    // never persist a violating narrative.)
    if matches!(draft.confidence, Confidence::Observed) && draft.evidence_refs.is_empty() {
        return Err(EngineError::from(LlmError::ValidationFailed(
            "confidence=observed requires non-empty evidence_refs (wedge invariant)".into(),
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::llm::{FinishReason, Generation, MockLlmClient};

    fn ctx() -> Context {
        Context::single_user_local()
    }

    fn now() -> DateTime<Utc> {
        "2026-05-13T12:00:00Z".parse().unwrap()
    }

    fn narrative_ctx() -> NarrativeContext {
        NarrativeContext {
            description: "Always run the formatter before committing".into(),
            source_feedback: Some("you forgot to format again".into()),
            transcript_excerpt: Some("[tool=Bash] git commit -m \"x\"".into()),
        }
    }

    fn success_generation(json_str: &str) -> Generation {
        Generation {
            text: json_str.to_string(),
            parsed: Some(serde_json::from_str(json_str).unwrap()),
            finish_reason: FinishReason::Stop,
            usage: None,
        }
    }

    #[tokio::test]
    async fn generate_happy_path_returns_filled_narrative() {
        let json = r#"{
            "trigger": "commit without format",
            "failure_mode": "PR was rejected by CI lint check",
            "correction": "run cargo fmt before git commit",
            "confidence": "inferred",
            "evidence_refs": ["\"you forgot to format again\""]
        }"#;
        let mock = MockLlmClient::default().with_response(success_generation(json));
        let n = generate(
            &ctx(),
            &mock,
            &narrative_ctx(),
            &NarrativeConfig::default(),
            now(),
        )
        .await
        .unwrap();
        assert_eq!(n.trigger, "commit without format");
        assert!(matches!(n.confidence, Confidence::Inferred));
        assert_eq!(n.evidence_refs.len(), 1);
        assert!(matches!(n.generated_by, GeneratedBy::Llm));
        assert!(n.generated_at.contains("2026-05-13"));
    }

    #[tokio::test]
    async fn generate_refusal_surfaces_as_insufficient_context() {
        let json = r#"{"error": "insufficient_context"}"#;
        let mock = MockLlmClient::default().with_response(success_generation(json));
        let r = generate(
            &ctx(),
            &mock,
            &narrative_ctx(),
            &NarrativeConfig::default(),
            now(),
        )
        .await;
        assert!(matches!(r, Err(EngineError::NarrativeInsufficientContext)));
    }

    /// Phase D audit A-M4 regression: a mixed-shape response (both
    /// `error` AND valid narrative fields populated) MUST be treated
    /// as a refusal — the model's self-marked refusal signal wins
    /// over a coincidentally-valid field set. The original
    /// `serde(untagged)` discriminator would have silently selected
    /// `Success` here.
    #[tokio::test]
    async fn generate_mixed_shape_with_error_key_treated_as_refusal() {
        let json = r#"{
            "error": "insufficient_context",
            "trigger": "looks valid but the model said refusal",
            "failure_mode": "f",
            "correction": "c",
            "confidence": "inferred",
            "evidence_refs": []
        }"#;
        let mock = MockLlmClient::default().with_response(success_generation(json));
        let r = generate(
            &ctx(),
            &mock,
            &narrative_ctx(),
            &NarrativeConfig::default(),
            now(),
        )
        .await;
        assert!(
            matches!(r, Err(EngineError::NarrativeInsufficientContext)),
            "wedge-trust: presence of `error` MUST win over narrative fields. \
             Got: {r:?}"
        );
    }

    #[tokio::test]
    async fn generate_rejects_observed_with_empty_evidence_refs() {
        // The wedge invariant. Mirror of gate::s16 at parse time.
        let json = r#"{
            "trigger": "t",
            "failure_mode": "f",
            "correction": "c",
            "confidence": "observed",
            "evidence_refs": []
        }"#;
        let mock = MockLlmClient::default().with_response(success_generation(json));
        let r = generate(
            &ctx(),
            &mock,
            &narrative_ctx(),
            &NarrativeConfig::default(),
            now(),
        )
        .await;
        match r {
            Err(EngineError::Llm(LlmError::ValidationFailed(msg))) => {
                assert!(msg.contains("observed"), "msg={msg}");
                assert!(msg.contains("evidence_refs"), "msg={msg}");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn generate_rejects_trigger_over_140_chars() {
        let over_cap = "x".repeat(141);
        let json = format!(
            r#"{{
                "trigger": "{}",
                "failure_mode": "f",
                "correction": "c",
                "confidence": "inferred",
                "evidence_refs": []
            }}"#,
            over_cap
        );
        let mock = MockLlmClient::default().with_response(success_generation(&json));
        let r = generate(
            &ctx(),
            &mock,
            &narrative_ctx(),
            &NarrativeConfig::default(),
            now(),
        )
        .await;
        match r {
            Err(EngineError::Llm(LlmError::ValidationFailed(msg))) => {
                assert!(msg.contains("trigger"), "msg={msg}");
                assert!(msg.contains("141"), "msg={msg}");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn generate_rejects_evidence_ref_over_80_chars() {
        let over_cap = "x".repeat(81);
        let json = format!(
            r#"{{
                "trigger": "t",
                "failure_mode": "f",
                "correction": "c",
                "confidence": "inferred",
                "evidence_refs": ["{over_cap}"]
            }}"#
        );
        let mock = MockLlmClient::default().with_response(success_generation(&json));
        let r = generate(
            &ctx(),
            &mock,
            &narrative_ctx(),
            &NarrativeConfig::default(),
            now(),
        )
        .await;
        match r {
            Err(EngineError::Llm(LlmError::ValidationFailed(msg))) => {
                assert!(msg.contains("evidence_refs[0]"), "msg={msg}");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn generate_handles_observed_with_evidence_refs() {
        let json = r#"{
            "trigger": "t",
            "failure_mode": "f",
            "correction": "c",
            "confidence": "observed",
            "evidence_refs": ["\"you forgot to format again\""]
        }"#;
        let mock = MockLlmClient::default().with_response(success_generation(json));
        let n = generate(
            &ctx(),
            &mock,
            &narrative_ctx(),
            &NarrativeConfig::default(),
            now(),
        )
        .await
        .unwrap();
        assert!(matches!(n.confidence, Confidence::Observed));
        assert_eq!(n.evidence_refs.len(), 1);
    }

    #[tokio::test]
    async fn generate_surfaces_llm_transport_errors() {
        let inner = std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "boom");
        let mock = MockLlmClient::default().with_error(LlmError::transport(inner));
        let r = generate(
            &ctx(),
            &mock,
            &narrative_ctx(),
            &NarrativeConfig::default(),
            now(),
        )
        .await;
        assert!(matches!(r, Err(EngineError::Llm(LlmError::Transport(_)))));
    }

    #[tokio::test]
    async fn generate_surfaces_invalid_output_on_missing_parsed_field() {
        // Adapter returned text but no `parsed` for a JsonSchema request.
        let g = Generation {
            text: "(adapter forgot to populate parsed)".into(),
            parsed: None,
            finish_reason: FinishReason::Stop,
            usage: None,
        };
        let mock = MockLlmClient::default().with_response(g);
        let r = generate(
            &ctx(),
            &mock,
            &narrative_ctx(),
            &NarrativeConfig::default(),
            now(),
        )
        .await;
        assert!(matches!(r, Err(EngineError::Llm(LlmError::InvalidOutput(_)))));
    }

    #[tokio::test]
    async fn generate_surfaces_invalid_output_on_unparseable_json() {
        // Adapter returned parsed = Some(...) but the structure didn't
        // match either NarrativeLlmOutput variant.
        let g = Generation {
            text: "{}".into(),
            parsed: Some(json!({"unrelated": "field"})),
            finish_reason: FinishReason::Stop,
            usage: None,
        };
        let mock = MockLlmClient::default().with_response(g);
        let r = generate(
            &ctx(),
            &mock,
            &narrative_ctx(),
            &NarrativeConfig::default(),
            now(),
        )
        .await;
        assert!(matches!(r, Err(EngineError::Llm(LlmError::InvalidOutput(_)))));
    }

    #[test]
    fn fill_template_substitutes_all_placeholders() {
        let nc = NarrativeContext {
            description: "DESC_TOKEN".into(),
            source_feedback: Some("FEEDBACK_TOKEN".into()),
            transcript_excerpt: Some("TRANSCRIPT_TOKEN".into()),
        };
        let p = fill_template(&nc);
        assert!(p.contains("DESC_TOKEN"));
        assert!(p.contains("FEEDBACK_TOKEN"));
        assert!(p.contains("TRANSCRIPT_TOKEN"));
        assert!(!p.contains("{DESCRIPTION}"));
        assert!(!p.contains("{SOURCE_FEEDBACK}"));
        assert!(!p.contains("{TRANSCRIPT_EXCERPT}"));
    }

    #[test]
    fn fill_template_substitutes_none_as_placeholder_text() {
        let nc = NarrativeContext {
            description: "x".into(),
            source_feedback: None,
            transcript_excerpt: None,
        };
        let p = fill_template(&nc);
        // The replacement string "(none)" appears for both None fields.
        // Just verify the template doesn't leave any {} placeholders.
        assert!(!p.contains("{SOURCE_FEEDBACK}"));
        assert!(!p.contains("{TRANSCRIPT_EXCERPT}"));
    }

    #[test]
    fn build_narrative_schema_has_oneof_with_two_options() {
        let s = build_narrative_schema();
        let one_of = s.get("oneOf").and_then(|v| v.as_array()).expect("oneOf");
        assert_eq!(one_of.len(), 2);
    }
}
