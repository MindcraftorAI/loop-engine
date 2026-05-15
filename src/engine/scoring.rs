//! Text-match scoring shared between lesson + memory recall paths.
//!
//! Phase G (v0.5): promoted from `serve.rs`'s private `score()` helper so
//! the lesson-recall + memory-text-search code paths share one
//! authoritative scorer. Before this module existed, lesson_recall had
//! its own inline `score()` and there was no memory text-match path at
//! all — v0.4 dogfooding caught a real false-negative on "Gianna"
//! (similarity 0.486 < 0.5 default threshold).
//!
//! The function is intentionally hand-rolled (token-overlap + substring
//! bonus) rather than pulling in Tantivy / BM25 — the corpus is small
//! and the user's "no bloat" feedback ruled out a heavy dep just to
//! reuse a 50-line score function. Future cycles can swap the
//! internals without touching call sites.

/// Score a query against a (description, body) field pair.
///
/// Returns a value in `[0.0, 1.0]`. Empty queries return `0.0`. The
/// description field is weighted 2x the body field — proper nouns and
/// other user-facing labels live in descriptions and a match there
/// should rank higher than the same match in body text.
///
/// Final formula:
///   `score_text_match = clamp01((s(q, desc) * 2.0 + s(q, body)) / 3.0)`
/// where `s(q, h)` is `token_overlap(q, h) + substring_bonus(q, h)`,
/// itself clamped to `[0.0, 1.0]`.
///
/// The 2x multiplier mirrors Tantivy/ES BM25 defaults for title vs
/// body weighting; the weighted average prevents description matches
/// from completely drowning body matches.
pub fn score_text_match(query: &str, description: &str, body: &str) -> f32 {
    let q_tokens: std::collections::HashSet<String> = tokenize(query).into_iter().collect();
    if q_tokens.is_empty() {
        return 0.0;
    }
    let desc_score = score_field(&q_tokens, query, description);
    let body_score = score_field(&q_tokens, query, body);
    ((desc_score * 2.0 + body_score) / 3.0).min(1.0)
}

/// Score one field against the query. Pure token-overlap ratio plus a
/// substring bonus, clamped to `[0.0, 1.0]`. Kept private so callers
/// can't accidentally bypass the description weighting in
/// [`score_text_match`].
fn score_field(q_tokens: &std::collections::HashSet<String>, query: &str, haystack: &str) -> f32 {
    let h_tokens: std::collections::HashSet<String> = tokenize(haystack).into_iter().collect();
    let overlap = q_tokens.iter().filter(|t| h_tokens.contains(*t)).count() as f32;
    let token_score = overlap / q_tokens.len() as f32;
    let substring_bonus = if haystack.to_lowercase().contains(&query.to_lowercase()) {
        0.3
    } else {
        0.0
    };
    (token_score + substring_bonus).min(1.0)
}

/// Tokenize on ASCII-alphanumeric runs, lowercased, dropping tokens of
/// length ≤1 (noise filter). Public because future callers may want
/// to inspect token sets directly; the surface is intentionally
/// stable (changing it would shift scores).
pub fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .filter(|t| t.len() > 1)
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_scores_zero() {
        assert_eq!(score_text_match("", "anything", "anything"), 0.0);
        assert_eq!(score_text_match("   ", "x", "y"), 0.0);
    }

    #[test]
    fn exact_description_match_scores_high() {
        // Token overlap 1.0 (1/1) + substring 0.3 → field score 1.0
        // (clamped). With 2x desc weight + 0.0 body: (1.0*2 + 0.0)/3
        // = 0.666. Capped at 1.0 so no overflow.
        let s = score_text_match("Gianna", "Sangmin's family — daughter Gianna", "");
        assert!(s > 0.6, "got {s}");
        assert!(s <= 1.0);
    }

    #[test]
    fn body_only_match_scores_lower_than_desc() {
        let desc_match = score_text_match("Gianna", "family memory", "Gianna is 4");
        let same_in_desc = score_text_match("Gianna", "family Gianna memory", "the body");
        assert!(
            same_in_desc > desc_match,
            "desc-match ({same_in_desc}) should beat body-match ({desc_match})"
        );
    }

    #[test]
    fn substring_bonus_fires() {
        // Token "ll" is too short (filtered by tokenize length>1
        // rule), so token_score is 0. Pure substring bonus = 0.3.
        // Picked a token of length 2 that's a substring of the
        // haystack but not a separate word — "ll" inside "rollout".
        let s = score_text_match("ll", "kubectl rollout", "");
        // Token "ll" stays after tokenize (len>=2); haystack
        // tokens include "ll" only as substring, not as full token,
        // so token_score = 0 but substring_bonus fires.
        assert!(s > 0.0, "substring bonus should fire, got {s}");
    }

    #[test]
    fn no_match_scores_zero() {
        let s = score_text_match("xyzqwerty", "completely unrelated", "different again");
        assert_eq!(s, 0.0);
    }

    #[test]
    fn partial_token_overlap() {
        // Query "Rust borrow checker" against "Rust enforces borrow".
        // Overlap: rust + borrow = 2/3 = 0.666. No substring match.
        // Field score = 0.666 capped to ≤1.0. With 0 body:
        // (0.666 * 2 + 0) / 3 = 0.444.
        let s = score_text_match("Rust borrow checker", "Rust enforces borrow", "");
        assert!(s > 0.3 && s < 0.6, "got {s}");
    }

    #[test]
    fn tokenize_drops_punctuation_and_short_tokens() {
        let toks = tokenize("Sangmin's family — daughter Gianna (age 4)");
        assert!(toks.contains(&"sangmin".to_string()));
        assert!(toks.contains(&"family".to_string()));
        assert!(toks.contains(&"daughter".to_string()));
        assert!(toks.contains(&"gianna".to_string()));
        // "s" (from possessive) is len 1 → dropped
        assert!(!toks.contains(&"s".to_string()));
        // "4" is len 1 → dropped
        assert!(!toks.contains(&"4".to_string()));
    }
}
