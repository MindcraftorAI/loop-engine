# ADR-0003: Two-tier product — free self-hosted + paid hosted SaaS

**Status:** Accepted
**Date:** 2026-05-09 (sequence decided 2026-05-09: free first, paid as fast-follow)

## Context

LOOP needs a commercial model that aligns with its backbone-infrastructure identity and the user's preference for accessible open distribution combined with sustainable revenue. The well-trodden pattern in this space (Supabase, Vercel, Hugging Face, n8n, Sentry, Plausible) is open core: free self-hosted + paid hosted SaaS.

## Decision

Two tiers:

- **Free tier = self-hosted LOOP.** Runs on user's laptop, server, container, or own cloud infrastructure. Includes the 4-stage loop, multi-agent orchestration, auto-updating skills + memory, MCP server, local persistence, user-authored skills, bundle import/export, best-effort live-source ingestion.
- **Paid tier = LOOP-hosted SaaS.** Device sync, hosted 24/7 live-source ingestion, auth + accounts, multi-tenancy for app builders (API tier), team-shared memory, marketplace participation (post-beta), centralized data pool participation (post-beta).

**Beta sequencing:** free tier ships first. Paid tier follows as a fast-follow.

**"Local" means self-hosted, not laptop-only.** RankLabs deploys LOOP inside its own cloud infrastructure as a free-tier deployment during beta.

## Consequences

**Pros:**
- Strong adoption story (free path that respects users — privacy, no lock-in)
- Validated pattern (Supabase, Postgres, n8n, etc. all reached unicorn or strong outcomes via this)
- Beta scope is smaller without multi-tenant cloud infrastructure
- RankLabs can validate LOOP without paying for hosted infra during beta
- Paid tier value is concrete: sync, hosted services, multi-tenancy, marketplace, enterprise features

**Cons:**
- Two deployment models to maintain (mitigated by sharing data model and skill/memory formats across both)
- Free-to-paid conversion is typically 1-5% in this category — need top-of-funnel
- Paid tier doesn't ship in beta, so revenue is delayed until v1.1+

## What's in the OSS code vs proprietary

See [ADR-0009](0009-open-core-licensing.md) for the licensing strategy.

## Alternatives considered

- **Closed source only:** rejected — competes with OSS Hermes Agent on its strongest ground (adoption), and infrastructure products without OSS adoption struggle in this space
- **OSS only, no paid tier:** rejected — no sustainable revenue model; user explicitly wants commercial product
- **Free + paid together at launch:** rejected — doubles beta build scope; user prefers gauging reactions to free-tier beta before committing to cloud infra investment

## Related

- [BETA_SCOPE.md](../BETA_SCOPE.md)
- [ADR-0009](0009-open-core-licensing.md)
