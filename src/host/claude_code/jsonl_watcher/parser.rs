//! JSONL line → WatcherEvent.
//!
//! Implements the 6-step filter chain from
//! `docs/research/day-13-learn-notes.md`:
//!
//! 1. type == "user"
//! 2. isMeta != true
//! 3. isSidechain != true
//! 4. Content not a tool_result array
//! 5. Text not matching ^\[Request interrupted → emit as UserInterrupt
//! 6. Text not wrapped in <command-name>...</command-name>
//!
//! Pure function: takes a JSON line + session_id, returns either a
//! WatcherEvent or a classification (skip / error). No I/O.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::events::WatcherEvent;

/// Result of parsing a single JSONL line.
///
/// Audit Day 14 M5: `#[non_exhaustive]` so adding outcomes (e.g.
/// `Skip(SkipReason::FutureCase)` consumers see) is non-breaking.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseOutcome {
    /// A relevant WatcherEvent was extracted.
    Event(WatcherEvent),
    /// The line parsed but was filtered out (noise, tool_result, etc.).
    /// Tracked separately from errors because parse-skip is the common case.
    Skip(SkipReason),
    /// The line failed to parse — malformed JSON, missing required fields.
    Error(String),
}

/// Audit Day 14 M5: `#[non_exhaustive]` so adding skip reasons (e.g.
/// `SidechainAuditPolicy`) is non-breaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SkipReason {
    /// type field is not "user" (assistant turn, system, etc.).
    NotUserType,
    /// isMeta == true (Claude Code-injected system event).
    IsMeta,
    /// isSidechain == true (Task-spawned subagent activity).
    IsSidechain,
    /// Content is a tool_result array, not typed user input.
    ToolResultContent,
    /// Text was wrapped in <command-name>...</command-name> (slash command).
    SlashCommandSentinel,
    /// Empty content / no extractable text.
    NoText,
}

/// Parse a single JSONL line for a given session. The session_id is
/// passed in (rather than derived from the event) because the watcher
/// already knows the session from the filename — fewer error paths.
pub fn parse_line(line: &str, session_id: &str) -> ParseOutcome {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return ParseOutcome::Error(format!("json parse: {e}")),
    };

    // Step 1: type filter
    let event_type = value.get("type").and_then(|v| v.as_str());
    if event_type != Some("user") {
        return ParseOutcome::Skip(SkipReason::NotUserType);
    }

    // Step 2: isMeta filter
    if value.get("isMeta").and_then(|v| v.as_bool()) == Some(true) {
        return ParseOutcome::Skip(SkipReason::IsMeta);
    }

    // Step 3: isSidechain filter
    if value.get("isSidechain").and_then(|v| v.as_bool()) == Some(true) {
        return ParseOutcome::Skip(SkipReason::IsSidechain);
    }

    let message = value.get("message");
    let content = message.and_then(|m| m.get("content"));

    // Step 4: tool_result content filter
    if is_tool_result_content(content) {
        return ParseOutcome::Skip(SkipReason::ToolResultContent);
    }

    // Extract the text content (string or array-with-text-block).
    let text = match extract_text(content) {
        Some(t) if !t.trim().is_empty() => t,
        _ => return ParseOutcome::Skip(SkipReason::NoText),
    };

    // Step 5: interrupt sentinel — promote to UserInterrupt.
    if text.starts_with("[Request interrupted") {
        return match build_interrupt_event(&value, session_id) {
            Ok(ev) => ParseOutcome::Event(ev),
            Err(e) => ParseOutcome::Error(format!("interrupt event: {e}")),
        };
    }

    // Step 6: slash-command sentinel — text wrapped in <command-name> tags.
    if is_slash_command_sentinel(&text) {
        return ParseOutcome::Skip(SkipReason::SlashCommandSentinel);
    }

    match build_user_turn(&value, session_id, text) {
        Ok(ev) => ParseOutcome::Event(ev),
        Err(e) => ParseOutcome::Error(format!("user turn event: {e}")),
    }
}

fn is_tool_result_content(content: Option<&Value>) -> bool {
    let Some(arr) = content.and_then(|c| c.as_array()) else {
        return false;
    };
    arr.iter()
        .any(|item| item.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
}

fn extract_text(content: Option<&Value>) -> Option<String> {
    let c = content?;
    if let Some(s) = c.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = c.as_array() {
        let mut buf = String::new();
        let mut found = false;
        for item in arr {
            if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(s) = item.get("text").and_then(|t| t.as_str()) {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(s);
                    found = true;
                }
            }
        }
        if found {
            return Some(buf);
        }
    }
    None
}

fn is_slash_command_sentinel(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("<command-name>") && trimmed.contains("</command-name>")
}

fn build_user_turn(value: &Value, session_id: &str, text: String) -> Result<WatcherEvent> {
    let event_uuid = require_string(value, "uuid").context("uuid")?;
    let parent_uuid = optional_string(value, "parentUuid");
    let cwd = require_string(value, "cwd").context("cwd")?;
    let git_branch = optional_string(value, "gitBranch");
    let timestamp = parse_timestamp(require_string(value, "timestamp").context("timestamp")?)?;
    let cc_version = optional_string(value, "version").unwrap_or_else(|| "unknown".to_string());

    Ok(WatcherEvent::UserTurn {
        session_id: session_id.to_string(),
        event_uuid,
        parent_uuid,
        cwd: PathBuf::from(cwd),
        git_branch,
        timestamp,
        text,
        cc_version,
    })
}

fn build_interrupt_event(value: &Value, session_id: &str) -> Result<WatcherEvent> {
    let event_uuid = require_string(value, "uuid").context("uuid")?;
    let parent_uuid = optional_string(value, "parentUuid");
    let timestamp = parse_timestamp(require_string(value, "timestamp").context("timestamp")?)?;

    Ok(WatcherEvent::UserInterrupt {
        session_id: session_id.to_string(),
        event_uuid,
        parent_uuid,
        timestamp,
        // Lookback to the preceding assistant text is Day 15's job
        // (the correction-window mining lives there).
        interrupted_assistant_text: None,
    })
}

fn require_string(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("missing or non-string {key}"))
}

fn optional_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn parse_timestamp(raw: String) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&raw)
        .map(|dt| dt.with_timezone(&Utc))
        .with_context(|| format!("parsing timestamp: {raw}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_turn_json(extra: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"u1","parentUuid":"p1","cwd":"/cwd","gitBranch":"main","timestamp":"2026-05-13T10:00:00.000Z","version":"2.1.139","sessionId":"sess-1","message":{{"role":"user","content":"hello there"}}{extra}}}"#
        )
    }

    #[test]
    fn parses_plain_user_turn() {
        let line = user_turn_json("");
        let outcome = parse_line(&line, "sess-1");
        let ev = match outcome {
            ParseOutcome::Event(e) => e,
            other => panic!("expected event, got {other:?}"),
        };
        match ev {
            WatcherEvent::UserTurn {
                text,
                cc_version,
                event_uuid,
                ..
            } => {
                assert_eq!(text, "hello there");
                assert_eq!(cc_version, "2.1.139");
                assert_eq!(event_uuid, "u1");
            }
            other => panic!("expected UserTurn, got {other:?}"),
        }
    }

    #[test]
    fn skips_assistant_type() {
        let line = r#"{"type":"assistant","uuid":"a1"}"#;
        match parse_line(line, "sess-1") {
            ParseOutcome::Skip(SkipReason::NotUserType) => {}
            other => panic!("expected NotUserType, got {other:?}"),
        }
    }

    #[test]
    fn skips_is_meta_true() {
        let line = user_turn_json(r#","isMeta":true"#);
        match parse_line(&line, "sess-1") {
            ParseOutcome::Skip(SkipReason::IsMeta) => {}
            other => panic!("expected IsMeta, got {other:?}"),
        }
    }

    #[test]
    fn skips_is_sidechain_true() {
        let line = user_turn_json(r#","isSidechain":true"#);
        match parse_line(&line, "sess-1") {
            ParseOutcome::Skip(SkipReason::IsSidechain) => {}
            other => panic!("expected IsSidechain, got {other:?}"),
        }
    }

    #[test]
    fn skips_tool_result_array_content() {
        let line = r#"{"type":"user","uuid":"u1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"output"}]}}"#;
        match parse_line(line, "sess-1") {
            ParseOutcome::Skip(SkipReason::ToolResultContent) => {}
            other => panic!("expected ToolResultContent, got {other:?}"),
        }
    }

    #[test]
    fn skips_slash_command_sentinel() {
        let line = r#"{"type":"user","uuid":"u1","cwd":"/c","timestamp":"2026-05-13T10:00:00.000Z","message":{"role":"user","content":"<command-name>doc</command-name>"}}"#;
        match parse_line(line, "sess-1") {
            ParseOutcome::Skip(SkipReason::SlashCommandSentinel) => {}
            other => panic!("expected SlashCommandSentinel, got {other:?}"),
        }
    }

    #[test]
    fn promotes_request_interrupted_to_interrupt_event() {
        let line = r#"{"type":"user","uuid":"u1","parentUuid":"p1","cwd":"/cwd","timestamp":"2026-05-13T10:00:00.000Z","message":{"role":"user","content":"[Request interrupted by user]"}}"#;
        match parse_line(line, "sess-1") {
            ParseOutcome::Event(WatcherEvent::UserInterrupt {
                event_uuid,
                parent_uuid,
                ..
            }) => {
                assert_eq!(event_uuid, "u1");
                assert_eq!(parent_uuid.as_deref(), Some("p1"));
            }
            other => panic!("expected UserInterrupt, got {other:?}"),
        }
    }

    #[test]
    fn extracts_text_from_array_content_with_text_block() {
        let line = r#"{"type":"user","uuid":"u1","cwd":"/cwd","timestamp":"2026-05-13T10:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"hello"},{"type":"text","text":"world"}]}}"#;
        match parse_line(line, "sess-1") {
            ParseOutcome::Event(WatcherEvent::UserTurn { text, .. }) => {
                assert_eq!(text, "hello\nworld");
            }
            other => panic!("expected UserTurn, got {other:?}"),
        }
    }

    #[test]
    fn skips_empty_content_string() {
        let line = r#"{"type":"user","uuid":"u1","cwd":"/cwd","timestamp":"2026-05-13T10:00:00.000Z","message":{"role":"user","content":"   "}}"#;
        match parse_line(line, "sess-1") {
            ParseOutcome::Skip(SkipReason::NoText) => {}
            other => panic!("expected NoText, got {other:?}"),
        }
    }

    #[test]
    fn errors_on_malformed_json() {
        let line = "{not valid json";
        match parse_line(line, "sess-1") {
            ParseOutcome::Error(msg) => assert!(msg.contains("json parse")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn errors_when_required_field_missing() {
        let line = r#"{"type":"user","message":{"role":"user","content":"hi"}}"#;
        // Missing uuid + cwd + timestamp — should error on the first.
        match parse_line(line, "sess-1") {
            ParseOutcome::Error(_) => {}
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn cc_version_defaults_when_missing() {
        let line = r#"{"type":"user","uuid":"u1","cwd":"/c","timestamp":"2026-05-13T10:00:00.000Z","message":{"role":"user","content":"hi"}}"#;
        match parse_line(line, "sess-1") {
            ParseOutcome::Event(WatcherEvent::UserTurn { cc_version, .. }) => {
                assert_eq!(cc_version, "unknown");
            }
            other => panic!("expected UserTurn, got {other:?}"),
        }
    }
}
