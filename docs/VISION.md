# Vision

## What LOOP is

LOOP is a persistent self-improving context layer for AI agents. It exists because every LLM-powered product today hits the same wall: the model forgets, skills don't compound, and the same context gets rebuilt from scratch every session.

LOOP closes that gap.

## The thesis

LOOP changes the **context fed to a model**, not the model itself. As you use it, skills and memory accumulate **Lessons** — provisional learnings that, once validated, become permanent improvements. Every session improves the next.

This is in-context learning, not training. It works with any LLM (Claude, GPT, Gemini, open-weight) and requires no GPUs, no fine-tuning, no model deployment. The improvements happen at inference time, every call.

## What LOOP isn't

- **Not a training service** — in-context, not weight-update
- **Not an LLM provider** — orchestrates whichever LLM you're already using
- **Not a runtime replacing your tools** — slots into Claude Code, Cursor, etc. via MCP
- **Not "approaching AGI"** — one step closer; an incremental advance on a missing primitive (persistent self-improving context)

## What LOOP promises

LOOP doesn't promise your AI gets smarter. It promises **your AI becomes increasingly useful to you over time**.

- **Capability ceiling** — set by the underlying model. LOOP doesn't change it.
- **Utilization rate** — set by context quality. LOOP raises it dramatically.

Most users get 20-30% of what closed models like Claude or GPT can do. LOOP pushes that to 70-80% by carrying skills, memory, and lessons across every session.

## Three audience tiers

All served by the same core infrastructure:

1. **End users** plug LOOP into their existing LLM host (Claude Code, Claude.ai, Cursor, ChatGPT) via MCP. They get persistent role personas (UX designer, PM, vibe coder), skills that track live documentation, and memory that follows them across tools.
2. **App builders** embed LOOP into their AI-native products. Indie / vertical AI builders use LOOP as the substrate so they don't have to build memory + skill infrastructure themselves.
3. **Multi-agent system authors** use LOOP as the persistent layer underneath their orchestration. Each agent gets role-specific memory and skills that compound.

## Reference applications

- **RankLabs** — already runs LOOP's architecture in production for ecommerce AI-search visibility. Bootstrap proof; not a future reference to design against.
- **Content generator** — planned standalone product built on LOOP. Generalization test for whether the API holds outside RankLabs's vertical.

## Realistic positioning

LOOP is for users and builders who want to enhance their existing LLM workflow without changing tools. We meet you where you already work. Hermes Agent (the nearest comparable project) is its own runtime — it asks users to come to it. Different audience, different surface, different jobs.

## Eventual pitch framing (post-beta)

> "LOOP is the upload jack for AI agents. Plug in a context pack and the agent is instantly capable in a new domain — like Neo learning kung fu."

Technically accurate (inference-time context, not training). Saved for the pitching phase, not the build phase.

## Why now

- Persistent agent memory is the most-requested missing primitive in LLM tooling
- MCP is mature enough to be a real distribution surface (Claude Code, Claude.ai, Cursor all support it)
- Closed models are getting good enough that in-context utilization gains are large in absolute terms
- No incumbent has fused auto-evolving memory + auto-evolving skills + multi-agent + MCP integration into one product
