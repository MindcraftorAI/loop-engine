# Day 11 learn notes — YAML reader/writer design decisions

**Backfilled 2026-05-13.** Synthesizes the design calls made between
pre-research and build. Should have been written before any code landed.

## Module split

Three modules in `src/yaml/`:
- `schema.rs` — typed `LessonFrontmatter` struct + nested types
  (`CausalNarrative`, `IngestProvenance`, enum variants). Field order
  matches TS load-path emit order. Serde-derived for the reader to use.
- `scalar.rs` — `scalar_style()` + `render_scalar()` + `double_quote()`
  + `literal_block()`. Pure logic, no I/O. Tested in isolation.
- `writer.rs` — owns field-emission order + per-section serialization.
  Uses `scalar.rs` for the per-value style decision.
- `reader.rs` — thin `serde_yml::from_str` wrapper for the parse path.
- `frontmatter.rs` — `---` envelope split/combine. Separate concern.

## Scalar style rules (matching TS)

- **Plain** preferred when YAML 1.2 plain-scalar rules allow.
- **Double-quoted** with named escapes when plain would round-trip
  to a different value (numeric-looking, YAML keyword, leading reserved
  char, contains `: ` or ` #`).
- **Literal block `|-`** when value contains `\n` AND no embedded control
  chars (other than `\t`/`\n`). Chomp-strip (`|-`) for values without
  trailing newline; preserve (`|`) for values ending in `\n`.

## Field order

Matches TS `tryLoadLessonFile` load-path order (NOT capture-path order).
After any read-modify-write on the TS side, lessons stabilize in this
order. The daemon's first signal write may "scramble" a freshly-captured
lesson into this order — acceptable, idempotent thereafter.

## Known compatibility quirk: combine_frontmatter newline drift

TS's `renderLessonFile` produces `---\n{yaml}\n---\n\n{body}`. The
loader's regex captures body INCLUDING the post-delimiter `\n`. So
load-modify-save accumulates one `\n` per cycle in the body's leading
whitespace.

Mirror the behavior for compatibility. Day 12 callers MUST normalize
body whitespace (`trim_start_matches('\n')`) before passing to
`combine_frontmatter` to prevent unbounded growth.

## What we deferred

- Anchor/alias support: Loop never emits these, parser handles via
  `serde_yml` if encountered, writer doesn't emit. Not a v1 concern.
- Comment preservation: Loop frontmatter never has comments. Drop on parse.
- Flow-style sequences (`[a, b, c]` on one line): only emitted for empty
  arrays; non-empty arrays always block-style. Matches TS.
