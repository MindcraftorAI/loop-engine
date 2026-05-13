# ECC Source Code — Patterns + Anti-Patterns (2026-05-12)

Distilled from deep-source research on `affaan-m/everything-claude-code` v2.0.0-rc.1. 180K stars, MIT, but effectively single-maintainer (0 of 7 community PRs merged).

## CRITICAL findings that change Loop's plan

### 1. Path migration — `~/.claude/` is blocked

Claude Code has a "sensitive path guard" that blocks background writes to `~/.claude/`. ECC works around it by writing to `${XDG_DATA_HOME:-~/.local/share}/ecc-homunculus/`. **Loop will hit the same wall.** Migrate the default lesson write path from `~/.claude/lessons/` to `${XDG_DATA_HOME:-~/.local/share}/loop-engine/` before users see the failure.

### 2. PR-to-affaan-m fallback is unlikely to land

Evidence: 0 of 7 community PRs merged. Even trivial doc fixes closed without merge. ECC is functionally a single-maintainer project despite the star count. **Pivot the fallback:** ship Loop as a **separate ECC plugin** (same model AgentShield uses), not as a PR. Target `/evolve` flow via the public CLI surface.

### 3. AgentShield is a separate repo, not internal to ECC

Earlier research treated it as ECC's internal security subsystem. It's `affaan-m/agentshield` — 626 stars, separate MIT TypeScript project consumed as a plugin. **Use this as the plugin model.** Mention AgentShield as a companion in Loop docs; don't duplicate scope.

### 4. ECC's `/evolve` has NO anti-self-grading guard — confirmed wedge

`/evolve` schema (was undocumented before this research):
- Skill candidate = cluster of ≥2 instincts on normalized trigger text
- Command candidate = workflow instinct with confidence ≥0.70
- Agent candidate = cluster of ≥3 instincts AND avg confidence ≥0.75

`/evolve --generate` writes files with **no human gate**. The only quality check anywhere is `/learn-eval` — a manual LLM self-grade against 4 buckets (Save/Improve/Absorb/Drop). That's the bar Loop's verifier must clear.

## Patterns to LIFT (MIT, with attribution)

| Pattern | Where in ECC | Loop application |
|---|---|---|
| Project-scope hash via `git remote get-url origin` → 12 chars | `scripts/detect-project.sh` | Same repo on different machines maps to same ID; lesson portability |
| Multi-doc YAML with `---` as frontmatter-only boundary | `instinct-cli.py::parse_instinct_file` | Body horizontal rules use `***` / `___`; sidesteps YAML lib edge cases |
| Confidence-as-float with default-on-malformed | same | Numeric field parse errors → 0.5 default, don't reject |
| TTL prune on pending items: 30-day delete, 7-day warning | `instinct-cli.py::prune` | Adopt for `pending` and `active` lessons; prevents accumulation |
| Hook re-entrancy guard: 60s cooldown, tail-sampling, 1800s idle-exit, SIGUSR1 trigger | `agents/observer-loop.sh` | Required if Loop ever runs an in-process observer (sentiment subagent counts) |
| Secret-redaction regex: `(?i)(api.?key\|token\|secret\|password).*([A-Za-z0-9_\-/.+=]{8,})` | `hooks/observe.sh` | Apply before any disk write in memory + sentiment audit layers |
| Capture vs analysis separation | observe.sh writes JSONL synchronously; observer-loop.sh reads async | Same split for sentiment subagent: regex pretrigger sync, LLM async |
| Per-host shim directories (`.claude/`, `.cursor/`, `.codex/`...) | repo layout | Host-agnostic core + per-host adapter dirs |

## Anti-Patterns to AVOID

1. **Confidence set by same LLM that captured the pattern** (`agents/observer.md`). The wedge.
2. **No evidence verification** — "Observed 5 instances" is free-text, nothing checks the 5 instances exist
3. **Promotion threshold = count + self-graded confidence** — a noisy detector firing same false positive in 2 projects auto-promotes globally
4. **Mechanical confidence drift** (+0.05 per LLM-self-confirmation, -0.1 per LLM-self-contradiction, -0.02 weekly) — no orthogonal evaluator
5. **Project-scope fallthrough to global** when CLAUDE_PROJECT_DIR unset + no .git
6. **Body content not validated against frontmatter `trigger`** — drift undetected

## Hook integration shape (Claude Code)

```ts
interface HookInput {
  tool_name: string;
  tool_input: { command?; file_path?; old_string?; new_string?; content? };
  tool_output?: { output? };  // PostToolUse only
}
// Exit codes: 0 success, 2 block (PreToolUse only), other non-zero = error
// PostToolUse cannot block
```

Top-level keys in `hooks.json`: `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PreCompact`, `SessionStart`, `Stop`, `SessionEnd`.

Synchronous capture (fast, no LLM, just I/O) + async analysis. Loop's sentiment subagent must follow this split.

## Market opening surfaced by the research

ECC's observer is `observer.enabled: false` by default. Hooks fire (capture works), but the analysis loop most users never enable. **ECC is shipping a learning system most users aren't running.** Loop's verified, on-by-default pipeline is a real opening.

## Files to read for deep dive

Priority order:
1. `skills/continuous-learning-v2/SKILL.md` (13KB)
2. `skills/continuous-learning-v2/scripts/instinct-cli.py` (60KB) — the actual law
3. `skills/continuous-learning-v2/hooks/observe.sh` (18KB)
4. `skills/continuous-learning-v2/agents/observer.md` + `agents/observer-loop.sh`
5. `commands/evolve.md` (4.5KB) — critical, short
6. `hooks/hooks.json` (50KB) + `hooks/README.md`
7. `.cursor/hooks.json` + `.codex/config.toml` — cross-host adapter ends
8. `commands/learn-eval.md` — the LLM-self-grade pattern Loop must beat
