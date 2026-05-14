# Contributing to loop-engine

Thanks for your interest. `loop-engine` is the substrate layer for an AI
agent memory system; the public API is committed to SemVer at 1.0.

## Development

Requirements:
- Rust 1.85+ (see `Cargo.toml`)
- `cargo`, `cargo clippy`, `rustfmt` (all via rustup)

Build + test:

```bash
cargo build
cargo test --lib                                  # 534 unit tests
cargo test --tests                                # + 17 integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Workflow discipline

Each feature/fix follows a **6-phase cycle**:

1. **Pre-research** — survey existing code + TS-parity (where relevant)
   + open questions. Output: `docs/research/<topic>-pre-research.md`.
2. **Learn-notes** — lock decisions for every open question. Output:
   `docs/research/<topic>-learn-notes.md`.
3. **Build** — implement against the locked decisions.
4. **Post-research** — document what was learned, trade-offs accepted,
   forward-feeds. Output: `docs/research/<topic>-post-research.md`.
5. **Audit** — independent NO-GO/GO review (spawn an audit agent).
   Output: `docs/research/<topic>-audit-report.md`.
6. **Audit-fix close** — apply findings, commit.

Skipping a phase is workflow drift. The wedge invariant has been
preserved across 8 phases via this discipline.

## Public API stability

`loop-engine` 1.0 commits to SemVer. The public API gate is enforced
by `cargo public-api`:

```bash
cargo install --locked cargo-public-api
cargo public-api > public-api-current.txt
diff public-api-v1.0.txt public-api-current.txt   # must be empty for patches
cargo public-api diff                              # for releases
```

CI runs this on every PR. Any accidental breakage fails the build.

### What counts as a breaking change

- Removing a public item (function, type, trait, variant, field)
- Changing a public function signature (incl. parameter types, return type)
- Adding a required parameter
- Adding a required associated type or method to a public trait
- Narrowing a generic bound
- Removing a `#[non_exhaustive]` marker

### What's additive (safe)

- Adding a new public item
- Adding a new variant to a `#[non_exhaustive]` enum
- Adding a new field to a `#[non_exhaustive]` struct
- Adding a new `#[serde(default)]` field to a YAML-frontmatter struct
- Adding a new method to a sealed trait

## Releasing (maintainers)

Pre-tag checklist:
1. `cargo test --tests && cargo clippy --all-targets -- -D warnings`
2. `cargo public-api > public-api-v<x.y>.txt` and commit
3. Update `CHANGELOG.md` (move Unreleased to v<x.y>, date stamp)
4. Bump `version =` in `Cargo.toml`
5. `git tag v<x.y>.0` + push

crates.io publishing:
- Currently `publish = false` (D-H3, see `phase-h-learn-notes.md`).
- Flip to `publish = true` when the standalone repo split lands.
- Then: `cargo publish` from the standalone repo (NOT from inside the
  monorepo).

## File-size discipline

Hard cap: **500 prod LOC per file** (tests don't count). Files
approaching the cap should split. Examples of completed splits:
- `manifest/mod.rs` → `manifest/{internal,session}.rs` (Phase F audit)
- `memory/store.rs` → `memory/{store,lifecycle,compress}.rs` (Phase E2)

## Wedge invariant — DO NOT BREAK

The 4-layer ratchet (promotion gate → compression chain → skill
immunity → lesson-lifecycle decrement) is the load-bearing claim. Any
change that lets a user-authored memory's `consumed_by_user_lessons`
go stale, OR lets a self-graded lesson promote, OR lets an engine-
initiated path delete a user-authored entry without `force=true`, is
a wedge break. PRs touching these paths must add or update a
cross-cutting wedge test (see `tests/{compression,skill}_wedge_e2e.rs`).

## Code style

- No emojis in code/docs unless explicitly requested.
- Comments: WHY only (the WHAT is obvious from well-named code).
  No "// removed X" comments. No "// TODO" without a phase/issue tag.
- Async: never `.await` while holding state across iterations of a
  CAS loop. Re-read on every iteration.
- Errors: `EngineError` for crate-public APIs; `anyhow` only in
  legacy sync wrappers being phased out.

## License

By contributing you agree your contributions are licensed under MIT.
