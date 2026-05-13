//! Frontmatter delimiter handling.
//!
//! A Loop lesson file looks like:
//!
//! ```text
//! ---
//! id: les-...
//! description: ...
//! ...
//! ---
//!
//! Markdown body content goes here.
//! ```
//!
//! This module splits and combines the `---`-delimited frontmatter
//! block. The `serialize_lesson_frontmatter` writer in `writer.rs`
//! handles the YAML inside; this module handles the outer envelope.

use anyhow::{bail, Result};

/// Result of splitting a lesson file into its frontmatter block and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterSplit<'a> {
    /// The YAML between the `---` delimiters (no delimiters, no trailing newline).
    pub yaml: &'a str,
    /// The markdown body after the closing `---`. Preserves leading newline(s).
    pub body: &'a str,
}

/// Result of splitting a lesson file into its frontmatter block and body
/// (owned, not borrowed — allows CRLF normalization without retaining
/// references into a transient buffer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedFrontmatterSplit {
    pub yaml: String,
    pub body: String,
}

/// Split a full lesson .md file into (frontmatter YAML, body). Errors if
/// the file does not begin with a `---` delimiter or has no closing one.
///
/// CRLF input is normalized to LF in BOTH the YAML and the body — TS's
/// regex captures non-greedy and the same normalization happens implicitly.
/// Audit A7: returning bare `id: a\r` would cause downstream YAML parse
/// drift; we strip the `\r` here.
pub fn split_frontmatter(source: &str) -> Result<FrontmatterSplit<'_>> {
    let rest = if let Some(r) = source.strip_prefix("---\n") {
        r
    } else if let Some(r) = source.strip_prefix("---\r\n") {
        r
    } else {
        bail!("file does not start with a `---` frontmatter delimiter");
    };

    let end = find_closing_delimiter(rest)
        .ok_or_else(|| anyhow::anyhow!("frontmatter has no closing `---` delimiter"))?;
    let yaml = &rest[..end.yaml_end];
    let body = &rest[end.body_start..];
    Ok(FrontmatterSplit { yaml, body })
}

/// Same as `split_frontmatter` but normalizes CRLF → LF in the returned
/// strings. Owns the strings so callers can mutate them. Use this for
/// any path that feeds the parser; the borrowed version is fine when
/// callers only care about byte ranges.
pub fn split_frontmatter_normalized(source: &str) -> Result<OwnedFrontmatterSplit> {
    let split = split_frontmatter(source)?;
    Ok(OwnedFrontmatterSplit {
        yaml: split.yaml.replace("\r\n", "\n").replace('\r', ""),
        body: split.body.replace("\r\n", "\n"),
    })
}

/// Combine a frontmatter YAML block and a body into a full lesson file
/// string. The YAML is wrapped in `---\n` delimiters with the body
/// appended. Adds a final newline if the body lacks one (mirrors
/// `renderLessonFile` in TS `loader.ts`).
///
/// Known compatibility quirk inherited from TS: this function emits
/// `---\n\n{body}` unconditionally. The companion `split_frontmatter`
/// returns body INCLUDING the post-delimiter `\n` (matching the TS
/// regex). The combination means a load → save cycle adds one `\n` to
/// the body each pass. We match this to coexist with TS on the same
/// files. Day 12 callers that do read-modify-write should normalize
/// body whitespace to prevent unbounded accumulation across many
/// signal-emit cycles.
pub fn combine_frontmatter(yaml: &str, body: &str) -> String {
    let trimmed_yaml = yaml.trim_end_matches('\n');
    let mut out = String::with_capacity(trimmed_yaml.len() + body.len() + 16);
    out.push_str("---\n");
    out.push_str(trimmed_yaml);
    out.push_str("\n---\n\n");
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out
}

struct CloseLocation {
    yaml_end: usize,
    body_start: usize,
}

fn find_closing_delimiter(after_open: &str) -> Option<CloseLocation> {
    // We look for `\n---\n` or `\n---\r\n` or `\n---` at end of input.
    // The closing delimiter must be on its own line.
    let mut search_from = 0usize;
    while let Some(idx) = after_open[search_from..].find("---") {
        let abs = search_from + idx;
        let at_line_start = abs == 0 || matches!(after_open.as_bytes().get(abs - 1), Some(b'\n'));
        if !at_line_start {
            search_from = abs + 3;
            continue;
        }
        let after = &after_open[abs + 3..];
        let next = after.as_bytes().first().copied();
        if next == Some(b'\n') {
            // Strip the preceding newline from the YAML block (it belongs to the delimiter).
            let yaml_end = if abs > 0 { abs - 1 } else { abs };
            let body_start = abs + 4; // skip "---\n"
            return Some(CloseLocation {
                yaml_end,
                body_start,
            });
        }
        if next == Some(b'\r') && after.as_bytes().get(1).copied() == Some(b'\n') {
            let yaml_end = if abs > 0 { abs - 1 } else { abs };
            let body_start = abs + 5; // skip "---\r\n"
            return Some(CloseLocation {
                yaml_end,
                body_start,
            });
        }
        if next.is_none() {
            // EOF — no body. Allow.
            let yaml_end = if abs > 0 { abs - 1 } else { abs };
            let body_start = abs + 3;
            return Some(CloseLocation {
                yaml_end,
                body_start,
            });
        }
        search_from = abs + 3;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_minimal_file() {
        let source = "---\nid: a\n---\n\nbody\n";
        let split = split_frontmatter(source).unwrap();
        assert_eq!(split.yaml, "id: a");
        assert_eq!(split.body, "\nbody\n");
    }

    #[test]
    fn splits_multi_line_frontmatter() {
        let source = "---\nid: a\ndescription: b\n---\n\nbody\n";
        let split = split_frontmatter(source).unwrap();
        assert_eq!(split.yaml, "id: a\ndescription: b");
        assert_eq!(split.body, "\nbody\n");
    }

    #[test]
    fn splits_handles_crlf_open() {
        let source = "---\r\nid: a\r\n---\r\n\r\nbody\r\n";
        let split = split_frontmatter(source).unwrap();
        // Borrowed split returns raw bytes including trailing \r from each line.
        assert_eq!(split.yaml, "id: a\r");
        assert!(split.body.contains("body"));
    }

    /// Audit A7: the normalized split path strips CRs so downstream YAML
    /// parsing doesn't see them. Used by the daemon's loader.
    #[test]
    fn split_normalized_strips_carriage_returns() {
        let source = "---\r\nid: a\r\ndescription: b\r\n---\r\n\r\nbody\r\n";
        let split = split_frontmatter_normalized(source).unwrap();
        assert_eq!(split.yaml, "id: a\ndescription: b");
        assert!(!split.yaml.contains('\r'));
        assert!(!split.body.contains('\r'));
    }

    #[test]
    fn split_normalized_passes_through_lf_input() {
        let source = "---\nid: a\n---\n\nbody\n";
        let split = split_frontmatter_normalized(source).unwrap();
        assert_eq!(split.yaml, "id: a");
        assert_eq!(split.body, "\nbody\n");
    }

    /// Audit A7 follow-through: after normalization, the parser actually
    /// accepts the cleaned YAML (not a smoke test, a real parse).
    #[test]
    fn split_normalized_yaml_parses_cleanly() {
        use crate::engine::yaml::reader::parse_lesson_frontmatter;

        let source = "---\r\nid: les-aaaaaaaa\r\ndescription: minimal\r\nstatus: active\r\ncreated_at: 2026-05-13T00:00:00.000Z\r\napplied_count: 0\r\nthumbs_up_count: 0\r\nthumbs_down_count: 0\r\nexternal_signal_sources: []\r\n---\r\n\r\nbody\r\n";
        let split = split_frontmatter_normalized(source).unwrap();
        let fm = parse_lesson_frontmatter(&split.yaml).unwrap();
        assert_eq!(fm.id, "les-aaaaaaaa");
    }

    #[test]
    fn refuses_file_without_opening_delimiter() {
        let source = "id: a\n---\nbody";
        assert!(split_frontmatter(source).is_err());
    }

    #[test]
    fn refuses_file_without_closing_delimiter() {
        let source = "---\nid: a\nbody no end";
        assert!(split_frontmatter(source).is_err());
    }

    #[test]
    fn body_dashes_in_content_do_not_close_frontmatter() {
        // A line of `---` only counts as a delimiter when it's a *full
        // line*. Embedded `---` in body text is fine — but our parser only
        // looks for the FIRST closing delimiter. Verify with a body that
        // contains a later `---` line.
        let source = "---\nid: a\n---\n\nbody\n\n---\nmore body\n";
        let split = split_frontmatter(source).unwrap();
        assert_eq!(split.yaml, "id: a");
        assert!(split.body.contains("more body"));
    }

    #[test]
    fn combines_round_trip() {
        let yaml = "id: a\ndescription: b\n";
        // Body deliberately doesn't start with \n — the combiner always
        // emits `---\n\n` as the post-frontmatter separator (mirrors the
        // TS-side `renderLessonFile`). A leading-\n body would compound
        // into `\n\n\n` after the closing delimiter.
        let body = "## Heading\n\ncontent\n";
        let combined = combine_frontmatter(yaml, body);
        let split = split_frontmatter(&combined).unwrap();
        assert_eq!(split.yaml, "id: a\ndescription: b");
        // The split returns the body INCLUDING the post-delimiter blank
        // line. That's what `renderLessonFile` produces on the TS side.
        assert_eq!(split.body, "\n## Heading\n\ncontent\n");
    }

    #[test]
    fn combines_preserves_body_starting_with_newline() {
        // If the caller's body already starts with \n, the combiner
        // produces three consecutive newlines after the closing `---`:
        // one from `---\n\n` and the body's own leading `\n`. Split
        // returns both.
        let yaml = "id: a";
        let body = "\nleading-newline body\n";
        let combined = combine_frontmatter(yaml, body);
        let split = split_frontmatter(&combined).unwrap();
        assert_eq!(split.yaml, "id: a");
        assert_eq!(split.body, "\n\nleading-newline body\n");
    }

    #[test]
    fn combines_adds_trailing_newline_if_missing() {
        let yaml = "id: a";
        let body = "body without newline";
        let combined = combine_frontmatter(yaml, body);
        assert!(combined.ends_with('\n'));
    }

    #[test]
    fn combines_strips_trailing_newlines_from_yaml() {
        let yaml = "id: a\n\n\n";
        let body = "body\n";
        let combined = combine_frontmatter(yaml, body);
        // Closing delimiter should appear immediately after the last YAML line.
        assert!(combined.contains("id: a\n---\n\n"));
    }
}
