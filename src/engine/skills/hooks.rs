//! Phase F D-F2: Claude-Skills hooks model in Rust.
//!
//! Claude's hooks structure (investigated 2026-05-14 — 27+ events):
//! `hooks: { <event_name>: [ { matcher: "...", hooks: [<handler>, ...] }, ... ] }`
//!
//! Translation to typed Rust:
//! - `HookEvent(String)` newtype — open-ended for new events Anthropic
//!   adds (was 6 events in late 2025, 27 by 2026; growth continues).
//! - `HookMatcherGroup { matcher, hooks }` — the per-event group shape.
//! - `HookHandler` tagged enum — 5 KNOWN handler types
//!   (`Command`, `Http`, `McpTool`, `Prompt`, `Agent`) with
//!   `#[non_exhaustive]` so new handler types land additively.
//!
//! Engine STORES; host EXECUTES. The `Command::script` is a
//! `String` (not `PathBuf`) so it can be a path OR inline code —
//! host adapter resolves at execution time.

use serde::{Deserialize, Serialize};

/// Hook lifecycle event name. Open-ended `String` newtype so the
/// schema forward-compats with Anthropic's growing event list.
/// Engine doesn't validate event names — host adapter recognizes
/// what it cares about.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HookEvent(pub String);

impl HookEvent {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HookEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Per-event group: optional matcher (e.g. tool name pattern) +
/// list of handlers to invoke when the matcher fires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookMatcherGroup {
    /// Optional matcher pattern. `None` matches everything.
    /// Semantics are host-defined (Claude uses tool-name regex on
    /// `PreToolUse`, etc).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    /// Handlers to invoke. Each handler is one of the 5 known
    /// types — see [`HookHandler`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<HookHandler>,
}

impl HookMatcherGroup {
    pub fn new() -> Self {
        Self {
            matcher: None,
            hooks: Vec::new(),
        }
    }
}

impl Default for HookMatcherGroup {
    fn default() -> Self {
        Self::new()
    }
}

/// One of the 5 known Claude-Skills hook handler types. Tagged
/// externally on the `type` field. `#[non_exhaustive]` so new
/// Anthropic-side handler types land additively.
///
/// Common fields across all variants:
/// - `timeout` (seconds): how long the host runtime gives the hook
///   to complete before killing it. None = host default.
/// - `once`: skills-specific — fire at most once per session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum HookHandler {
    /// Run a shell command. `script` is either a path (host
    /// resolves) or inline code (host runtime decides).
    Command {
        script: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<u32>,
        #[serde(default, skip_serializing_if = "is_false")]
        once: bool,
    },
    /// POST to an HTTP endpoint with the hook context as the body.
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<u32>,
        #[serde(default, skip_serializing_if = "is_false")]
        once: bool,
    },
    /// Invoke an MCP server's tool.
    McpTool {
        server: String,
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<u32>,
        #[serde(default, skip_serializing_if = "is_false")]
        once: bool,
    },
    /// Run a prompt template against the active LLM.
    Prompt {
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<u32>,
        #[serde(default, skip_serializing_if = "is_false")]
        once: bool,
    },
    /// Hand off to a subagent.
    Agent {
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<u32>,
        #[serde(default, skip_serializing_if = "is_false")]
        once: bool,
    },
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_event_round_trips_as_string() {
        let e = HookEvent::new("PreToolUse");
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(s, "\"PreToolUse\"");
        let back: HookEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn hook_matcher_group_default_is_empty() {
        let g = HookMatcherGroup::default();
        assert!(g.matcher.is_none());
        assert!(g.hooks.is_empty());
    }

    #[test]
    fn hook_handler_command_round_trip() {
        let h = HookHandler::Command {
            script: "/usr/local/bin/fmt".into(),
            timeout: Some(30),
            once: true,
        };
        let s = serde_json::to_string(&h).unwrap();
        let back: HookHandler = serde_json::from_str(&s).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn hook_handler_mcp_tool_round_trip() {
        let h = HookHandler::McpTool {
            server: "loop".into(),
            tool: "loop_capture_lesson".into(),
            timeout: None,
            once: false,
        };
        let s = serde_json::to_string(&h).unwrap();
        let back: HookHandler = serde_json::from_str(&s).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn hook_handler_uses_tagged_serialization() {
        // Verify `type` discriminator is emitted.
        let h = HookHandler::Http {
            url: "https://example.com".into(),
            timeout: None,
            once: false,
        };
        let s = serde_json::to_string(&h).unwrap();
        assert!(s.contains("\"type\":\"http\""), "missing type tag: {s}");
    }

    #[test]
    fn unknown_handler_type_rejects() {
        // `#[non_exhaustive]` enums don't accept unknown tags on
        // deserialize — confirm we reject rather than silently coerce.
        let json = r#"{"type": "future_handler", "field": "value"}"#;
        let r: Result<HookHandler, _> = serde_json::from_str(json);
        assert!(r.is_err());
    }
}
