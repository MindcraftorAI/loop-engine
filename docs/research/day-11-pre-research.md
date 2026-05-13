# Day 11 pre-research — purpose-built YAML reader/writer

**Backfilled 2026-05-13.** This artifact should have been produced
BEFORE Day 11's commit; it's reconstructed here from what informed
the build at the time.

## Question

Build a YAML reader + writer for Loop lesson frontmatter such that
the daemon and the TS MCP server can both read/write the same .md
files without format drift across cross-process mutations.

## What we already knew going in (Days 1-9 + ECC research)

- TS side uses `yaml@2.x` with pinned options
  `{blockQuote: 'literal', lineWidth: 0, defaultStringType: 'PLAIN',
  defaultKeyType: 'PLAIN'}`. Phase 2 audit A3 documented this — multi-
  paragraph values get refolded by defaults; pinning these options is
  what keeps round-trip stable.
- The TS lesson schema lives in `core/src/types/index.ts` — ~17 fields,
  some optional, two levels of nesting max (`causal_narrative`, `ingest_provenance`).
- Real lesson on disk at `~/.loop/lessons/active/les-dfs24ojt.md` —
  shows actual TS output. Used as ground truth.
- `serde_yml` is the actively-maintained fork of the deprecated
  `serde_yaml`. Adequate for parsing; round-trip control is weak.
- Rust YAML ecosystem state (2026-05): `serde_yaml` archived,
  `serde_yml` maintained but limited round-trip features, `yaml-rust2`
  bare. No off-the-shelf solution gives byte-stable round-trip with
  TS yaml output.

## Decisions made

- **Parser:** use `serde_yml` (deprecation concern is maintenance not
  correctness on shipped versions). Risk is concentrated in the writer.
- **Writer:** hand-rolled. Constrained to Loop's narrow shape — no anchors,
  no comments, no flow-style maps. 4 scalar styles supported: plain,
  double-quoted, literal-block-strip (`|-`), literal-block-keep (`|`).
- **Round-trip parity strategy:** match TS output byte-for-byte under
  the same inputs. Verify via fixture tests (hand-crafted expected YAML)
  and integration test against a real on-disk TS-written lesson.
- **Frontmatter split/combine** as separate concern from YAML serialization.
  The `---` envelope handling is delimiter logic, not YAML.

## Risks identified at this stage

- TS yaml's plain-vs-quoted decision heuristics weren't fully verified
  empirically — left as audit follow-up.
- Multi-line strings: TS uses literal blocks under `blockQuote: 'literal'`.
  Implementing this correctly was the hardest part of the writer.
- Field order: TS load-path order may differ from capture-path order.
  Worth verifying against `tryLoadLessonFile`.

## What we did NOT research and should have

- Empirical TS output for adversarial values (`.inf`, `0x10`, leading-`+`,
  `yes/no/on/off`). Caught by audit A3+A4 retroactively.
- Control-char escape table TS uses (named escapes like `\a`, `\v`).
  Caught by audit A5.

These gaps reflect that this artifact was inline-mental rather than
agent-driven. Future days should fire the pre-research agent to cover
the empirical-TS-output question explicitly.
