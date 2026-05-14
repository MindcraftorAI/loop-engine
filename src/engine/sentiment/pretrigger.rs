//! Pretrigger — fast regex pre-filter on user utterances.
//!
//! The classifier (LLM call, ~800ms) is expensive; most user turns
//! carry no sentiment-actionable signal at all. Pretrigger is a cheap
//! sync regex that rejects ~95% of user turns before the classifier
//! is even consulted.
//!
//! Locked decisions (learn-notes D2 + OQ3):
//! - `regex = "1"` (1.11.x), promoted to direct dep
//! - `LazyLock<Regex>` for one-time compilation
//! - Wrapped in [`Pretrigger`] struct (test-injection, future per-locale)
//! - `Pretrigger::default()` instead of `default_en()` (KISS for Day 15)
//!
//! Pattern lineage: matches the TS-side pretrigger at
//! `loop-archive-2026-05-13/core-ts/src/sentiment/types.ts:87` including
//! audit-A1 fixes for negative contractions and low-register lexicon.

use std::sync::{Arc, LazyLock};

use regex::Regex;

/// The default pretrigger pattern. Mirrors the TS pretrigger with the
/// audit-A1 fixes baked in:
///   - negative contractions (`don't`, `doesn't`, `won't`, `can't`) using
///     a `'` / `'` / no-quote tolerant character class
///   - low-register positives (`nice`, `cool`, `sweet`, `lol`, `lmao`)
///   - low-register negatives (`ugh`, `meh`, `fuck`, `wtf`, `bullshit`)
///   - interrupt sentinel (`[Request interrupted`)
///
/// Word-boundary anchored, case-insensitive. Smart-quote tolerant via
/// the `[']` character class (single quote, U+2019 right single quotation
/// mark, U+2018 left single quotation mark).
const DEFAULT_PATTERN: &str = r"(?ix)
    (?:
        # Positive — high register
        \b (?:
            thanks?       | thank\s+you   | perfect      | exactly
          | great         | awesome       | excellent    | brilliant
          | nice          | cool          | sweet        | yay
          | lol           | lmao          | rofl
          | (?:you['‘’]?re|youre|that['‘’]?s|thats)\s+right
        ) \b
      |
        # Negative — high register
        \b (?:
            wrong         | broken        | sorry        | bad
          | no(?:pe)?     | not           | stop         | bug
          | error         | fail(?:ed|s)? | bullshit
        ) \b
      |
        # Negative — contractions (audit-A1)
        \b (?:
            do['‘’]?n['‘’]?t
          | does['‘’]?n['‘’]?t
          | did['‘’]?n['‘’]?t
          | won['‘’]?t
          | can['‘’]?t        | cannot
          | should['‘’]?n['‘’]?t
          | would['‘’]?n['‘’]?t
          | could['‘’]?n['‘’]?t
          | is['‘’]?n['‘’]?t
          | are['‘’]?n['‘’]?t
          | was['‘’]?n['‘’]?t
          | were['‘’]?n['‘’]?t
        ) \b
      |
        # Low-register negatives (audit-A1)
        \b (?: ugh | meh | wtf | fuck (?:ing|ed)? ) \b
      |
        # Interrupt sentinel
        \[Request\s+interrupted
    )
";

static DEFAULT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(DEFAULT_PATTERN).expect("compiled DEFAULT_PATTERN; bug if this fails")
});

/// Pretrigger over the default pattern, or a custom pattern (test
/// injection). Cheap to construct; the regex itself is shared via
/// `LazyLock` in the default case.
#[derive(Debug, Clone)]
pub struct Pretrigger {
    regex: PretriggerRegex,
}

#[derive(Debug, Clone)]
enum PretriggerRegex {
    Default,
    /// Only constructed via `Pretrigger::with_pattern`, which is gated
    /// behind `#[cfg(any(test, feature = "test-fixtures"))]`. In default
    /// production builds the variant is reachable but never constructed,
    /// which is the intended design — silence the dead-code lint.
    #[allow(dead_code)]
    Custom(Arc<Regex>),
}

impl Default for Pretrigger {
    fn default() -> Self {
        Self {
            regex: PretriggerRegex::Default,
        }
    }
}

impl Pretrigger {
    /// Compile a custom pattern. Available to tests and to consumers
    /// that opt into the `test-fixtures` feature.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn with_pattern(pattern: &str) -> Result<Self, regex::Error> {
        let r = Regex::new(pattern)?;
        Ok(Self {
            regex: PretriggerRegex::Custom(Arc::new(r)),
        })
    }

    /// True when the text contains at least one pretrigger match.
    pub fn fires_on(&self, text: &str) -> bool {
        match &self.regex {
            PretriggerRegex::Default => DEFAULT_REGEX.is_match(text),
            PretriggerRegex::Custom(r) => r.is_match(text),
        }
    }
}

// =====================================================================
// Tests — ~30 adversarial fixtures (learn-notes OQ8)
// 10 positive, 10 negative, 10 edge
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn pretrigger() -> Pretrigger {
        Pretrigger::default()
    }

    // ---- 10 positives ----

    #[test]
    fn fires_on_thanks() {
        assert!(pretrigger().fires_on("thanks!"));
    }

    #[test]
    fn fires_on_thank_you() {
        assert!(pretrigger().fires_on("thank you for that"));
    }

    #[test]
    fn fires_on_perfect() {
        assert!(pretrigger().fires_on("that's perfect"));
    }

    #[test]
    fn fires_on_exactly() {
        assert!(pretrigger().fires_on("exactly what i needed"));
    }

    #[test]
    fn fires_on_great() {
        assert!(pretrigger().fires_on("works great"));
    }

    #[test]
    fn fires_on_nice() {
        assert!(pretrigger().fires_on("nice"));
    }

    #[test]
    fn fires_on_youre_right() {
        assert!(pretrigger().fires_on("you're right"));
    }

    #[test]
    fn fires_on_thats_right_smart_quote() {
        assert!(pretrigger().fires_on("that\u{2019}s right"));
    }

    #[test]
    fn fires_on_awesome() {
        assert!(pretrigger().fires_on("awesome work"));
    }

    #[test]
    fn fires_on_lol() {
        assert!(pretrigger().fires_on("lol"));
    }

    // ---- 10 negatives ----

    #[test]
    fn fires_on_wrong() {
        assert!(pretrigger().fires_on("this is wrong"));
    }

    #[test]
    fn fires_on_broken() {
        assert!(pretrigger().fires_on("it's broken"));
    }

    #[test]
    fn fires_on_dont_contraction() {
        assert!(pretrigger().fires_on("i don't want that"));
    }

    #[test]
    fn fires_on_doesnt_smart_quote_contraction() {
        assert!(pretrigger().fires_on("it doesn\u{2019}t work"));
    }

    #[test]
    fn fires_on_cant_no_apostrophe() {
        assert!(pretrigger().fires_on("you cant do that"));
    }

    #[test]
    fn fires_on_stop() {
        assert!(pretrigger().fires_on("stop"));
    }

    #[test]
    fn fires_on_no() {
        assert!(pretrigger().fires_on("no"));
    }

    #[test]
    fn fires_on_ugh() {
        assert!(pretrigger().fires_on("ugh"));
    }

    #[test]
    fn fires_on_wtf() {
        assert!(pretrigger().fires_on("wtf"));
    }

    #[test]
    fn fires_on_interrupt_sentinel() {
        assert!(pretrigger().fires_on("[Request interrupted by user]"));
    }

    // ---- 10 edge cases (non-firing or tricky) ----

    #[test]
    fn does_not_fire_on_empty_string() {
        assert!(!pretrigger().fires_on(""));
    }

    #[test]
    fn does_not_fire_on_neutral_text() {
        assert!(!pretrigger()
            .fires_on("could you read the file at src/main.rs and tell me the imports"));
    }

    #[test]
    fn does_not_fire_on_thanksgiving_substring() {
        // Word-boundary should prevent matching "thanks" inside "thanksgiving".
        // Note: regex \b allows "thanksgiving" to actually match because
        // "thanks" ends at a word boundary internal to compound — verify behavior.
        // The TS-side audit found this; we mirror: "thanksgiving" SHOULD NOT fire.
        let fires = pretrigger().fires_on("happy thanksgiving");
        // Document current behavior: regex \b after "thanks" looks at next
        // char ("g"); since g is a word char, no word boundary exists, so
        // "thanks" inside "thanksgiving" is NOT matched. Correct.
        assert!(!fires, "'thanksgiving' should not pretrigger");
    }

    #[test]
    fn fires_on_thanks_with_emoji_suffix() {
        assert!(pretrigger().fires_on("thanks 🙏"));
    }

    #[test]
    fn fires_on_mixed_case() {
        assert!(pretrigger().fires_on("THANKS!"));
        assert!(pretrigger().fires_on("ThAnKs"));
    }

    #[test]
    fn fires_on_thanks_with_leading_punct() {
        assert!(pretrigger().fires_on("...thanks"));
    }

    #[test]
    fn does_not_fire_on_noted_substring() {
        // "noted" contains "not" but only as a prefix; word-boundary
        // should reject.
        assert!(!pretrigger().fires_on("noted"));
    }

    #[test]
    fn does_not_fire_on_innot_substring() {
        // "cannot" should fire (explicit), but words like "annot" should not.
        assert!(!pretrigger().fires_on("hello annotation"));
        assert!(pretrigger().fires_on("cannot do that"));
    }

    #[test]
    fn fires_on_perfect_within_longer_sentence() {
        assert!(pretrigger().fires_on("this is the perfect amount of detail thank you"));
    }

    #[test]
    fn custom_pattern_matches_only_its_own_lexicon() {
        let custom = Pretrigger::with_pattern(r"(?i)\bfoobar\b").unwrap();
        assert!(custom.fires_on("foobar"));
        assert!(!custom.fires_on("thanks"));
    }
}
