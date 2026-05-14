//! Phase G D-G1 (v0.4): `MemoryOrigin` — provenance metadata.
//!
//! Captures where a memory came from so the wedge gate has a richer
//! external-signal vocabulary (`origin_diverse` = applied across N
//! distinct sessions = harder to fake than applied N times in one
//! session). Also enables session-aware recall biasing in future
//! cycles ("memories from this session" vs "memories from yesterday").
//!
//! All fields are optional so hosts populate what they can detect.
//! v0.3.1 memories without an `origin` block deserialize as `None` via
//! the parent struct's `#[serde(default)]`.
//!
//! Privacy invariant: NEVER store full file paths, user identity, or
//! raw transcript content here. `session_id` is an opaque (hashed or
//! truncated) discriminator, not a reversible key.

use serde::{Deserialize, Serialize};

/// Provenance attached to a memory at creation time. Mirrors the YAML
/// shape stored in the frontmatter under the `origin:` key.
///
/// All fields are `Option<String>` so partial detection (e.g. a host
/// that knows `host` and `model` but not `session_id`) round-trips
/// cleanly — empty fields are omitted from on-disk YAML via
/// `skip_serializing_if = "Option::is_none"`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryOrigin {
    /// The MCP host that initiated the memorize call. Free-form string
    /// — e.g. `"claude-code"`, `"claude-desktop"`, `"cline"`,
    /// `"continue"`, `"<unknown>"`. Used by future cycles for per-host
    /// recall biasing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,

    /// Opaque discriminator for the conversation/session that wrote
    /// this memory. First 8 chars of a session hash, or
    /// `sha1(start_time + pid)[:8]` when the host doesn't expose a
    /// session UUID. NOT a reversible key — privacy invariant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,

    /// Model identifier as known to the host at write time
    /// (e.g. `"claude-opus-4-7"`). Informational — carries no auth
    /// weight, but useful for downweighting memories from smaller
    /// models in recall ranking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Last path segment of the working directory at write time
    /// (e.g. `"opensquid"`). Redundant with `scope: {project: id}`
    /// when scope is set, but helpful when scope falls back to `User`
    /// and we still want a hint about where the memory came from.
    /// NEVER the full path — privacy invariant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd_basename: Option<String>,

    /// RFC3339 timestamp at which the host invoked `memorize`. Distinct
    /// from `created_at` on the parent frontmatter — `written_at`
    /// reflects the host-side clock + can differ from `created_at`
    /// after future supersession rewrites.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written_at: Option<String>,
}

impl MemoryOrigin {
    /// True when every documented field is unset. Lets callers
    /// short-circuit "skip serializing empty origin" without writing
    /// the predicate inline.
    pub fn is_empty(&self) -> bool {
        self.host.is_none()
            && self.session_id.is_none()
            && self.model.is_none()
            && self.cwd_basename.is_none()
            && self.written_at.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_all_none() {
        let o = MemoryOrigin::default();
        assert!(o.is_empty());
        assert!(o.host.is_none());
    }

    #[test]
    fn serde_round_trip_full() {
        let o = MemoryOrigin {
            host: Some("claude-code".into()),
            session_id: Some("a1b2c3d4".into()),
            model: Some("claude-opus-4-7".into()),
            cwd_basename: Some("opensquid".into()),
            written_at: Some("2026-05-14T19:32:00.000Z".into()),
        };
        let yaml = serde_yml::to_string(&o).unwrap();
        let back: MemoryOrigin = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(o, back);
    }

    #[test]
    fn serde_round_trip_partial_omits_none_fields() {
        let o = MemoryOrigin {
            host: Some("cline".into()),
            session_id: None,
            model: None,
            cwd_basename: None,
            written_at: None,
        };
        let yaml = serde_yml::to_string(&o).unwrap();
        // None fields must be absent from the on-disk YAML so files
        // stay small and back-compat is clean.
        assert!(!yaml.contains("session_id"));
        assert!(!yaml.contains("model"));
        assert!(!yaml.contains("cwd_basename"));
        assert!(!yaml.contains("written_at"));
        let back: MemoryOrigin = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(o, back);
    }

    #[test]
    fn legacy_v031_yaml_with_no_origin_field_parses_clean() {
        // What a memory written by v0.3.1 (no origin block) looks like
        // after YAML extraction. The parent frontmatter has
        // `origin: Option<MemoryOrigin>` with `#[serde(default)]`, so
        // missing key deserializes to None — verified at the parent
        // level. Here we just verify an empty MemoryOrigin
        // (no fields at all) round-trips through an empty YAML map.
        let empty: MemoryOrigin = serde_yml::from_str("{}").unwrap();
        assert!(empty.is_empty());
    }
}
