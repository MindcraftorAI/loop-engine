# Integrating loop-engine

> Per [Phase H D-H2](../CHANGELOG.md) — sealed for v1.0; revisit in v1.1
> if external implementors emerge.

`loop-engine` exposes 5 sealed traits that the engine consumes
abstractly: `Storage`, `Embedder`, `LlmClient`, `VectorIndex`,
`SentimentClassifier`. They are **sealed** — `pub(crate)` marker module
keeps external crates from implementing them directly.

This is deliberate. v1.0 commits to API stability under SemVer, and an
unsealed trait surface is a much larger compatibility promise than a
sealed one. We don't yet know what shape downstream implementors need.

## The workspace pattern

If you want to plug in your own backend (e.g., a different LLM
provider, a different vector index, a custom storage layer), the
integration path for v1.0 is:

1. Fork `loop-engine` OR add it as a path dependency in a workspace.
2. Add a new module under `src/engine/<trait>/<your_provider>.rs`
   that implements the relevant trait.
3. Wire it into the public API surface of your fork/workspace.
4. Build your host adapter against that workspace.

This is the same pattern `loop-engine` itself uses for the bundled
implementations:
- `src/engine/llm/mock.rs` — `MockLlmClient` for tests
- `src/engine/llm/anthropic.rs` — Anthropic provider (if shipped)
- `src/engine/storage/{memory,filesystem}.rs` — in-memory + LocalFs
- `src/engine/vector/hnsw.rs` — HNSW backend

## Why sealed?

Three reasons:

1. **SemVer durability.** Adding a method to an unsealed trait is a
   breaking change. Adding a method to a sealed trait is additive.
2. **Implementation invariants.** Several methods rely on contract
   not expressible in the type system (e.g., `Storage::put_if_version`
   atomicity guarantees). Keeping impls in-crate lets us audit them.
3. **No premature extensibility.** We have a finite list of impls
   today. When that list grows, we'll revisit.

## When v1.1 might unseal

Trigger conditions:
- Two or more external implementors emerge for the same trait.
- A bundled impl proves too restrictive for a common use case.
- The host community has stabilized around a workspace structure that
  obviously benefits from unsealing.

Until then, fork-or-workspace is the path.

## Mock fixtures

For testing your code against `loop-engine`, the `test-fixtures`
feature exposes `MockLlmClient`, `MockEmbedder`, and `TestHarness`:

```toml
[dev-dependencies]
loop-engine = { version = "1.0", features = ["test-fixtures"] }
```

These are SEALED implementations of the relevant traits and don't
require unsealing the public API.
