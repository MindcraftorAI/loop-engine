# ADR-0009: Open Core licensing — MIT-licensed engine + closed proprietary platform

**Status:** Accepted
**Date:** 2026-05-11

## Context

LOOP must pick a licensing strategy that supports both adoption (especially against OSS competitors like Hermes Agent) and sustainable commercial revenue. Three viable patterns:

- **Fully OSS / no commercial entity** — Hermes shape. Maximizes adoption, no revenue.
- **Closed source only** — proprietary throughout. Cleanest commercial control, hardest adoption story for an infrastructure product.
- **Open core** — OSS engine, proprietary commercial platform on top. Validated by Supabase, MongoDB, Elastic (pre-relicense), GitLab, Sentry, Hugging Face, n8n, Plausible.

The two-tier product structure ([ADR-0003](0003-two-tier-free-self-hosted-paid-saas.md)) already commits LOOP to "free self-hosted + paid hosted SaaS." Open core is the natural license model for this product shape.

## Decision

LOOP uses an **Open Core** licensing model.

### What's open source — the `core/` repo
- LOOP engine itself (4-stage loop, multi-agent orchestration)
- Memory layer (with provider abstraction)
- Skill system + Lesson model
- MCP server
- Bundle format + import/export
- Local persistence (SQLite + FTS5)
- All free-tier functionality

**License:** **MIT.** Maximally permissive. Hermes uses MIT — LOOP picks MIT to be on equal footing for adoption.

### What's proprietary — separate private repos (never public)
- Hosted SaaS platform (multi-tenancy infrastructure, sync, hosted ingestion)
- Marketplace platform (discovery, payments, ratings, moderation)
- Centralized data pool tooling (privacy / compliance infrastructure)
- Enterprise governance (SSO, audit logs, RBAC)
- Billing / subscriptions

## Dependency license discipline

The OSS core's permissive license must not be compromised by copyleft dependencies. The hosted/proprietary code paths must also not be locked into copyleft.

### Allowed dependency licenses
- **MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC** — permissive, safe for closed code
- **CC0, Unlicense** — public domain
- **LGPL-2.1, LGPL-3.0** — weak copyleft; Node dynamic linking generally OK but **review usage** before adding
- **MPL-2.0** — file-level copyleft; mixable but **review terms** before adding

### Blocked dependency licenses
- **AGPL-1.0, AGPL-3.0** — network-use copyleft, kills closed-source SaaS
- **GPL-2.0, GPL-3.0** — strong copyleft
- **SSPL-1.0** — MongoDB's anti-SaaS license
- **CC-BY-NC** — non-commercial restriction
- **Commons Clause modifications / BSL / source-available** — case-by-case, default-deny

### Enforcement (when `core/` is scaffolded)
- `license-checker` as a dev dependency
- CI step fails the build on any disallowed license in the dependency tree
- Optional pre-commit hook (can be slow on large trees)
- Allowed/blocked list maintained in `core/.licenseignore` or equivalent config

This is not optional. **No AGPL / GPL / SSPL dependencies, ever.**

## Timing

- **During design phase:** workspace docs are private. `core/` is not yet scaffolded.
- **During bootstrapping (weeks 1-8):** `core/` repo is **private**. Premature OSS visibility produces an abandoned-looking repo before there's anything to use.
- **Once a working alpha exists:** `core/` repo flipped to **public, MIT-licensed**, with `license-checker` already enforced in CI.
- **Hosted platform / marketplace repos:** **always private.**

## Consequences

**Pros:**
- Adoption pattern validated by every comparable successful project (Supabase, n8n, Sentry, Plausible)
- Competes with OSS Hermes on equal license footing
- Builds trust for an infrastructure layer handling user data
- Clean legal story for RankLabs embedding LOOP in its own infrastructure
- Permissive license attracts contributions (bug reports, plugins, integrations)
- Hosted-SaaS revenue path is clearly defensible without compromising the OSS commitment

**Cons:**
- Anyone can fork LOOP's OSS core and run their own hosted version (AWS / Azure risk)
- "Free" sets the price expectation for everything built on top — paid tier must deliver clear additional value
- Mitigation if a cloud provider does the AWS-Elastic hostile-wrap thing: relicense future versions to BSL or AGPL (MongoDB / Elastic / HashiCorp all did this — political cost, but survivable)

## Alternatives considered

- **AGPL or BSL at launch:** rejected — signals distrust, depresses adoption. Hostile-wrap risk doesn't justify it at LOOP's scale yet.
- **Source-available (Confluent / Elastic license):** rejected — not real OSS, loses adoption benefits without enough commercial protection at beta scale.
- **Closed source only:** rejected — competes with OSS Hermes on Hermes's strongest ground; infrastructure products without OSS adoption struggle.
- **Apache-2.0 over MIT:** considered. Apache adds an explicit patent grant which matters for some enterprise users. **Acceptable substitute** — could swap to Apache-2.0 if a specific enterprise asks. MIT picked for default because it's shorter, more familiar, and matches Hermes.

## Related

- [ADR-0003](0003-two-tier-free-self-hosted-paid-saas.md) — two-tier product structure that this licensing strategy supports
- [COMPETITIVE.md](../COMPETITIVE.md) — Hermes comparison (MIT-licensed OSS competitor)
