//! YAML scalar rendering — plain vs double-quoted vs literal block.
//!
//! Matches the TS-side `yaml@2.x` library's output under the pinned options
//! `{blockQuote: 'literal', lineWidth: 0, defaultStringType: 'PLAIN'}`.
//!
//! The audit Day 11 caught five distinct over/under-quoting cases here.
//! Fixes:
//!   - A2: multi-line strings emit literal block `|-` / `|`, not escaped \n in "..."
//!   - A3: `.inf`/`.nan`/hex/octal/leading-+ patterns are quoted as data corruption guards
//!   - A4: yes/no/on/off lowercase are NOT quoted (YAML 1.2 dropped these as keywords);
//!     embedded `"` / `'` / tabs are spec-legal in plain and are NOT quoted
//!   - A5: control chars use named escapes (\0/\a/\b/\v/\f/\e) not \xNN
//!   - A6: `?key` (not followed by space) is plain

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScalarStyle {
    Plain,
    DoubleQuoted,
    /// Literal block scalar. `chomp_strip` true → `|-` (no trailing newline);
    /// false → `|` (preserve trailing newline).
    LiteralBlock {
        chomp_strip: bool,
    },
}

pub fn scalar_style(value: &str) -> ScalarStyle {
    if value.is_empty() {
        return ScalarStyle::DoubleQuoted;
    }

    // Multi-line → block style. Mirrors TS's `blockQuote: 'literal'` behavior.
    // Trailing-newline rule from empirical TS output:
    //   "p1\np2"   → |-\n  p1\n  p2
    //   "p1\np2\n" → |\n  p1\n  p2
    if value.contains('\n') {
        // Block style cannot represent strings with control chars other than
        // LF/HT inside lines. Fall back to double-quoted in that case.
        let has_invalid_for_block = value
            .chars()
            .any(|c| c != '\n' && c != '\t' && c.is_control());
        if has_invalid_for_block {
            return ScalarStyle::DoubleQuoted;
        }
        // Lines themselves must not contain leading whitespace ambiguity at
        // block opening. Plain block scalars can handle this via indentation
        // indicators; we keep it simple by falling back to double-quoted if
        // the first line starts with whitespace.
        let first_line = value.split('\n').next().unwrap_or("");
        if first_line.starts_with(' ') || first_line.starts_with('\t') {
            return ScalarStyle::DoubleQuoted;
        }
        let chomp_strip = !value.ends_with('\n');
        return ScalarStyle::LiteralBlock { chomp_strip };
    }

    if needs_double_quoting(value) {
        ScalarStyle::DoubleQuoted
    } else {
        ScalarStyle::Plain
    }
}

fn needs_double_quoting(value: &str) -> bool {
    // YAML 1.2 reserved scalar tags — these would parse back as non-string.
    // The lowercase `true`/`false`/`null` are the only ones in spec 1.2.
    // The Title-case variants are accepted by some readers and we quote
    // defensively (TS does too).
    if matches!(
        value,
        "true" | "false" | "null" | "True" | "False" | "Null" | "TRUE" | "FALSE" | "NULL" | "~"
    ) {
        return true;
    }

    // Numeric-looking strings: Rust's i64/f64 parsers don't catch every YAML
    // numeric form. Cover hex (`0x10`), octal (`0o7`), leading-`+` ints
    // (`+42`), infinity / NaN literals, leading-`.` decimals.
    if value.parse::<i64>().is_ok() || value.parse::<f64>().is_ok() {
        return true;
    }
    if is_yaml_extra_numeric(value) {
        return true;
    }

    let first = value.chars().next().unwrap();

    // Plain-scalar disallowed first characters per YAML 1.2 spec. `?` is
    // only a problem when followed by space (complex mapping key indicator)
    // — we handle that inline below. `"` and `'` as FIRST char would be
    // mistaken for quoted scalars on parse, so disallow them as first char
    // even though A4 says embedded quotes are plain-legal.
    if matches!(
        first,
        '-' | ':'
            | ','
            | '['
            | ']'
            | '{'
            | '}'
            | '#'
            | '&'
            | '*'
            | '!'
            | '|'
            | '>'
            | '\''
            | '"'
            | '%'
            | '@'
            | '`'
    ) {
        return true;
    }
    // Leading `?` only if followed by space (complex mapping key indicator).
    if first == '?' && value.chars().nth(1) == Some(' ') {
        return true;
    }
    if first.is_whitespace() {
        return true;
    }

    // Plain-scalar disallowed mid-string sequences.
    if value.contains(": ") || value.contains(" #") {
        return true;
    }

    // Control chars (other than \n which we handled above as block style and
    // \t which is spec-legal in plain). Tab IS legal in plain scalars per
    // YAML 1.2 spec.
    if value.chars().any(|c| c.is_control() && c != '\t') {
        return true;
    }

    // Trailing whitespace would be ambiguous in plain style.
    if value.ends_with(' ') || value.ends_with('\t') {
        return true;
    }

    false
}

fn is_yaml_extra_numeric(value: &str) -> bool {
    // Hex / octal / binary numerics.
    let lowered = value.to_ascii_lowercase();
    if lowered.starts_with("0x") || lowered.starts_with("0o") || lowered.starts_with("0b") {
        let tail = &value[2..];
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_hexdigit() || c == '_') {
            return true;
        }
    }

    // YAML infinity / NaN.
    matches!(
        lowered.as_str(),
        ".inf" | ".nan" | "+.inf" | "-.inf" | "+.nan" | "-.nan"
    )
}

pub fn render_scalar(value: &str, indent: usize) -> String {
    match scalar_style(value) {
        ScalarStyle::Plain => value.to_string(),
        ScalarStyle::DoubleQuoted => double_quote(value),
        ScalarStyle::LiteralBlock { chomp_strip } => literal_block(value, indent, chomp_strip),
    }
}

/// Double-quoted YAML string with TS-compatible escapes.
/// Audit A5: named escapes match TS yaml's table, not `\xNN`.
pub fn double_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            '\0' => out.push_str(r"\0"),
            '\x07' => out.push_str(r"\a"),
            '\x08' => out.push_str(r"\b"),
            '\t' => out.push_str(r"\t"),
            '\n' => out.push_str(r"\n"),
            '\x0b' => out.push_str(r"\v"),
            '\x0c' => out.push_str(r"\f"),
            '\r' => out.push_str(r"\r"),
            '\x1b' => out.push_str(r"\e"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                use std::fmt::Write;
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Literal block scalar. Each line indented by `indent` spaces.
/// `chomp_strip` selects `|-` (no trailing newline preserved) vs `|`.
fn literal_block(value: &str, indent: usize, chomp_strip: bool) -> String {
    let header = if chomp_strip { "|-" } else { "|" };
    let mut out = String::with_capacity(value.len() + indent + header.len() + 8);
    out.push_str(header);
    out.push('\n');
    // Strip the trailing \n that the chomp-strip case represents — the block
    // header `|-` says "no trailing newline" so we don't emit one for the
    // final line. The non-strip case keeps the input's trailing structure.
    let body = if chomp_strip {
        value
    } else {
        // For `|`, the input already has a trailing `\n`; we'll emit one
        // newline at the end via the loop without adding another.
        value.trim_end_matches('\n')
    };
    let pad = " ".repeat(indent);
    let lines: Vec<&str> = body.split('\n').collect();
    for (i, line) in lines.iter().enumerate() {
        out.push_str(&pad);
        out.push_str(line);
        if i + 1 < lines.len() {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_for_simple_value() {
        assert_eq!(scalar_style("hello"), ScalarStyle::Plain);
    }

    #[test]
    fn plain_for_yaml_1_2_boolean_keywords_no_longer_reserved() {
        // YAML 1.2 dropped yes/no/on/off — they're plain strings now.
        for v in ["yes", "no", "on", "off", "Yes", "No", "On", "Off"] {
            assert_eq!(scalar_style(v), ScalarStyle::Plain, "{v} should be plain");
        }
    }

    #[test]
    fn double_quoted_for_yaml_1_2_keywords() {
        for v in ["true", "false", "null", "True", "False", "Null", "~"] {
            assert_eq!(
                scalar_style(v),
                ScalarStyle::DoubleQuoted,
                "{v} should be quoted"
            );
        }
    }

    #[test]
    fn double_quoted_for_numeric_looking() {
        for v in [
            "42", "-1", "3.14", "1e10", "0x10", "0o7", "+42", ".inf", ".nan", "-.INF",
        ] {
            assert_eq!(
                scalar_style(v),
                ScalarStyle::DoubleQuoted,
                "{v} should be quoted"
            );
        }
    }

    #[test]
    fn plain_with_embedded_quotes() {
        // Audit A4: TS yaml emits these plain.
        assert_eq!(scalar_style("she said \"hi\""), ScalarStyle::Plain);
        assert_eq!(scalar_style("it's fine"), ScalarStyle::Plain);
    }

    #[test]
    fn plain_with_question_not_followed_by_space() {
        assert_eq!(scalar_style("?key"), ScalarStyle::Plain);
    }

    #[test]
    fn quoted_when_question_followed_by_space() {
        assert_eq!(scalar_style("? key"), ScalarStyle::DoubleQuoted);
    }

    #[test]
    fn quoted_for_disallowed_first_chars() {
        for c in [
            '-', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>',
        ] {
            let s = format!("{c}foo");
            assert_eq!(
                scalar_style(&s),
                ScalarStyle::DoubleQuoted,
                "{s:?} should be quoted"
            );
        }
    }

    #[test]
    fn quoted_for_colon_space_in_middle() {
        assert_eq!(scalar_style("key: value"), ScalarStyle::DoubleQuoted);
    }

    #[test]
    fn block_for_multiline_no_trailing_newline() {
        let s = scalar_style("line1\nline2");
        assert_eq!(s, ScalarStyle::LiteralBlock { chomp_strip: true });
    }

    #[test]
    fn block_for_multiline_with_trailing_newline() {
        let s = scalar_style("line1\nline2\n");
        assert_eq!(s, ScalarStyle::LiteralBlock { chomp_strip: false });
    }

    #[test]
    fn block_falls_back_to_quoted_for_control_chars() {
        let s = scalar_style("line1\nline2\x07");
        assert_eq!(s, ScalarStyle::DoubleQuoted);
    }

    #[test]
    fn block_render_basic() {
        let out = render_scalar("p1\np2", 2);
        assert_eq!(out, "|-\n  p1\n  p2");
    }

    #[test]
    fn block_render_with_trailing_newline() {
        let out = render_scalar("p1\np2\n", 2);
        assert_eq!(out, "|\n  p1\n  p2");
    }

    #[test]
    fn block_render_nested_indent() {
        let out = render_scalar("a\nb", 4);
        assert_eq!(out, "|-\n    a\n    b");
    }

    #[test]
    fn double_quote_named_escapes() {
        assert_eq!(double_quote("a\0b"), r#""a\0b""#);
        assert_eq!(double_quote("a\x07b"), r#""a\ab""#);
        assert_eq!(double_quote("a\x08b"), r#""a\bb""#);
        assert_eq!(double_quote("a\tb"), r#""a\tb""#);
        assert_eq!(double_quote("a\nb"), r#""a\nb""#);
        assert_eq!(double_quote("a\x0bb"), r#""a\vb""#);
        assert_eq!(double_quote("a\x0cb"), r#""a\fb""#);
        assert_eq!(double_quote("a\rb"), r#""a\rb""#);
        assert_eq!(double_quote("a\x1bb"), r#""a\eb""#);
    }

    #[test]
    fn double_quote_unmapped_control_uses_hex() {
        // \x01 has no named escape; should use \xNN.
        assert_eq!(double_quote("a\x01b"), r#""a\x01b""#);
    }

    #[test]
    fn double_quote_escapes_quotes_and_backslash() {
        assert_eq!(double_quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(double_quote(r"a\b"), r#""a\\b""#);
    }

    #[test]
    fn plain_for_normal_iso_timestamp() {
        assert_eq!(
            scalar_style("2026-05-13T00:00:00.000Z"),
            ScalarStyle::Plain,
            "ISO timestamps do not contain `: ` (colon-space) so they stay plain"
        );
    }
}
