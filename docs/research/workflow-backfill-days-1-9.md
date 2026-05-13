# Workflow backfill — Days 1-9 (TS batch verifier build)

**Backfilled 2026-05-13** per `feedback_dont_skip_audit_cycle` and the
user's call-out that the locked discipline was not followed
consistently during Days 1-9 of the verification-layer build.

Each day's section captures the workflow artifacts that should have
been produced at the time. The pre-research artifacts that DID exist
are referenced; only the missing pieces are written here.

The cycle is: pre-research → learn → build → post-research → audit →
commit. Post-research is DISTINCT from audit (audit = "is THIS
correct?"; post-research = "what did we LEARN that should inform the
next day?").

---

## Day 1 — Shared ingest pipeline (commit 1fb0b79)

### Pre-research (existed)
Four parallel agents fired before any code:
- `sentiment-linguistics-2026-05-12.md` — CA + Pragmatics + Appraisal taxonomy
- `sentiment-engineering-2026-05-12.md` — Haiku 4.5, calibration, attribution
- `sentiment-design-rules.md` — synthesized 20 hard rules
- `ecc-source-patterns-2026-05-12.md` — initial ECC reference (later superseded by Day 39 deep dive)

### Learn (existed)
`sentiment-design-rules.md` is the learn artifact for the broader sentiment work AND for Day 1's `CandidateLesson` shape decision.

### Post-research (missing — backfill below)

**What we learned from building Day 1:**

1. The `external_signal_sources` field in `LessonFrontmatter` (a `string[]`) can carry strings beyond just signal names — we use `auto_memory_curated`, `user_thumbs_up`, `sentiment_positive`, etc. The Set semantics (via the loader's `Array.from(new Set(...))`) are what makes this clean.

2. The `CaptureLessonInput` already exists for direct user captures (via `loop_capture_lesson` MCP tool); the ingest adapters reuse this same path with extra fields. No need for a separate "ingest API" — extending `CaptureLessonInput` with optional `external_signal_sources` is enough.

3. The `causal_narrative` field has a confidence ladder (observed/inferred/speculative) where `observed` requires `evidence_refs`. This is enforceable at ingest time (fast-fail) AND at the gate (defense in depth) — both layers added.

4. The audit caught the **silent shared `evidence_refs` array** bug (A1 — shared by reference between candidate and frontmatter). The defensive copy `[...ni.evidence_refs]` is the fix and applies anywhere we hand external array references to frontmatter.

**What this implies for Day 2 pre-research:**
- The Auto Memory frontmatter shape needs explicit verification (the public docs are wrong about it — see Day 30 ingestion research).
- `originSessionId` is provenance, not signal — strip from frontmatter, log to source_metadata.
- Auto Memory has NO confidence field → must default; default should be `inferred` if originSessionId present, else `speculative`.

---

## Day 2 — Auto Memory adapter (commit 00fa54c, audit-fix 54f941d)

### Pre-research (existed)
The Auto Memory ingestion research agent fired (task #30 / agentId archived; deliverable summarized the actual on-disk frontmatter shape).

### Learn (missing — backfill below)

**Design decisions made for Day 2:**

- **`ingest_provenance` as a persisted frontmatter field** carrying `{source_type, source_path, source_external_id?, extracted_at}`. Required for dedup on re-ingest (must NOT re-create a lesson the user already has).
- **Dedup key = (source_type, source_external_id)** — initially. Later corrected to (source_type, source_external_id, source_path) by audit A2 (cross-project collision: two projects with same filename would silently collide).
- **MEMORY.md presence as a curation signal.** A file appearing in the user's MEMORY.md index gets `external_signal_sources: ['auto_memory_curated']` on import; files absent from the index start with empty signals.
- **TOCTOU guard via re-stat after parse** — Auto Dream consolidation can rewrite memory files mid-read; if mtime changed between initial stat and final stat, skip with a "retry later" log.
- **Confidence default ladder** — `inferred` if `metadata.originSessionId` present (model was involved in capture), else `speculative`. evidence_refs stay empty (we'd be inventing if we extracted them from prose).

### Post-research (missing — backfill below)

**What we learned building Day 2:**

1. **Public Auto Memory docs misdescribe the frontmatter shape.** They claim "plain markdown, no frontmatter." Reality (verified across 6 real files): YAML frontmatter with `name`, `description`, and a NESTED `metadata:` block carrying `node_type`, `type`, `originSessionId`. Documented for future ingest work.

2. **Cross-project dedup collision risk is real** (audit A2 caught it). Source IDs like `feedback-typecheck` are NOT globally unique — they're per-project. Path-scoping the dedup key is mandatory.

3. **`MEMORY.md` is a hand-curated bullet-list index, not structured data.** Reliable extraction = `[label](filename.md)` regex match. Treat label as opaque.

4. **`originSessionId` is provenance not signal.** Don't promote it to frontmatter; debug-log it for offline audit.

**Day 3 pre-research implications:**
- `loop verify` CLI is essentially the Day 2 adapter in dry-run mode. Same parsing logic, no writes. Don't re-research the format.
- The gate's blocker output (from `evaluatePromotionGate`) needs structured per-blocker text so the CLI can pretty-print specific reasons. Verify the gate already emits structured strings.

---

## Day 3 — `loop verify` CLI (commit 62c8bdc, audit-fix a861db0)

### Pre-research (missing — backfill below)

**Question:** Build a read-only CLI that runs the gate against any markdown file or directory of files (learnings.md, MEMORY.md, ad-hoc lessons) without writing.

**What we needed to research:**
- The gate (`gate.ts`) already operates on `LessonFrontmatter`, so the question is how to *construct* a hypothetical `LessonFrontmatter` from arbitrary markdown.
- For files that look like Auto Memory entries, the Day 2 parser already handles the shape. Reuse.
- For arbitrary markdown that doesn't have full Loop frontmatter, we need to fill in plausible defaults: applied_count=0, age=0, no thumbs_down, no signals (unless MEMORY.md curation present).
- Exit code semantics: CI tooling expects 0 for "pass" / non-0 for "fail." But all fresh lessons fail on `time_floor` + `insufficient_volume` — should those count as failures? Answer (decided post-audit): NO, distinguish wedge blockers (anti-self-grading) from ripening blockers (volume/age).

### Learn (missing — backfill below)

**Design decisions:**

- **Two-layer gate evaluation in verify output.**
  - `wedge_blocked` = lesson has `no_external_signal`, `speculative_narrative`, `missing_causal_narrative`, `observed_without_evidence`, or `hard_block` (thumbs_down). These are STRUCTURAL problems with the entry.
  - `ripening_blocked` = lesson has `insufficient_volume` or `time_floor`. These clear naturally with usage + time.
  - `--strict` flag treats both as failures (old behavior); default treats only wedge as failure (CI-friendly).
- **`--json` flag** for CI consumption. Always emit `blockers` and `reasons` as arrays (even when empty) so JSON consumers don't have to defend against undefined.
- **Directory-mode warnings** for "no MEMORY.md found" — explains why all entries would block on `no_external_signal`.

### Post-research (missing — backfill below)

**What we learned:**

1. **Exit codes are user-experience.** Initial implementation exited 1 on any blocker → made the CLI useless in CI (every fresh ingest fails immediately on time_floor). The wedge-vs-ripening classification is the load-bearing UX call.

2. **The `blockers: string[]` shape (from `gate.ts`) is stable enough to parse-by-prefix** for the wedge classification. `WEDGE_BLOCKER_PREFIXES` is a small constant set.

3. **Always-present arrays for JSON.** Optional fields are footguns for downstream consumers; defaulting to empty arrays is cheap.

**Day 4 pre-research implications:**
- ECC instincts have a completely different shape (multi-doc YAML, `confidence` self-graded as float). The Auto Memory + verify infrastructure mostly reuses, but the source-specific parser is new.

---

## Day 4 — ECC instincts adapter (commit 70d0613, audit-fix 9035f44)

### Pre-research (existed)
Day 39 ECC source dive task (`ecc-source-patterns-2026-05-12.md`) covered the instinct format, multi-doc YAML pattern, `/evolve` mechanism, anti-self-grading absence.

### Learn (missing — backfill below)

**Design decisions:**

- **Hard-cap at `speculative`** for ALL ECC instincts regardless of self-declared confidence (0.3-0.9 float). ECC's confidence is pure self-grading with no evidence; Loop's wedge ignores it entirely.
- **No external_signals on import.** The gate blocks all ECC instincts until apply count + sentiment accumulate through real usage.
- **Dedup by (source_type, instinct_id, source_path).** ECC IDs are repo-unique by construction, but ECC has global vs project scopes — same ID can legitimately appear in both.
- **Multi-instinct-per-file parser** — ECC files contain multiple instincts separated by `---` on its own line. Skip empty blocks. Each instinct that lacks an `id` field is skipped with a warning (audit A1 mitigation: catches body-line-`---` corruption).
- **Recursive directory walk with symlink loop guard** (audit A3) — use `lstat` to detect symlinks + skip them, track visited real paths to break cycles.

### Post-research (missing — backfill below)

**What we learned:**

1. **ECC's multi-doc YAML convention is fragile.** `---` is a hard frontmatter boundary; body horizontal rules MUST use `***` or `___`. Files violating this convention silently corrupt — our warning approach (parsed-but-no-id) is the best we can do without a full YAML state machine.

2. **`/evolve` is undocumented in SKILL.md** — has to be reverse-engineered from `lib/`. We deferred ingesting `/evolve`-produced skills; only raw instincts handled. Worth a future follow-up if Phase B shows ECC integration matters.

3. **Symlink loops are a real adversarial-input vector** — even on benign installs, a misconfigured symlink could hang the daemon. The `lstat` + visited-set fix is the standard mitigation.

**Day 5 pre-research implications:**
- Auto Dream JSONL is FILE-based like Auto Memory + ECC, but the content shape is wildly different — append-only event log, not curated documents.
- Interrupts (the strongest auto-signal) may not be a literal `interrupted: true` flag on this user's transcripts; need empirical check.

---

## Day 5 — Auto Dream JSONL adapter (commit db2e051, audit-fix 191638c)

### Pre-research (existed)
The broader sentiment research covered linguistic markers + correction triggers; Day 5 reused that knowledge.

### Learn (missing — backfill below)

**Design decisions:**

- **Two signal kinds:** (1) explicit `toolUseResult.interrupted=true` or tool_result with `is_error=true` + `interrupt|abort|cancel` in content; (2) text correction — short user message (≤240 chars) matching CA-derived trigger regex.
- **Conservative regex with smart-quote support** (audit A1): `Don'?t` / `Don’t` both match. iOS/macOS auto-smart-quote would otherwise silently miss most corrections.
- **Drop "actually" from triggers** (audit A2): polysemous — affirmation just as often as correction. Caught a real false positive.
- **`isMeta: true` events skipped** (audit B3): Claude Code injects `<local-command-caveat>` and similar system-side text masquerading as user input.
- **Lockfile coordination with Auto Dream:** `~/.claude/tasks/<session-uuid>/.lock` — skip if fresh (<10 min by default), ignore if stale.

### Post-research (missing — backfill below)

**What we learned:**

1. **Empirically, `interrupted: true` rarely fires on real transcripts.** Audited Day 5's lessons against the user's actual JSONL files — zero hits for either `"interrupted":true` or tool_result content matching `/interrupt|abort|cancel/`. Path 1 + 2 of detection are effectively dead code today. Path 3 (text-correction regex) does all the work. Worth re-checking in 6 months as Claude Code evolves.

2. **`isMeta` filter is critical.** Without it, system-injected `<local-command-caveat>` messages that contain words like "no" trigger as user corrections.

3. **Smart-quote apostrophes are not a niche concern.** They're the iOS/macOS default. Always normalize.

**Day 6 pre-research implications:**
- Solicitor is read-only (pure query over the lesson set), no new adapter concerns.
- The "active lessons that need user feedback" question requires defining what counts as a "real" external signal vs a "passive" one — design call.

---

## Day 6 — Solicitor (commit 7bd4c19, audit-fix ade4b0d)

### Pre-research (missing — backfill below)

**Question:** Find lessons that should be actively surfaced for user-asking — the ones with applied_count > 0 but no real verification signal.

**What we needed to research:**
- What makes a signal "real" vs "passive"? Initial cut: `auto_memory_curated`, `ecc_instinct`, `auto_memory_origin_session` are passive (capture-time markers); `user_thumbs_up`, `sentiment_positive`, `user_interrupt` are active (verification signals).
- Should we use a denylist (passive set) or allowlist (active set)? Denylist fails open on unknown sources (a typo in a future adapter silently exits the solicitation funnel). Allowlist fails safe (unknown sources stay in the funnel until classified). Audit A1 caught this — switched to allowlist.

### Learn (missing — backfill below)

**Design decisions:**

- **ACTIVE_SIGNAL_SOURCES allowlist** (audit A1): `user_thumbs_up`, `sentiment_positive`, `user_interrupt`. Any lesson with any of these is excluded from solicitation.
- **Composite ranking score:** `log2(applied_count + 1) * 3 + log10(age_hours) + no_signal_bonus`. Log-scaled apply count avoids capping at 10 (the original Math.min(applied, 10) * 2 flattened high-usage lessons; audit A2 caught it).
- **Forced-choice question template** ("keep / scale back / drop?") instead of yes/no ("did it help?"). Per `sentiment-design-rules.md` — yes/no biases toward agreement; forced-choice with concrete alternatives produces honest signal.
- **Rate limit at caller (orchestrator), not module.** This module is a pure query — returns a ranked list. Callers pick ONE per session per the design rule of ≤1 solicitation per ~20 turns.

### Post-research (missing — backfill below)

**What we learned:**

1. **Allowlist vs denylist on signal classification is a recurring shape.** Anywhere we classify signal sources, default to allowlist with explicit additions. Fail-safe drift.

2. **Question template phrasing is a product surface, not pure logic.** The template lives in code today but should be configurable (different deployments may have different tone). Deferred to Phase B.

3. **Ranking is heuristic.** No way to A/B test without users. The log2/log10 mix is defensible but unproven; revisit after Phase B dogfood.

**Days 7-9 pre-research implications:**
- Sentiment subagent is the largest single piece. Pre-research already exists (linguistics + engineering + psychology agents from Day 1).
- Key gap: per-session rate limiting was DOCUMENTED in design rules but never IMPLEMENTED on the TS side. Caught by Days 7-9 audit A4.

---

## Days 7-9 — Sentiment subagent (commit 745479d, audit-fix c828a21)

### Pre-research (existed)
The three agents from Day 0 (linguistics, engineering, psychology) + `sentiment-design-rules.md` synthesizing them.

### Learn (existed)
`sentiment-design-rules.md` is the comprehensive learn artifact.

### Post-research (missing — backfill below)

**What we learned building the sentiment subagent:**

1. **Attribution-abstain is the load-bearing safety check** (audit A2 caught the bug here). The 5-pass attribution algorithm can return null when no path resolves. The orchestrator MUST skip the signal in that case — even if the classifier confidently picked a target. Initial code used a ternary that emitted with method='salience' regardless; that defeated the whole point of structural attribution.

2. **Hazards must auto-abstain** (audit A3). If the classifier flags `sarcasm_suspected` or `ambiguous_referent`, the signal is unreliable by definition. Emitting it with a hazard tag the gate ignores is worse than abstaining.

3. **Pretrigger regex is the cost gate.** ~92% of user turns carry no sentiment signal. Without the regex pre-filter, we'd burn LLM tokens on every turn. The regex needs to cover negative contractions (`doesn't`, `won't`, `isn't`) which we initially missed — audit A1.

4. **Calibration is deferred but architecturally provisioned.** `calibratedConfidence` field exists in `SentimentSignal`; today it equals `rawConfidence`. Future Phase B work will add the 3-layer calibration (consistency-over-samples → empirical recalibration table → reject-option overlay).

5. **Per-session rate limiting was DOCUMENTED but not IMPLEMENTED** on the TS side (audit A4). The orchestrator's `seenItems` Set dedups within a single classify call, not across calls. The fix belongs in the Rust daemon (Day 16) where in-memory state survives the call boundary.

6. **MCP-surface stays shadow-mode forever.** The MCP `loop_classify_sentiment` tool uses a noop classifier + `emit=false`. The production classifier is injected by the host process (the Rust daemon), NOT the MCP server. This preserves the anti-self-grading invariant — an autonomous LLM that can call MCP tools cannot self-grade its own lessons.

**Day 10 pre-research implications:**
- The daemon's architecture is foundational to making sentiment actually work in practice. MCP server alone can never run sentiment continuously.
- ECC's `ecc2/` Rust daemon is MIT and ~70% of what we need. Cherry-pick.

---

## Lessons applied to the daemon build (Days 10+)

The pattern that emerges from these backfills:

1. **Audit catches what pre-research should have caught.** Days 4-9 had no separate pre-research per module, and audits surfaced 13 critical findings retroactively. Days 10-12 had pre-research only intermittently, with similar results. Day 13 onward: pre-research agent fires for every non-trivial module.

2. **Post-research is the missing forward-feedback step.** Without it, the next day's pre-research is uninformed by what we just learned. Every backfilled post-research note above ends with "implications for next day's pre-research" — that's the feed-forward function the cycle exists to enable.

3. **Learn notes are cheap and load-bearing.** Each one above is ~200-400 words. Writing them at the time prevents the "we made this decision but I forgot why" problem 3 weeks later.
