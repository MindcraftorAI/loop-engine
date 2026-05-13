# Sentiment Subagent — Linguistics Research (2026-05-12)

Pre-write research on inferring user approval/disapproval of loaded AI context from natural conversation. Conversational Analysis + Pragmatics + Appraisal Theory angle.

## Key architectural conclusion

**The sentiment subagent is an EVIDENCE producer, not a JUDGE.** It emits structured `external_signals[]` entries with `kind / polarity / weight / referent / confidence / quoted_span / surrounding_turns`. The gate aggregates and decides promotion/demotion.

## Signal taxonomy with weights

Range: −1.0 (strong negative) to +1.0 (strong positive). Aggregate via saturating function, not sum.

### Positive — high confidence
- `explicit_praise_with_referent` +0.8 — "exactly what I needed" naming the thing
- `terminology_adoption` +0.4 — user unprompted-reuses LLM's terms (N≥2)
- `yes_and_extension` +0.4 — Pomerantz preferred turn: no gap, builds on
- `flow_continuation` +0.2 — no repair, increased specificity over turns

### Positive — interpretive
- `booster_agreement` +0.3 — "definitely", "absolutely"
- `litotes_positive` +0.15 — "not bad", "not wrong"
- `casual_ack` +0.05 — "yeah ok", near-zero floor

### Negative — high confidence
- `dispreferred_turn_shape` −0.5 — gap+hedge+account+disagreement
- `rephrase_repeat` −0.4 first, −0.7 second — same intent retried
- `imperative_collapse` −0.5 — terseness, dropped politeness vs baseline
- `other_initiated_repair` −0.3 — "huh?", "wait what?", "are you sure?"
- `lexical_frustration` −0.7 — profanity, "useless", "broken"
- `punctuation_escalation` −0.3 — "??", "!!", CAPS
- `explicit_error_pointing` −0.6 — "that's wrong", "you missed X"

### Negative — interpretive
- `hedged_disagreement` −0.25 — "I wonder if...", "could we try..."
- `actually_correction` −0.3 — turn-initial "actually"
- `intent_concretization_post_response` −0.3 — user re-specifies after AI reply
- `task_subdivision_post_response` −0.25 — user breaks task smaller after reply

### Ambiguous
- `topic_shift` 0.0 — resolved by trajectory (closure marker present?)
- `silence` 0.0 — resolved by trajectory + time-of-day
- `esc_interrupt` −0.4 default, −0.8 mid-tool-use

## Disambiguators (false-positive guards)

- "this is broken" → check referent: code-frustration vs AI-frustration
- Sarcasm "oh great" → polarity inversion + exaggeration + emoji cluster
- Performative complaint "ugh fine" → check compliance follow-through
- Playful "that's terrible" → emoji, laughter, repeated vowels
- "not exactly what I asked" → whole-phrase parse (litotes negative)
- User self-repair vs OISR → "I mean" / "let me rephrase" = self
- Topic shift → closure token present? ("ok cool", "ship it") = satisfied

## Attribution heuristics (multi-target)

Ranked by reliability:
1. **Referent match** — user names the lesson/skill or its content
2. **Content overlap** — semantic overlap between complaint and loaded context
3. **Behavioral fingerprint** — lesson L → uniquely causes behavior X → user complains about X
4. **Temporal locality** — frustration immediately after lesson-L-influenced turn
5. **Counterfactual hint** — "why did you do X?" → X's cause is suspect
6. **Default ambiguous → session-level**, not specific lesson
7. **ESC interrupt special case** — targets current trajectory not necessarily a loaded lesson

## Solicitation phrasing principles

Social-desirability bias is the main hazard. Rules:
- Ask about work product, not feelings about the AI
- Pre-license the negative answer in the prompt
- Offer asymmetric options that pre-name the bad case ("keep / scale back / drop")
- Keep preamble <25 words
- Avoid moralized praise terms ("good", "right") in the question
- Bury the ask at natural beats, don't headline as modal

Sample (graded by expected honesty):
- HIGH: "I've been auto-prefixing all commits with a ticket ID — want me to keep, scale back, or drop that?"
- LOW (yes-biased): "Did that work for you?"

## Open questions (need empirical work)

1. Baseline drift per user (curt-by-default vs polite-by-default)
2. Sarcasm in code contexts — no corpus exists
3. Cross-cultural calibration (high-context vs low-context)
4. Cumulative vs acute signal aggregation
5. Adversarial sycophantic compliance ("thanks, perfect" to end faster)

## Sources

See full report in conversation transcript. Key cites:
- Pomerantz (1984) — preferred/dispreferred assessments
- Martin & White (2005) — Appraisal Theory
- Brown & Levinson — politeness theory
- COLING 2025 — user frustration detection
- arxiv 2311.07434 — ChatGPT dissatisfaction taxonomy
- arxiv 2507.23158 — implicit feedback is informative but noisy
