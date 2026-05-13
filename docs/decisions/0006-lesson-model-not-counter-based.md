# ADR-0006: Lesson model is not counter-based — uses human-learning patterns

**Status:** Accepted
**Date:** 2026-05-09

## Context

Existing memory products (Mem0, Letta, Zep) use counter-based accumulation for memory updates — collect enough signals, cross a threshold, commit the change. A first draft of LOOP's promotion logic followed this pattern.

User pushed back: counter-based accumulation is the wrong abstraction because humans don't learn that way. Real learning involves salience, surprise, causal narrative, pattern crossover, contradiction, consolidation — not just signal counting.

## Decision

LOOP's Lesson model rejects counter-based accumulation as the core mechanism. Instead, it follows a multi-factor judgment that reflects how humans actually encode lessons.

### Required for promotion
- **Causal narrative** exists (the *why*) — LLM-assisted draft, refined as signal accumulates
- **Application phase passed** — lesson was served in inference and produced positive outcomes
- **Consolidation event passed** — explicit periodic review approved the lesson
- **Zero strong negatives** during application phase

### Any one of (sufficient trigger)
- **Salience high enough alone** — one severe / high-stakes event can warrant promotion without volume
- **Volume accumulated** — many positive signals over time
- **Pattern crossover validated** — same insight in 2+ different contexts

### Status lifecycle
```
observed → hypothesized → active → applied → validated → consolidated → promoted
                                       ↓
                             (or discarded, expired, superseded)
```

When `active`, the lesson is layered onto the skill's effective content at inference time — so users get in-flight learning immediately, not only after promotion.

## Consequences

**Pros:**
- Matches how humans actually encode lessons (salience overrides volume; surprise > predictability; narrative required; pattern crossover validates)
- Differentiates from Mem0 / Letta / Zep, which all do simple accumulation
- Consolidation phase is genuinely novel — the "sleep on it" step no other memory product has
- Active-but-not-promoted lessons enable continuous compounding (not stepwise)

**Cons:**
- More complex than counter-based — more code, more LLM calls (narrative generation, consolidation)
- Causal narrative requires LLM generation — token cost
- Consolidation phase needs scheduling infrastructure

## Promotion policy structure (configurable)

Behind the required + sufficient-trigger checks, a configurable `PromotionPolicy` controls how six dimensions weight together:

- Volume, score, ratio, time window, negative floor, app-defined metric

Each dimension can be **REQUIRED** (hard gate), **WEIGHTED** (contributes to composite score), or **INFORMATIONAL** (tracked, non-gating). Presets ship: `default`, `safety_critical`, `fast_iteration`, `trending`, `production_critical`. Skills override per-skill in MD frontmatter.

## Strategic significance

This is the model where LOOP genuinely differentiates from Mem0 / Letta / Zep / Hermes Agent. If "continuous context compounding" is LOOP's wedge, this is what makes it real instead of marketing.

## Alternatives considered

- **Pure counter-based:** rejected — simpler but doesn't match how humans learn; differentiation collapses
- **Single-dimension threshold:** rejected — different scenarios (safety-critical, trending content, production-critical) weight dimensions differently; one rule doesn't fit
- **Promote directly from FeedbackSignal:** rejected — no staging area means no rollback safety, no observability into in-flight learning

## Related

- [ARCHITECTURE.md](../ARCHITECTURE.md) — Lessons section
- [DATA_MODEL.md](../DATA_MODEL.md) — Lesson entity
