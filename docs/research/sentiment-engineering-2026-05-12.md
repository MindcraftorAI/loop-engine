# Sentiment Subagent — Engineering Research (2026-05-12)

Pre-write engineering architecture for the sentiment subagent. Classifier choice, calibration, attribution, evaluation.

## Recommended architecture (one-line)

**Claude Haiku 4.5 (cloud, Agent SDK auth) + structured-output JSON + ~3K-token rolling context + run-on-meaningful-turn (regex pretrigger, ~8% of turns) + emit only when calibrated_confidence ≥ 0.75 AND single-best target attributed.**

## Per-turn cost / latency

- Pretrigger fires ~8% of turns (per arXiv:2509.18361 prior)
- Per fired inference: ~$0.0014 with prompt caching, ~2.8s latency (1.5s with cache hit)
- Per active user: ~$0.05/day, ~$1.50/month
- **Must be async w.r.t. assistant reply** — don't block on classifier

## TypeScript interface (drafted)

```ts
interface LoadedItem { id; kind; loadedAtTurn; lastReferencedTurn; summary }
interface SentimentSubagentInput { recentTurns[4-6]; loadedItems[≤20]; sessionId }
interface SentimentSignal {
  itemId; polarity; rawConfidence; calibratedConfidence;
  attributionEvidence; attributionMethod; detectedHazards[]
}
interface SentimentSubagentOutput { signals[]; abstained; abstentionReason?; rawLog }

// MCP tool (internal, LLM-callable, never user-facing)
interface LoopEmitExternalSignalInput {
  lessonId; source: 'sentiment_subagent_v1';
  polarity; strength: number /* 0-1 calibrated */;
  evidence: { sessionId; turnIndex; utteranceSnippet; attributionMethod }
}
```

## Critical gate change required

Current gate at `core/src/lessons/gate.ts:98-105` does a *presence check* on `external_signal_sources`. With weighted sentiment, we need a *budget*:
- Replace presence check with `sum(strength) ≥ 0.75`
- Backwards compat: existing entries (user_thumbs_up etc.) implicitly have strength=1.0
- New parallel `external_signal_weights` map preserves frontmatter compat

**Negative signal asymmetry:** explicit thumbs-down is a hard block. Single negative sentiment is NOT — requires either (a) explicit thumbs-down OR (b) two independent sentiment-negatives with calibrated conf > 0.85.

## Attribution algorithm (5-pass)

```
Pass 1: direct mention (keyword/id match) → conf 0.95
Pass 2: pronoun anaphor on prior assistant turn → conf 0.80
Pass 3: recency × salience, single candidate → conf 0.65
Pass 4: LLM-judged over top-K (only if ≤5 candidates) → conf = classifier output
Pass 5: ABSTAIN — ambiguous referent

Default for ambiguity = abstain. Mis-attribution is more expensive than silence.
```

## Calibration (3-layer)

1. **Consistency-over-samples** — run classifier twice with reordered context; abstain on polarity disagreement; floor at min(scores)
2. **Empirical recalibration table** — bucket verbalized confidences, replace with observed precision against thumbs-up ground truth; refit weekly; model-version-keyed
3. **Reject-option overlay** — drop if calibrated conf < 0.75; negative class requires > 0.85

## Privacy posture

- Cloud Haiku via existing Anthropic auth (zero-config for Claude Code users)
- Send minimal window: 4-6 turns, truncated to 800 tokens each, 1-line item summaries (not bodies)
- Redact secret-shaped strings before send (regex)
- P2 escape hatch: `LOOP_SENTIMENT_LOCAL=1` → Ollama (Qwen3 4B, ~0.5 F1 on sarcasm, half accuracy)
- Audit log at `~/.loop/sentiment-audit/<sessionid>/<turn>.json`, 30-day TTL

## Evaluation strategy (no production ground truth)

1. **Shadow mode 30 days** — emit to log, gate still requires explicit thumbs-up
2. **Co-occurrence proxy** — sentiment-positive + thumbs-up within 14 days = TP. Target P ≥ 0.85 before live
3. **Promotion-rate guardrail** — auto-disable if promotions jump > 2× without usage growth
4. **Adversarial fixtures** — ~50 hand-curated edge cases (sarcasm, dev-talk, performative, ambiguous), regression-tested each upgrade
5. **Per-user holdout** — for users with ≥20 explicit signals, hold out 20%

## Open questions for user

1. **Strength as first-class gate field** — replace presence check with budget? Recommend: yes, threshold 0.75
2. **Negative hard-block?** — recommend NO for sentiment, YES for explicit thumbs-down
3. **Solicitation rate** — recommend ≤1 per session, only when conf straddles threshold > 3 turns
4. **Per-user vs global calibration** — global default, per-user after ≥50 signals
5. **Local-mode parity** — ship with reduced accuracy or feature-flag local mode

## Risks / surprises

1. **Sarcasm SOTA is 0.79 F1 on best LLMs, 0.51 on locally-hostable.** Dev-talk ("this is awful" = self-frustration) is long-tail. Mitigation: hazard flags → auto-abstain
2. **Positive rate is rare (~1%)** per the Gemini Flash precedent — classifier risks low recall on the actionable signal
3. **Attribution failures dominate polarity failures** — spend classifier capacity there
4. **LLMs hallucinate target-stance when no target is salient** — abstention must be default
5. **Calibration drift across model versions** — bucket table must be model-version-keyed
6. **Gate schema migration** — `external_signal_sources` → add parallel `external_signal_weights` map
7. **Adversarial gaming** — rate-limit per source per lesson per session

## Sources

See full report in conversation transcript. Key cites:
- arxiv 2509.18361 — direct precedent (Gemini Flash, 75% acc, 8% signal rate)
- arxiv 2505.08464 — stance detection survey
- arxiv 2410.06707 — verbalized confidence overestimates
- arxiv 2408.11319 — SarcasmBench (0.79/0.51 F1)
- platform.claude.com — Haiku 4.5, structured outputs, prompt caching
