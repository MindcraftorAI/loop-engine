# ADR-0008: Closed-model-first stance, with open-weight support as non-special-cased

**Status:** Accepted
**Date:** 2026-05-11

## Context

LOOP's core mechanism is in-context learning, not training. This works with any LLM. But every design decision can subtly favor one ecosystem or another.

Hermes Agent (closest comparable project) leans toward open-weight LLMs — its full vision involves Atropos fine-tuning of cheaper open models on captured trajectories. Hermes's identity is in the open-weight community (Nous Research is an open-weight model lab).

LOOP needs an explicit stance.

## Decision

LOOP is **closed-model-first**, with open-weight model support treated as a non-special-cased capability.

- Default development and dogfooding targets Claude (specifically Claude Code as the dogfood host)
- Documentation and examples use closed models as the assumed runtime
- Open-weight models (Llama, Qwen, DeepSeek, etc.) work — but are not the primary target
- Training-based optimization (Atropos-style) is **out of scope for LOOP's beta and likely v1**. If exposed later, it's as trajectory data export to partner training services — never as in-house training infrastructure (see [VISION.md](../VISION.md), trajectory-as-export middle path)

## Reasoning

- **LOOP rides closed-model improvements.** As Claude / GPT / Gemini get smarter, LOOP gets more leverage. Closed models are improving rapidly.
- **Closed-model users are the bigger market.** The audience using Claude Code, Claude.ai, Cursor, ChatGPT vastly outnumbers the open-weight community.
- **Differentiation from Hermes.** Hermes already serves the open-weight-community audience well. LOOP serving the closed-model audience is broader market without competing on Hermes's home turf.
- **No training infrastructure means lighter ops.** Closed models don't require GPU management, model hosting, fine-tuning pipelines.
- **In-context learning works equally well on closed and open models.** The decision doesn't reduce LOOP's capability — only its identity emphasis.

## Consequences

**Pros:**
- Broader market appeal — meets users where most are
- No GPU / training infrastructure dependency
- Documentation and examples are cleaner (one assumed model family)
- Avoids dragging in open-weight ecosystem complexity (model serving, quantization, hardware concerns)

**Cons:**
- Lose ideological alignment with parts of the open-source AI community
- Cost-conscious-at-scale users (running thousands of agent runs daily) may prefer Hermes's fine-tune-cheap-models pitch
- LOOP's value is bounded by the closed model's capability ceiling — but in-context learning is bounded by any model's ceiling, so this is universal, not closed-model-specific

## What "non-special-cased" support for open models means

- Anthropic and OpenAI SDKs are the primary LLM clients in beta
- LiteLLM-style abstraction can be added if open-weight support becomes a meaningful demand — but only as a layer over the existing LLM-call abstraction, not a fork
- No open-weight-specific features (e.g., custom quantization, hardware acceleration) ship in beta
- An open-weight user can use LOOP — they just point the LLM client at any OpenAI-compatible endpoint (Together AI, vLLM, OpenRouter, etc.)

## Alternatives considered

- **Open-weight-first (Hermes shape):** rejected — head-to-head with Hermes on its strongest ground; smaller audience; drags in training infrastructure expectations
- **True model-agnostic (no default):** rejected for beta — design without a default leads to lowest-common-denominator decisions; pick a default explicitly and broaden later
- **Closed-model-only:** rejected — gratuitously cuts off a real user segment for no benefit

## Related

- [VISION.md](../VISION.md) — what LOOP isn't (not a training service)
- [COMPETITIVE.md](../COMPETITIVE.md) — Hermes comparison, model-stance differentiator
