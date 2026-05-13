# Sentiment Subagent — Design Rules (consolidated 2026-05-12)

Synthesized from three parallel research threads: linguistics (CA + Pragmatics + Appraisal), engineering (classifier + calibration), psychology (HCI + biases + chilling effects). Full reports in `sentiment-linguistics-*.md` and `sentiment-engineering-*.md`.

## Hard rules (architectural constraints)

1. **Subagent is an EVIDENCE producer, not a JUDGE.** Emits structured signals with kind/polarity/weight/referent/confidence/quoted_span. The gate aggregates and decides.

2. **In-session signal only.** Cross-session retrospective ("was that lesson useful last week?") is confabulation, not evidence. Hard cutoff: signals captured within the same session as the lesson's activation.

3. **Observe in flow; never name it back.** Never say "you seemed frustrated" — that's the uncanny zone and triggers chilling effects. Act silently on the inference.

4. **Default = abstain.** Mis-attribution is more expensive than silence. Ambiguous referent → emit no signal.

5. **Asymmetric thresholds.** Positive ≥ 0.75 calibrated confidence; negative ≥ 0.85. Cost of falsely promoting a bad lesson is integrated forward; cost of falsely blocking a good one is recoverable.

6. **Hard-block via explicit thumbs-down only.** Negative sentiment never single-handedly hard-blocks — requires either (a) explicit thumbs-down OR (b) two independent sentiment-negatives.

7. **Strength as first-class gate field.** Replace `external_signal_sources` presence check with `sum(strength) ≥ 0.75` budget. Backwards compat: existing entries get implicit strength=1.0.

## Solicitation rules

8. **Max 1 solicited prompt per ~20 turns.** Reactance ceiling. Cooldown counter resets per prompt.

9. **Discount solicited signal by 30-50% vs unsolicited.** Social desirability bias inflates solicited responses.

10. **Forced-choice with concrete alternative.** "Keep / scale back / drop?" — never "did this help?"

11. **Strip provenance from elicitation phrasing.** No "I loaded that skill for you — useful?" Triggers politeness compliance.

12. **Behavioral, not attitudinal.** Ask about future actions, not past feelings.

## Attribution rules

13. **Five-pass attribution.** Direct mention (0.95) → pronoun anaphor (0.80) → recency single-candidate (0.65) → LLM-judged top-K (variable) → abstain.

14. **Weight by recency + effort-visibility.** Frustration immediately after lesson-L-influenced turn → high prior on L. Frustration when AI was silent → low prior on any loaded item.

15. **Frustration without proximal AI/skill action does NOT decrement skill score.** Code-frustration, third-party-library-frustration, self-blame must not propagate.

16. **Default to session-level for ambiguous attribution.** Never to a specific lesson without high-confidence referent.

## Transparency posture

17. **Transparent at system level, silent at moment-to-moment.** README/onboarding documents what's inferred, what signals are watched, how to inspect/disable. In-flow, the subagent is invisible.

18. **Sentiment-to-behavior pathway is opaque to users.** Don't expose which utterance maps to which weight. Prevents gaming and reduces operant conditioning risk.

## Privacy + storage

19. **Minimal window only.** Send 4-6 recent turns truncated to 800 tokens each + 1-line item summaries. Never full lesson bodies. Regex-redact secret-shaped strings.

20. **Audit log at `~/.loop/sentiment-audit/<sessionid>/<turn>.json`** with 30-day TTL. Required for calibration pipeline and user inspection.

## Hazards that auto-abstain

- Sarcasm suspected (polarity inversion + exaggeration + emoji/laughter)
- Low register volatility (developer cursing as baseline)
- Ambiguous referent (≥2 plausible loaded items)
- Faux-pas-class oversteps (LLM ToM weak spot — known blind inheritance)

## Evaluation strategy

- Shadow mode 30 days; emit to log, gate still requires explicit thumbs-up
- Co-occurrence proxy: sentiment-positive + later thumbs-up within 14 days = TP
- Target precision ≥ 0.85 before flipping to live mode
- Adversarial fixtures (~50 hand-curated edge cases) as regression gate
- Auto-disable if promotion rate jumps > 2× without usage growth

## Confidence calibration

- Verbalized scores are over-confident — discount before thresholding
- Run classifier twice with reordered context; abstain on polarity disagreement
- Empirical recalibration: bucket verbalized scores, replace with observed precision against explicit signals, refit weekly, model-version-keyed

## Open questions (need user decision)

- Per-user calibration vs global (recommend: global default, per-user after ≥50 signals)
- Local-mode parity (ship reduced-accuracy mode behind feature flag?)
- Solicitor frequency (recommend: only when conf straddles threshold > 3 turns AND ≥20 turns since last prompt)
