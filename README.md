# loop-engine

**The cognitive-memory substrate for AI agents — with a wedge-gated promotion check that beats self-grading.**

`loop-engine` is a Rust library that sits between AI agent capture systems
(Anthropic Dreaming, Claude Code Auto Memory, learnings.md kits) and the
permanent-knowledge store. Capture systems propose; the engine decides what
graduates — under an explicit anti-self-grading promotion gate, structured
manifest assembly, scoped memory store with semantic search, lesson
lifecycle transitions, and per-agent skills + personas + teams.

```
[ Auto Memory / Dreaming / instincts / capture kits ]   ←  candidate lessons
                          ↓
                  [ loop-engine ]                       ← this layer
                          ↓
        ┌─ Promoted lessons (gate-passed, audit-trailed)
        ├─ Manifest sections surfaced to host LLM
        └─ Discarded / superseded (with reason + decrement)
```

---

## The wedge claim

Every promotion path through `loop-engine` runs an **anti-self-grading
gate**. A lesson cannot graduate to `promoted` based on the originating
agent's own thumbs-up — it must carry **external evidence** (structured
causal narrative + observed-or-inferred confidence + ground-truthed
citations to typed `EvidenceRef::Memory(_)` entries). User authorship is
load-bearing throughout: user-authored lessons are eviction-immune from
engine-initiated cleanup, and the memories they cite inherit that immunity
via a tracked counter.

This is the same anti-self-grading discipline that published research
(Reflexion-derived structured narrative + Voyager-derived external
verification) gets right — applied locally, MIT-licensed, composable with
whatever capture mechanism you already have.

### The 4-layer ratchet

| Layer | Promise | Source | Defense |
|-------|---------|--------|---------|
| **B: Promotion gate** | No self-graded promotions | `src/engine/lessons/gate.rs` | `gate::tests::*` (26 tests covering tampered age, missing narrative, thumbs-down block, time-floor) |
| **E2: Memory compression chain** | Citations survive compression — user-cited memories stay reachable through `derived_from` chains | `src/engine/memory/compress.rs` | `tests/compression_wedge_e2e.rs` (7 cross-cutting tests) |
| **F: Skill / persona / team immunity** | User-authored skills citing memories make those memories immune to engine-initiated delete | `src/engine/skills/store.rs` | `tests/skill_wedge_e2e.rs` (3 tests, incl. LLM-authored negative control) |
| **G: Lesson lifecycle decrement** | Retiring a user-authored lesson releases its slice of memory immunity (symmetric to step F) | `src/engine/lessons/transitions.rs` | `transitions::tests::discard_user_authored_with_force_decrements_memory_citations` + `supersede_user_authored_with_force_decrements_memory_citations` |

The 4 layers form a closed ratchet: claims add immunity at the user-
authorship boundary; retirements release it. Nothing in the middle
self-grades.

---

## Quick start

```rust
use loop_engine::engine::{
    assemble, AssembleConfig, Context, Manifest, SessionState,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ctx = Context::single_user_local();
    // `storage` is anything implementing `Storage` — `MemoryStorage`
    // for tests, `LocalFsStorage` for the filesystem backend.
    let storage = loop_engine::engine::storage::MemoryStorage::default();
    let config = AssembleConfig::default();
    let session = SessionState::empty();
    let manifest: Manifest = assemble(
        &ctx,
        &storage,
        None,                       // embedder (memory search optional)
        None,                       // vector index (memory search optional)
        &config,
        Some(&session),
        chrono::Utc::now(),
    ).await?;
    println!("active lessons: {}", manifest.active_lessons.len());
    Ok(())
}
```

Add to `Cargo.toml`:
```toml
loop-engine = "1.0"
```

For test-only fixtures (`MockLlmClient`, `MockEmbedder`, `TestHarness`):
```toml
loop-engine = { version = "1.0", features = ["test-fixtures"] }
```

---

## Architecture

```
src/engine/
├── context.rs        # Multi-tenant Context + UserId/TeamId/SessionId
├── storage/          # Storage trait + MemoryStorage + LocalFsStorage + CAS
├── embedding/        # Embedder trait + MockEmbedder
├── llm/              # LlmClient trait + Generation + LlmError + Mock
├── vector/           # VectorIndex trait + HnswVectorIndex
├── lessons/          # Loader, gate, narrative gen, signals, transitions
│   ├── gate.rs       # Promotion gate — the wedge
│   ├── narrative.rs  # Causal-narrative generation
│   ├── signals.rs    # external_signal_sources + applied_count
│   └── transitions.rs# promote / discard / supersede / capture_feedback
├── memory/           # Frontmatter + vec sidecars + compression + scope
│   ├── store.rs      # CRUD + decrement_citation_count
│   └── compress.rs   # Citation-chain-preserving compression
├── skills/           # Claude-Skills hooks model + immunity
├── personas/         # Identity descriptors + immunity
├── teams/            # Groupings + typed TeamMember
├── manifest/         # Manifest::assemble — the host LLM payload
└── sentiment/        # Pretrigger + classifier + attribution
```

The engine returns `Manifest` (engine-stable) but never serializes a wire
format — adapter crates (the future MCP server / monolith) own the wire
shape via `From<&Manifest>`.

---

## Stability

| Surface | Stable in 1.0 | Notes |
|---------|---------------|-------|
| Public types (`Manifest`, `Memory`, `Skill`, `Persona`, `Team`, `LoadedLesson`) | Yes | All `#[non_exhaustive]` for SemVer-additive growth |
| Storage / Embedder / LlmClient / VectorIndex / SentimentClassifier traits | Yes (sealed) | Trait sealing keeps implementation in-crate. Workspace pattern: external impls land via adapter modules. |
| YAML frontmatter shapes (`LessonFrontmatter`, `MemoryFrontmatter`, etc) | Yes | Additive growth via `#[serde(default)]` — legacy files always parse |
| `EngineError` variants | Yes | `#[non_exhaustive]` |
| **Body audit-line format** (`<!-- discard reason: ... -->`, `<!-- feedback: ... -->`) | Unstable | May graduate to typed events in v1.1 |
| **Skill `lesson-history.yaml` sidecar** | Unstable | Append-only text format; may become typed in v1.1 |
| Multi-tenant `Context` shape | Yes | Single-user + multi-tenant constructors both stable |

---

## Testing

```bash
cargo test --lib              # 534 unit tests
cargo test --tests            # + 17 integration tests
cargo clippy --all-targets -- -D warnings
```

Wedge-defense tests specifically:

```bash
cargo test --test compression_wedge_e2e
cargo test --test skill_wedge_e2e
cargo test --lib transitions::tests::discard_user_authored_with_force_decrements_memory_citations
cargo test --lib transitions::tests::supersede_user_authored_with_force_decrements_memory_citations
```

Public-API stability gate (see [CONTRIBUTING.md](./CONTRIBUTING.md)):

```bash
cargo install --locked cargo-public-api
cargo public-api diff      # CI fails on accidental breakage
```

---

## License

MIT. See [LICENSE](./LICENSE).

Zero AGPL/GPL/SSPL dependencies — verified via `cargo tree --license`
and tracked in [THIRD_PARTY_LICENSES.md](./THIRD_PARTY_LICENSES.md).

---

## Project name

`loop-engine` is the substrate; the product brand built on top is
**MindCraftor**. The engine ships standalone (this crate); adapter
binaries (MCP server, monolith host) live in separate repos.
