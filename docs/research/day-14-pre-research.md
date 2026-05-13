# Day 14 Pre-Research: Single-Crate Module Restructure + Context/Storage/EventSource

**Date:** 2026-05-13
**Cycle phase:** Pre-research
**Cycle:** Day 14 (engine + host module split + multi-tenant abstractions)
**Toolchain assumed:** Rust 1.85 (MSRV), Cargo 1.95.0, edition = "2024" upgrade path noted in Q1.

---

## Executive summary

The crate today is roughly ~5,000 LOC of host-coupled procedural code: every
public function calls a free function `paths::loop_home()` that reads
`$LOOP_HOME` from process-global env state. There is one tenant
(implicit: whoever runs the daemon), one storage backend (the user's home
filesystem), and one event source (the Claude Code JSONL watcher). Tests
mutate global env vars under a process-wide `ENV_LOCK` mutex. That model
served the first 13 days, but it is the ceiling — the moment we want a
second tenant, a second storage backend, or a second host, the global-env
pattern collapses.

Day 14 restructures the source tree into two consumer layers — `engine/`
(host-agnostic pure logic) and `host/claude_code/` (the JSONL watcher and
future Anthropic Haiku adapter) — and threads three new compile-time
abstractions through every public engine API: `engine::Context` (per-call
identity bundle, ~24 bytes, cheap to `Clone`, passed by reference),
`engine::Storage` (object-safe `async_trait` over a small key-addressed
byte-blob interface, with a synchronous tempdir-backed test impl that
also satisfies the async trait via blocking shims), and
`engine::EventSource` (a `Stream<Item = EngineEvent>` producer with a
typed shutdown handle). Single-user default ships day one via
`Context::single_user_local()` which still resolves storage roots from
`$LOOP_HOME`.

The right idiomatic-Rust target here is the **tower/hyper/object_store
pattern**, not the **anyhow/tracing pattern**. tower threads
`Service<Request, Response>` by value; hyper threads `&Request` with
typed extensions; object_store carries `&Path` plus a `&dyn ObjectStore`
through every call. We adopt the object_store layering: the engine
**takes `ctx: &Context` and `storage: &dyn Storage`** as function
parameters, never via thread-local, never via env. Each gets a
struct-level `Engine` facade for the binary's wiring path that holds
`Arc<dyn Storage>` and constructs `Context` per request, but the
underlying functions remain free-of-state and unit-testable.

Compile-time invariants we get for free with this shape:
1. The engine becomes `pub`-stable: nothing inside `engine/` may reach
   into `host/`. A `#[deny(...)]` lint on `crate::host::*` use inside
   `engine::*` modules guarantees this at compile time.
2. `Storage` is **sealed** (private trait bound) so external impls of
   our public engine surface cannot violate our invariants — only `host/`
   may add backends.
3. `Context` is `#[non_exhaustive]` so adding `team_id`, `agent_id`, or
   `request_id` later is non-breaking.
4. Type-state on `Context` distinguishes `SingleUser` from `MultiTenant`
   only where it matters (the `Storage` resolver), not as a runtime
   discriminant — zero-cost.

We do **not** introduce a Cargo workspace; user-decided. We do **not**
introduce feature gates for engine-vs-host (gates split test surface
and slow CI). The boundary is a module boundary backed by a
`deny.toml`-style lint, plus eventually `cargo-public-api` snapshots
locked to the `engine::` namespace.

---

## Q1: Module organization

### Survey

Looking at how mature single-crate Rust projects separate "core logic"
from "platform glue":

| Crate | Pattern | What we steal |
|---|---|---|
| **tokio** (1.46) | `runtime/` vs `net/`, `fs/`, `process/`. The "core" `runtime`, `task`, `sync` modules are platform-neutral; `net`/`fs`/`process` are platform glue. No feature gates between them — everything is `pub mod` under `tokio::`. Platform branching is *inside* the platform modules via `cfg`. | Pure module boundaries, no top-level features. |
| **hyper** (1.7) | `body`, `service`, `proto` are core; `client`, `server`, `upgrade` are integration. Each is a top-level module, all `pub`. Internal-only items live in `proto::h1`, `proto::h2` and are `pub(crate)`. | Use `pub(crate)` aggressively for adapter internals. |
| **tower** (0.5) | `service::Service` trait is the seam. Everything is a `Service` — middlewares, routers, clients, servers. Core lives in `tower-service` (separate tiny crate); `tower` itself is composable middlewares. | Trait-as-seam pattern. Our seams: `Storage`, `EventSource`. |
| **axum** (0.8) | Single crate but heavy use of `pub use` re-exports at the root. Internal modules like `handler::future` are public for completeness but the curated API lives at `axum::` directly via `pub use`. | Curate the engine prelude. `loop_engine::lessons::get_by_id` is the entry, not `loop_engine::engine::lessons::loader::get_lesson_by_id`. |
| **serde** (1.0) | Famous for the trait-and-derive split. `Serializer`/`Deserializer` are the seams; data formats (`serde_json`, `serde_yaml`) live in separate crates. Inside `serde` itself, `ser` and `de` are flat top-level modules. | If we ever extract `loop-engine` to its own crate, the seam is already done. |
| **object_store** (Apache) (0.11) | The pattern we steal most directly. `ObjectStore` trait + path-addressed API (`Path`). Implementations (`LocalFileSystem`, `AmazonS3`, `GoogleCloudStorage`) ship as struct types under sibling modules; backends gated by feature flags only when they pull heavy deps. | This is our `engine::Storage` pattern. |
| **opendal** (0.51) | `Operator` facade + `Accessor` trait. Operator holds `Arc<dyn Accessor>`. Heavily feature-gated per backend. | Confirms: facade + trait, hold `Arc<dyn _>`. |

### Recommendation

Move to:

```
src/
├── lib.rs                    — pub use surface curation only (~20 LOC)
├── main.rs                   — binary entry, wires host into engine
├── engine/
│   ├── mod.rs                — `pub mod` declarations + `pub use` prelude
│   ├── context.rs            — Context, ContextBuilder, scope types
│   ├── storage/
│   │   ├── mod.rs            — `Storage` trait + sealed marker
│   │   ├── filesystem.rs     — LocalFsStorage (the only impl in engine; lives here so single-user default works without host)
│   │   └── error.rs          — StorageError
│   ├── events.rs             — EventSource trait + EngineEvent enum
│   ├── lessons/              — moves from src/lessons/ unchanged in API SHAPE; functions gain (ctx, storage) params
│   ├── yaml/                 — moves from src/yaml/ unchanged (no state, no ctx needed)
│   ├── lifecycle.rs          — moves from src/lifecycle.rs; takes &Context
│   ├── buffer.rs             — moves from src/buffer.rs
│   ├── pid.rs                — moves from src/pid.rs
│   ├── paths.rs              — moves from src/paths.rs but loop_home() becomes pub(crate); public API is via Context+Storage
│   └── sentiment/            — Day 15 lands here
└── host/
    └── claude_code/
        ├── mod.rs            — `pub mod` for jsonl_watcher, future haiku_client, future auto_memory_ingest
        ├── jsonl_watcher/    — moves from src/watcher/; impl EventSource
        ├── haiku_client.rs   — Day 15
        └── auto_memory_ingest.rs — later
```

Module-vs-feature decision: **modules**. Three reasons:

1. **Feature gates split the test matrix.** `cargo test` runs only the
   default feature set unless you pass `--all-features` or `--no-default-features`.
   The Day 13 audit found one cfg-gated test path that hadn't run in
   3 days. We don't pay for the same mistake twice.
2. **The single binary always wants both.** `loop-daemon` is the host —
   it always wires `host::claude_code::jsonl_watcher` into the engine.
   No build configuration actually wants engine without host. (When we
   extract `loop-engine` as a published crate, *that* crate has feature
   gates for backends — but the daemon crate doesn't.)
3. **Compile-time enforcement is cheaper via lint.** A `clippy::disallowed_methods`
   or a custom `xtask` grep that fails CI if `engine/**` mentions
   `crate::host::*` enforces the seam without slowing the build.

Edition: stay on `edition = "2021"` for Day 14. Edition 2024 brings RPIT
lifetime captures and `gen` blocks that we don't yet need; bumping is a
separate audit. Note for the Day 14 learn phase only.

### Code sketch

`src/lib.rs` (curated surface):

```rust
//! loop-engine: host-agnostic cognitive memory engine.
//! loop-daemon: claude-code host adapter + binary entry.

pub mod engine;        // The "to-be-extracted-as-loop-engine" surface.
pub mod host;          // Host adapters; not part of the stable engine surface.

// Prelude: most consumers want these directly.
pub use engine::context::{Context, Scope};
pub use engine::storage::{Storage, StorageError};
pub use engine::events::{EngineEvent, EventSource};
```

`src/engine/mod.rs`:

```rust
//! The engine — host-agnostic. Stability contract: anything `pub` here
//! is part of the loop-engine API surface and gets a cargo-semver-checks
//! snapshot. Anything `pub(crate)` is internal plumbing.

pub mod context;
pub mod storage;
pub mod events;
pub mod lessons;
pub mod yaml;
pub mod lifecycle;
pub mod buffer;
pub mod pid;
pub(crate) mod paths;   // Filesystem layout helpers — internal to default LocalFsStorage.
```

`src/host/mod.rs`:

```rust
//! Host adapters. Consumes the engine; not consumed by it. Anything
//! here is unstable — break freely.

pub mod claude_code;
```

### Trade-offs

| Option | Pros | Cons |
|---|---|---|
| **Modules + lint (chosen)** | Single compile unit, fast test loop, clean cargo doc output, no feature matrix | Enforcement is a lint, not the type system — a developer could route around it |
| Feature gates `engine` and `host` | Pure compile-time enforcement | CI matrix explodes; `cargo doc` produces conditional docs; cargo-edit churn |
| Cargo workspace `engine/` + `host/` crates | True compile barrier; can publish engine independently | User decided against workspace; binary wiring becomes path dep; rust-analyzer slower on cold start |
| Internal-only crate split via `pub(in crate::engine)` everywhere | Type-system enforcement | Punishing to refactor; future contributors hit "why is this not pub" walls; not the convention in mature crates |

### Audit smells

- **TS-style barrel modules** (`pub mod x; pub mod y; pub mod z;` with no
  curation). The TS side has `core/src/index.ts` that re-exports
  everything. Don't transliterate — Rust convention is curated prelude
  at the crate root, full surface inside modules.
- **`pub use crate::engine::lessons::loader::*`** as a flatten — kills
  module documentation grouping. Use `pub use engine::lessons::{get_by_id, write};`
  with named items.
- **Glob `pub use *`** anywhere. Convention is named re-exports for
  semver reasoning.
- **Module called `utils`, `helpers`, `common`, `shared`.** TS habit.
  Rust convention: name the actual concern (`engine::yaml::scalar`,
  not `engine::utils::yaml`).
- **`mod.rs` files >100 LOC.** Convention since edition 2018 is that
  `mod.rs` is a thin barrel; logic lives in sibling files. Our current
  `src/lessons/mod.rs` (24 LOC) is correct; preserve the pattern.

---

## Q2: Context parameter pattern

### Survey

How do mature crates thread per-request identity?

| Crate | Pattern | Mechanism | Verdict for us |
|---|---|---|---|
| **tower** | `Service<Request, _>` carries everything in the `Request` type | By value, generic over Request | Too coupled — every function would be generic in `Context` |
| **hyper** | `Request<Body>` with `Extensions` (a typemap) | `request.extensions().get::<MyType>()` | Untyped at the call site; not "no guesswork" — runtime `Option` everywhere |
| **tracing** | `Span` and `Context` via thread-locals + `Subscriber` | Implicit via `#[instrument]`/`Span::current()` | Implicit context = footguns. Excellent for observability, wrong for identity routing |
| **axum** | `FromRequest` extractors | `async fn handler(State(ctx): State<Context>, ...)` | Web-framework-specific; doesn't apply to a library API |
| **tokio** | `tokio::task_local!` for per-task state | Explicit scope via `LOCAL.scope(value, async { ... })` | Useful for ambient cross-cutting concerns; wrong default for identity |
| **anyhow** | `.context("msg")` chained on results | Builder-style on Result | Different concept (error context) — name collision only |
| **object_store** | Path + `&dyn ObjectStore` threaded explicitly | By reference, function param | **This** is our pattern. |
| **sqlx** | `&mut Transaction` / `&Pool` threaded explicitly | By reference, function param | Same family — explicit, typed, no thread-local |

### Recommendation

**Pass `ctx: &Context` as the first parameter of every engine public
function that touches storage or identity-scoped state.** Pass
`storage: &dyn Storage` as the second parameter when the function
needs to read or write. Don't bundle them — they have different
lifetimes and different ownership models. (`Context` is per-request
and cheap; `Storage` is process-lifetime and shared.)

Concrete `Context` shape (locked design):

```rust
// src/engine/context.rs

use std::sync::Arc;

/// Per-request identity bundle. Cheap to clone (24-32 bytes; one Arc).
/// Always passed by `&Context` through the engine.
///
/// `#[non_exhaustive]` so we can add `team_id`, `agent_id`, `request_id`
/// without a breaking change.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Context {
    pub tenant_id: TenantId,
    pub user_id: UserId,
    pub session_id: SessionId,
    pub team_id: Option<TeamId>,
}

/// Newtypes — opaque, validated, never raw `String` at API boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TenantId(Arc<str>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UserId(Arc<str>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(Arc<str>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TeamId(Arc<str>);

impl Context {
    /// Single-user local default. Today's behavior: tenant_id = "local",
    /// user_id = "default", session_id = generated.
    pub fn single_user_local() -> Self { /* ... */ }

    pub fn builder() -> ContextBuilder { /* ... */ }
}

pub struct ContextBuilder { /* ... */ }
```

### Why `Arc<str>` newtypes (not `String`, not `&'static str`, not `Copy`)?

- `String` allocates on every clone — wrong for a struct passed by
  reference but occasionally cloned into background tasks.
- `&'static str` works only for compile-time-known IDs; tenant IDs
  come from config/auth at runtime.
- `Copy` would require fixed-size byte arrays (e.g. `[u8; 16]` for
  UUIDs). Multi-tenant IDs in practice are 8-128 chars (slugs,
  UUIDs, ULIDs, hashed bearer-token prefixes). A `Copy` Context
  forces us to pick a max upfront.
- `Arc<str>` clones in ~5ns (atomic increment) and the struct as a
  whole clones in ~20ns. We never need this on a hot path
  (sentiment classification is 800ms; lesson load is 1ms+). Plenty
  of headroom.

### Why `&Context`, not `Context` or `Arc<Context>`?

- **`Context` by value**: clones at every fn call. Unnecessary; the
  inner Arcs already make clone cheap, but eliding the clone is
  cheaper.
- **`Arc<Context>`**: one level of indirection too many. We already
  have `Arc<str>` inside. Don't double-Arc. The exception: when
  the engine `spawn`s a background task, `Arc<Context>` is the
  right move there (task takes ownership). The function signature
  is still `fn foo(ctx: &Context)`; internally `let ctx = Arc::new(ctx.clone())`
  to move into the task.
- **`&Context`**: minimal, idiomatic, matches `&Path`, `&str`, `&Pool`
  from the survey above.

### Why pass `&dyn Storage` separately?

Because storage and context have **different lifetimes**:

- `Context` is created per request (or once per session in
  single-user mode).
- `Storage` is created once at daemon startup, lives until shutdown.

Bundling them would either (a) force `Storage` to clone with
`Context` (overkill — it's behind an `Arc` anyway), or (b) make
`Context` hold a borrow `&'a dyn Storage` and propagate lifetimes
through every function signature (viral lifetime infection).

### Code sketch — function signature evolution

Before (Day 13):

```rust
pub fn get_lesson_by_id(id: &str) -> Result<Option<LoadedLesson>> {
    for status in paths::LESSON_STATUS_DIRS {
        let candidate = paths::lessons_status_dir(status)?.join(format!("{id}.md"));
        // ...
    }
}
```

After (Day 14):

```rust
pub async fn get_lesson_by_id(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
) -> Result<Option<LoadedLesson>, EngineError> {
    for status in LESSON_STATUS_DIRS {
        let key = lesson_key(ctx, status, id);
        match storage.get(&key).await? {
            Some(bytes) => return Ok(Some(parse_lesson(&bytes, status)?)),
            None => continue,
        }
    }
    Ok(None)
}

fn lesson_key(ctx: &Context, status: &str, id: &str) -> StorageKey {
    // Single-user: lessons/{status}/{id}.md
    // Multi-tenant: tenants/{tenant_id}/users/{user_id}/lessons/{status}/{id}.md
    StorageKey::from(/* ... */)
}
```

The `lesson_key` function is where multi-tenancy lives. Single-user
mode collapses to today's layout; multi-tenant prefixes paths with
`tenants/<id>/users/<id>/`. Storage backends don't care about the
shape — they just hash/route the opaque key.

### Trade-offs

| Option | Compile-time safety | Test ergonomics | Idiomatic Rust score |
|---|---|---|---|
| **`&Context, &dyn Storage` params (chosen)** | High — types track ownership | High — fixture is `Context::single_user_local()` + `LocalFsStorage::tempdir()` | 10/10 |
| `Arc<Context>` globally | Med | Low — must construct Arc | 6/10 |
| tokio task-local | None | Low — must scope every test | 4/10 — feels magical |
| Engine facade struct holding both | High | Med — need facade in every test | 8/10 — good for binary wiring, bad for library calls |

### Audit smells

- **`ctx: Context` (by value)** in function signatures unless ownership
  is intended (e.g. `into_session`).
- **`fn foo<C: ContextLike>(ctx: C)`** — over-generic. We have one
  Context type. Don't trait-ify it.
- **`Context::current()`** style global getter. Would tempt us into
  task-locals; refuse.
- **`Default for Context`**. We do NOT want `Context::default()` to
  return a sentinel — the explicit `single_user_local()` factory is
  intentional friction so multi-tenant mistakes are loud.
- **Stringly-typed IDs** (`fn foo(ctx: &Context, tenant_id: &str, ...)`).
  All IDs are newtyped at the boundary.

---

## Q3: Storage trait

### Survey

Prior art for "abstract over local-disk now, S3/Postgres later":

| Crate | Trait | Object-safe? | Sync/async | Error | Key type |
|---|---|---|---|---|---|
| **object_store** 0.11 | `ObjectStore` | Yes (`dyn ObjectStore`) | `async_trait`-style — all methods `async fn` returning `Result<_, object_store::Error>` | Fixed enum error type | `&Path` (their custom `Path` type) |
| **opendal** 0.51 | `Accessor` | Yes (`Arc<dyn Accessor>`) | Async, `BoxFuture` returns | Fixed `opendal::Error` | `&str` |
| **rusty_s3** (Sigil) | N/A — concrete S3 builder | N/A | sync builders + ureq/reqwest | thiserror | N/A — S3-specific |
| **sled** 0.34 | `Db` is the concrete type, not a trait | N/A | Sync | sled::Error | `&[u8]` |
| **redb** 2.x | Generic over schema types | Generic, not dyn | Sync | redb::Error | Typed keys |

object_store is the clearest fit. Its design choices:

1. **Object-safe trait**: methods take `&self`, return `BoxFuture` or
   are `async fn` with `async-trait`. Held as `Arc<dyn ObjectStore>`
   throughout user code.
2. **Fixed error enum**: `object_store::Error` is one type, with
   variants for `NotFound`, `AlreadyExists`, `Permission`, `Generic`,
   etc. Not an associated `type Error;`.
3. **Path-addressed**: `Path` newtype wraps a slash-delimited string,
   validated for canonical form (no `..`, no leading slash, etc.).
4. **Atomic primitives**: `put`, `get`, `delete`, `list`, `copy`,
   `rename_if_not_exists`. No transactions, no read-modify-write at
   the trait level — RMW is the caller's job.
5. **Streaming**: `get` returns a stream-of-bytes for large objects.

### Recommendation

Adopt the object_store pattern, scaled down to what the engine
actually needs. Concrete trait:

```rust
// src/engine/storage/mod.rs

use std::fmt::Debug;
use async_trait::async_trait;
use bytes::Bytes;

/// Engine storage abstraction. Object-safe. Held as `Arc<dyn Storage>`.
///
/// Sealed — only the engine crate may add impls of this trait. Hosts
/// implement `Storage` indirectly by composing engine-provided backends
/// (LocalFsStorage today; future MemoryStorage, S3Storage).
#[async_trait]
pub trait Storage: Send + Sync + Debug + sealed::Sealed {
    /// Read a key's contents. None if absent.
    async fn get(&self, key: &StorageKey) -> Result<Option<Bytes>, StorageError>;

    /// Write key (overwrite if exists). Implementations must be
    /// crash-atomic — partial writes never observable.
    async fn put(&self, key: &StorageKey, bytes: Bytes) -> Result<(), StorageError>;

    /// Delete a key. Idempotent — deleting a missing key is Ok(()).
    async fn delete(&self, key: &StorageKey) -> Result<(), StorageError>;

    /// List keys under a prefix. Returns keys, not bytes. Implementations
    /// may stream; we currently consume into Vec at the boundary.
    async fn list(&self, prefix: &StorageKey) -> Result<Vec<StorageKey>, StorageError>;

    /// Atomic compare-and-set for cross-process safe RMW. Returns
    /// Ok(true) on success, Ok(false) if the precondition failed.
    /// Implementations: local fs uses sidecar flock + atomic rename;
    /// S3 uses If-Match etag; memory uses Mutex.
    async fn put_if_version(
        &self,
        key: &StorageKey,
        bytes: Bytes,
        expected_version: Option<&Version>,
    ) -> Result<bool, StorageError>;
}

/// Engine version token returned alongside `get` for CAS workflows.
/// Implementation-defined opaque blob. Local fs: mtime+inode. S3: etag.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version(Box<[u8]>);

mod sealed {
    pub trait Sealed {}
}
```

Storage key:

```rust
// src/engine/storage/key.rs

/// Slash-delimited path-like key. Always normalized (no `..`, no
/// leading slash, no empty segments). Constructed from typed
/// builders, never from raw user input.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StorageKey(String);

impl StorageKey {
    pub fn lesson(ctx: &Context, status: &str, id: &str) -> Self {
        match ctx.tenant_id.as_ref() {
            "local" => Self(format!("lessons/{status}/{id}.md")),
            other   => Self(format!("tenants/{other}/users/{}/lessons/{status}/{id}.md", ctx.user_id)),
        }
    }
    pub fn pid_file(ctx: &Context) -> Self { /* ... */ }
    pub fn config(ctx: &Context) -> Self { /* ... */ }
    // ... one constructor per resource shape; never a raw `from_str`
}
```

### Why object-safe (`dyn Storage`) and not generic (`<S: Storage>`)?

1. **Monomorphization cost.** Every public engine fn that touches
   storage would re-monomorphize against each backend. Compile time
   balloons; binary size doubles.
2. **Wiring at the binary boundary.** `main.rs` picks one backend at
   runtime (config-driven). With a generic seam, the entire engine
   becomes a generic type parameter. Test code becomes
   `fn test<S: Storage>(s: &S)`. `dyn Storage` lets every function
   call site type-check the same regardless of backend.
3. **Trait objects are idiomatic for runtime polymorphism.**
   object_store, opendal, hyper's `Service` (which IS generic, but
   they pay for it), tower's middleware stack — the polymorphic
   storage case is the textbook `dyn` use case.

Cost: one virtual call per storage op. Storage ops are I/O-bound
(microseconds at minimum). Virtual call dispatch is sub-nanosecond.
Negligible.

### Why `async fn` (`async_trait`) and not sync?

- Current filesystem operations are sync (`std::fs::read_to_string`).
- Future remote storage (S3, Postgres) is async-only — `reqwest`,
  `tokio-postgres` have no blocking story we'd accept.
- A sync trait now means a wholesale rewrite later. An async trait
  now means a thin `tokio::task::spawn_blocking` wrap around the
  current sync code today.

`async_trait` macro decision: rust 1.85's native `async fn` in
traits **does** work for object-safe traits (stabilized 1.75) BUT
the returned future is not `Send` by default, which breaks
`Arc<dyn Storage>` use in `tokio::spawn`. The `async_trait` macro
boxes the future and explicitly marks `Send`. We need `Send`. Use
`async_trait` (license: MIT OR Apache-2.0, version 0.1.x).

Alternative: native `async fn in trait` with explicit
`return_position_impl_trait_in_trait` and `Send` bounds — works
but verbose and fragile. Stick with `async_trait` until edition
2024 cleanup pass.

### Why fixed `StorageError` and not `type Error;`?

Survey across object_store, opendal, sqlx, rusty_s3: every one
uses a **fixed error enum**, not an associated type. Reasons:

1. **Call site sanity.** With `type Error;`, every storage caller
   must either be generic over `S::Error` or convert to a
   common error type. Both are noise.
2. **Conversion uniformity.** `EngineError: From<StorageError>` is
   one impl. With associated types, it becomes
   `impl<S: Storage> From<S::Error> for EngineError` — a blanket
   impl that may conflict with other From impls.
3. **What's the actual variant set?** It's the same across
   backends: NotFound, AlreadyExists, PermissionDenied,
   VersionMismatch, Backend(BoxedError). Codify it.

```rust
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("storage key not found: {key}")]
    NotFound { key: String },
    #[error("storage key already exists: {key}")]
    AlreadyExists { key: String },
    #[error("storage permission denied: {key}")]
    PermissionDenied { key: String },
    #[error("storage version mismatch on {key}")]
    VersionMismatch { key: String },
    #[error("storage backend: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}
```

### Key type: custom `StorageKey`, not `&str` / `PathBuf`

- `&str` lets callers pass `../etc/passwd`. Reject at boundary.
- `PathBuf` is OS-flavored (backslashes on Windows); storage keys
  are abstract and should be `/`-delimited everywhere.
- Custom `StorageKey` with constructor functions for each resource
  shape (`StorageKey::lesson(ctx, status, id)`) means we never
  expose raw string concat to user code. Multi-tenant prefix
  routing happens in one place per resource.

### Trade-offs

| Decision | Alternative | Why we chose |
|---|---|---|
| `dyn Storage` | `<S: Storage>` generic | Compile time + simpler call sites |
| `async_trait` | Native async fn in trait | Send bounds work out-of-box |
| Fixed `StorageError` | `type Error;` | Caller convenience, matches ecosystem |
| Custom `StorageKey` | `&str` | Validation + multi-tenant routing in one place |
| Sealed trait | Open trait | Engine impls only; backends gated through engine |
| CAS via `put_if_version` | Lock+RMW | Maps cleanly to S3 etag/Postgres serializable |

### Audit smells

- **Storage trait methods taking `&Context`**. Wrong layer. Storage
  is identity-agnostic — it sees `StorageKey` only. Context routing
  happens in `StorageKey::lesson(ctx, ...)`. If a Storage method
  needs Context, the layering is broken.
- **`type Item;` or other associated types on Storage**. Object-safe
  traits avoid associated types where possible. Use newtype wrappers
  in concrete code.
- **`Storage: Clone`**. No — clone via `Arc<dyn Storage>`.
- **`fn get(&self, key: String)`** (owned key). Borrowed `&StorageKey`
  is the convention; the method body never needs ownership.
- **Returning `String` instead of `Bytes`**. Lesson files are UTF-8
  but config and future blob-shaped data isn't guaranteed. `Bytes`
  is cheap to clone (Arc internally) and explicit about non-UTF-8.

---

## Q4: EventSource trait

### Survey

| Crate | Pattern | Verdict |
|---|---|---|
| **futures::Stream** | `Stream<Item = T>` — pull-based async iterator | The de facto Rust async sequence type |
| **tokio_stream** | Re-exports `Stream`; adds combinators | Convenience layer over futures::Stream |
| **notify** 8.x | sync callback `Fn(notify::Result<Event>)` | Wrong for our async engine; needs bridging |
| **async-channel** / **flume** | mpsc producer + consumer | Channel-based; flexible but trait-less |
| **tokio::sync::mpsc** | mpsc with Send bounds | What our existing watcher uses |
| **rdkafka** (Kafka consumer) | `Stream` impl on the consumer | Good prior art for "event source that wraps a callback" |

Watch our existing watcher: `runner::spawn_watcher` returns a
`WatcherHandle` and emits over `mpsc::UnboundedSender<WatcherEvent>`.
The consumer holds the receiver. This pattern works but isn't a
trait — it's a concrete shape.

### Recommendation

Define `EventSource` as a `Stream<Item = EngineEvent>` producer
factory, NOT a stream itself. The factory pattern matches how
tokio + rdkafka + reqwest streams work:

```rust
// src/engine/events.rs

use futures::stream::BoxStream;
use std::pin::Pin;
use tokio_util::sync::CancellationToken;

/// Normalized event types the engine consumes. Host adapters convert
/// their native events (Claude Code JSONL, Anthropic Haiku response,
/// Auto Memory feed) into one of these variants before emitting.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum EngineEvent {
    UserTurn { /* ... fields ... */ },
    UserInterrupt { /* ... */ },
    SessionStarted { /* ... */ },
    SessionEnded { /* ... */ },
    SentimentSignal { /* ... */ },
    // future: AutoMemoryCandidate, HaikuClassification, ...
}

/// A factory + handle for an event stream. Object-safe via
/// `Arc<dyn EventSource>`. Implementations live in `host/*`.
#[async_trait::async_trait]
pub trait EventSource: Send + Sync {
    /// Begin emitting events. The returned stream lives until the
    /// passed `shutdown` token is cancelled OR the source naturally
    /// terminates. Emitting `Err` is non-fatal — the source decides
    /// whether to continue.
    ///
    /// Pinned + Send: the stream may cross thread boundaries
    /// (tokio multi-thread runtime).
    async fn run(
        &self,
        ctx: &Context,
        shutdown: CancellationToken,
    ) -> BoxStream<'static, Result<EngineEvent, EventSourceError>>;

    /// Diagnostic name for logs / health endpoints.
    fn name(&self) -> &'static str;
}
```

### Why a `Stream` factory and not a `Stream` directly?

- **Lifecycle handle.** A bare `Stream` has no way to signal
  shutdown to its producer task. Our watcher needs explicit
  drop-the-FSEvents-stream-on-shutdown semantics. The shutdown
  token is the right primitive (already in use from Day 10).
- **Construction is async-able.** Starting the JSONL watcher
  requires reading directory metadata, scanning existing files,
  setting initial offsets. That's async I/O. The factory `run`
  awaits the setup, then returns the live stream.
- **Multi-source aggregation.** Engine startup wires N
  `Arc<dyn EventSource>` into a `futures::stream::select_all`,
  merging into one consumer loop. Factory pattern makes this
  uniform.

### Why `BoxStream`, not an associated type `type Stream: Stream`?

Same reasoning as Storage: object-safety. `BoxStream` is
`Pin<Box<dyn Stream<Item = ...> + Send + 'static>>` — one
allocation per stream is fine (we have a small fixed number of
event sources, never per-event).

Native `async fn` returning `impl Stream` would be tighter but
fails `dyn EventSource` object-safety. `BoxStream` keeps the
trait object-safe.

### Why `Result<EngineEvent, EventSourceError>` items, not just `EngineEvent`?

The watcher already has `WatcherEvent::ParseError`. Lifting that
into the stream-level `Err` slot lets the engine consumer
distinguish "stream is broken" from "one event is malformed":

```rust
while let Some(evt) = stream.next().await {
    match evt {
        Ok(engine_evt) => process(engine_evt).await,
        Err(EventSourceError::Transient(e)) => {
            tracing::warn!(error = %e, "transient event source error; continuing");
            continue;
        }
        Err(EventSourceError::Fatal(e)) => {
            tracing::error!(error = %e, "fatal event source error; stopping");
            break;
        }
    }
}
```

### Lifecycle: how does shutdown propagate?

The `CancellationToken` parameter is the contract:

```text
host wiring (main.rs):
    let shutdown = CancellationToken::new();
    let source = host::claude_code::JsonlWatcher::new(/* ... */);
    let stream = source.run(&ctx, shutdown.clone()).await;
    let handle = tokio::spawn(consume(stream, &engine));

    signal::ctrl_c().await;
    shutdown.cancel();       // stream ends; consume() drops out of loop
    handle.await?;
```

Inside the source, the implementation watches the token:
```rust
async fn run(&self, ..., shutdown: CancellationToken) -> BoxStream<...> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    // ... spawn FSEvents callback bridge ...
    tokio::spawn(async move {
        shutdown.cancelled().await;
        // Drop the notify watcher → FSEvents stops → tx drops → stream ends
    });
    Box::pin(UnboundedReceiverStream::new(rx))
}
```

### Trade-offs

| Option | Pros | Cons |
|---|---|---|
| `Stream` factory + `BoxStream` (chosen) | Lifecycle clear; object-safe; aggregatable | One Box allocation at construction |
| Bare `Stream` impl on `JsonlWatcher` | No factory | Shutdown semantics lost; construction not async |
| `mpsc` channel pair returned | Familiar to existing watcher code | Not a trait shape; no polymorphism |
| Callback-based (`fn on_event(&mut self, ...)`) | Lowest overhead | Caller manages threading; non-idiomatic in 2024 async Rust |

### Audit smells

- **Sync `EventSource::poll_event`.** Custom poll APIs in 2026 Rust
  are usually a smell — `Stream` is the standard. Use it.
- **`Vec<EngineEvent>` return** instead of a stream. The whole point
  is open-ended emission.
- **`Box<dyn Iterator>`** instead of `BoxStream`. `Iterator` is sync;
  our sources are async.
- **`'static` requirement on EventSource impls but `'a` lifetime on
  events.** Events should be `'static` (cloned, owned data). Anything
  referenced into the source's buffers is a footgun across `tokio::spawn`.
- **EventSource methods returning `Result<_, anyhow::Error>`** at the
  public boundary. Use a typed `EventSourceError` enum (Transient/Fatal
  split is meaningful to consumers).

---

## Q5: Refactor migration strategy

### Current state

- Public functions reach `paths::loop_home()` which reads `$LOOP_HOME`
  env var.
- All 127 tests use `with_temp_loop_home` helper that:
  1. Locks `ENV_LOCK` (process-wide mutex)
  2. Saves prior `LOOP_HOME`
  3. Sets `LOOP_HOME` to a tempdir
  4. Runs the test
  5. Restores prior value
- This pattern is **inherently sequential** at the test-binary level.
  All tests using LOOP_HOME run under the same lock.

### The strategy: leaf-first, two phases

**Phase 1: Add abstractions and parallel implementations** (no test
churn).

1. Land `engine::context::Context` + `engine::storage::Storage` +
   `engine::storage::LocalFsStorage`. **Do not yet remove**
   `paths::loop_home()`. `LocalFsStorage::new_from_env()` reads
   `$LOOP_HOME` the same way for default behavior.
2. Add a new public API surface on each module that takes
   `(ctx, storage)`. Keep the old API delegating to the new one:
   ```rust
   pub fn get_lesson_by_id(id: &str) -> Result<Option<LoadedLesson>> {
       let ctx = Context::single_user_local();
       let storage = LocalFsStorage::default()?;
       futures::executor::block_on(get_by_id_v2(&ctx, &storage, id))
   }
   pub async fn get_by_id_v2(ctx: &Context, storage: &dyn Storage, id: &str)
       -> Result<Option<LoadedLesson>, EngineError> { /* ... */ }
   ```
3. New tests use the v2 API + `LocalFsStorage::new_in_tempdir()`.
   No env var lock; tests can run in parallel.
4. Existing tests still pass unchanged.

**Phase 2: Migrate old call sites and tests**, module by module.

1. Pick the leaf module first — `lessons::loader` (Day 12 work, no
   internal deps on other engine modules).
2. Update every caller in `src/` and in `tests/` to use the new
   signature. Delete the old wrapper.
3. Convert tests away from `with_temp_loop_home` to a new
   `with_test_engine` helper:
   ```rust
   fn with_test_engine<F, T>(f: F) -> T
   where
       F: FnOnce(&Context, &dyn Storage) -> T,
   {
       let ctx = Context::single_user_local();
       let storage = LocalFsStorage::new_in_tempdir().unwrap();
       f(&ctx, &storage)
   }
   ```
4. Once a module is migrated, **delete** its `with_temp_loop_home`
   usage. ENV_LOCK contention reduces incrementally.
5. Move to the next leaf module. Repeat. Final removal: delete
   `paths::ENV_LOCK` entirely.

Module migration order (leaf-to-root by dependency):
1. `yaml/*` — pure functions, no paths, **no Context needed at all**.
   Don't add `&Context` to yaml functions; they're identity-free.
2. `buffer.rs` — same.
3. `pid.rs` — currently calls `paths::daemon_pid_path()`. New
   signature: `pid::write(ctx, storage, pid)`. `pid_path` becomes
   `StorageKey::pid_file(ctx)`.
4. `lessons/loader.rs` — Day 12 leaf.
5. `lessons/signals.rs` — depends on loader + lock; co-migrate with
   loader.
6. `lessons/lock.rs` — co-migrate.
7. `lifecycle.rs` — bigger surface, depends on pid; migrate after pid.
8. `watcher/*` → `host::claude_code::jsonl_watcher` directory move +
   `EventSource` impl. Migrate last because it's the most code.

### What does a Context fixture look like in tests?

```rust
// tests/common/mod.rs (new) — shared test fixtures.

use loop_daemon::engine::{Context, Storage};
use loop_daemon::engine::storage::LocalFsStorage;

pub struct TestEngine {
    pub ctx: Context,
    pub storage: LocalFsStorage,
    _tempdir: tempfile::TempDir,   // hold for RAII cleanup
}

impl TestEngine {
    pub fn new() -> Self {
        let tempdir = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new_at(tempdir.path()).unwrap();
        let ctx = Context::single_user_local();
        Self { ctx, storage, _tempdir: tempdir }
    }

    pub fn with_tenant(tenant: &str, user: &str) -> Self {
        let tempdir = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new_at(tempdir.path()).unwrap();
        let ctx = Context::builder()
            .tenant_id(tenant)
            .user_id(user)
            .session_id(format!("test-session-{}", uuid::Uuid::new_v4()))
            .build();
        Self { ctx, storage, _tempdir: tempdir }
    }
}

// Usage in a test:
#[tokio::test]
async fn finds_lesson_in_active_status() {
    let eng = TestEngine::new();
    write_test_lesson(&eng, "active", "les-aaaaaaaa").await;
    let loaded = get_by_id(&eng.ctx, &eng.storage, "les-aaaaaaaa")
        .await
        .unwrap()
        .expect("lesson should be found");
    assert_eq!(loaded.status_dir, "active");
}
```

Critical: no `ENV_LOCK`, no `unsafe { env::set_var }`, no
serialization at the binary level. Tests run in parallel.

### Migration: incremental vs big-bang

**Incremental.** Two reasons:
1. The audit cycle (per the user's standing rule) requires landing
   work in small enough chunks that each can be audited end-to-end.
   A big-bang migration produces an ungovernable diff.
2. The TS-vs-Rust cross-process behavior (flock sidecar, atomic
   rename) is verified in tests today. Moving them all at once
   risks introducing a regression that we can't bisect.

Total Day 14 estimate: 3-4 module-migration commits, each
self-contained, each with its own audit pass. The framework
(Context + Storage + LocalFsStorage) ships first; then the yaml +
buffer move (no-op API change); then the lessons migration; then
the watcher refactor.

### Trade-offs

| Strategy | Pro | Con |
|---|---|---|
| **Phase 1 + 2 incremental (chosen)** | Audit-able, bisectable, tests stay green throughout | More commits, slightly more churn |
| Big-bang single PR | One audit | Diff size; high risk |
| Run both APIs forever | No forced migration | API surface bloat; confusion |
| Bridge via macro | DRY | Macro debug pain; obscures behavior |

### Audit smells

- **Calling `Context::single_user_local()` deep inside an engine
  function.** If a function needs Context, it takes Context. Construction
  happens at the engine boundary only.
- **`block_on` in async-aware code.** The Phase 1 delegating wrapper
  uses `block_on`, but Phase 2 deletes all wrappers; no `block_on`
  may survive past the cleanup phase.
- **Tests that take `&mut env::Vars`.** Should not exist after migration.
- **`#[serial]` test attribute** (from the `serial_test` crate) anywhere
  in `engine::*`. If tests need serializing, the abstraction is leaking.
  Host adapter tests may still need it for filesystem-watching tests.

---

## Q6: Compile-time invariants to encode

The user's rule, verbatim: *"I want the refactor what is best for Rust.
I don't want any guess work this is extremely important."* The
compile-time invariants are the answer to "no guesswork."

### 1. Sealed traits for `Storage` and `EventSource`

```rust
pub trait Storage: sealed::Sealed { /* ... */ }
mod sealed {
    pub trait Sealed {}
    impl Sealed for crate::engine::storage::LocalFsStorage {}
    impl Sealed for crate::engine::storage::MemoryStorage {}
    // External crates physically cannot impl this trait.
}
```

Why: prevents downstream crates from creating Storage impls that
violate engine invariants (atomic-rename CAS, key-validation rules).
When we extract `loop-engine`, this stays sealed — backends are
opt-in via engine features, not external impls. Standard idiom
(used by `serde`, `tokio::sync::oneshot::error::*`).

### 2. `#[non_exhaustive]` on `Context`, `EngineEvent`, `StorageError`, `EventSourceError`

Every enum and struct on the engine public surface that may grow
new variants/fields gets `#[non_exhaustive]`. Forces downstream
consumers to write `_ => ...` arms / `..` patterns, preventing
"my code broke because you added a variant" complaints — a stable
engine surface is the explicit goal.

### 3. Newtype IDs (compile-time disambiguation)

```rust
pub struct TenantId(Arc<str>);
pub struct UserId(Arc<str>);
pub struct SessionId(Arc<str>);
pub struct TeamId(Arc<str>);
```

`get_lesson(&ctx.user_id, &ctx.tenant_id)` and
`get_lesson(&ctx.tenant_id, &ctx.user_id)` would both be valid
under stringly-typed parameters. With newtypes the compiler enforces
the right order.

Implementation note: derive `From<&str>` only inside the crate
(via `pub(crate)`), with the public constructor being
`TenantId::new(s) -> Result<Self, IdError>` that validates the
format. External code can never accidentally make a `TenantId`
from arbitrary string data.

### 4. Type-state on Storage backend selection (only where useful)

We do NOT type-state `Context` itself. Single-user vs multi-tenant
is a runtime concern (the tenant ID is "local" or "real"). Type-stating
this would bifurcate the engine API into two parallel sets — not
worth it.

We DO type-state where it has actual safety value: the **builder**
for `LocalFsStorage`:

```rust
pub struct LocalFsStorageBuilder<RootSet, LockMode> {
    root: Option<PathBuf>,
    lock_mode: Option<LockMode>,
    _phantom: PhantomData<(RootSet, LockMode)>,
}

pub struct Unset;
pub struct Set;

impl LocalFsStorageBuilder<Unset, Unset> {
    pub fn new() -> Self { /* ... */ }
}

impl<L> LocalFsStorageBuilder<Unset, L> {
    pub fn root(self, p: PathBuf) -> LocalFsStorageBuilder<Set, L> { /* ... */ }
}

impl LocalFsStorageBuilder<Set, Set> {
    pub fn build(self) -> Result<LocalFsStorage, StorageError> { /* ... */ }
}
```

`build()` only exists when both `root` and `lock_mode` are set —
compile error otherwise. This is the typical typestate use case
(small, finite, has-this-been-configured).

### 5. PhantomData markers — used sparingly

Avoid PhantomData unless it's load-bearing for variance or lifetime.
The newtype IDs don't need them (Arc<str> is `Send + Sync`). The
builder uses them only where typestate requires; nowhere else.

### 6. Exhaustive matches on `EngineEvent` inside the engine

`EngineEvent` is `#[non_exhaustive]` on the **public** surface but
**inside** the engine module we match exhaustively (no `_` wildcards)
so adding a variant is a forced visit-every-handler compile error.
Use `#[deny(non_exhaustive_omitted_patterns)]` at the `engine::events`
module level (stable since 1.70).

### 7. `Send + Sync` bounds on every public trait

```rust
pub trait Storage: Send + Sync + Debug + sealed::Sealed { /* ... */ }
pub trait EventSource: Send + Sync { /* ... */ }
```

Multi-threaded tokio runtime; trait objects must cross threads.
Lacking these bounds means `Arc<dyn Storage>` may not be `Send` —
caught only at use sites, with terrible error messages. Bake them in.

### 8. `cargo-public-api` snapshot for the engine module

Commit a `public-api/engine.txt` snapshot. CI fails on any change.
Forces a deliberate review of any addition/change to the engine
public surface — the "stable contract" the user wants for "keep
updating just the engine and use it to other projects."

### Trade-offs

| Invariant | Benefit | Cost |
|---|---|---|
| Sealed traits | Backend impls controlled | External users can't write custom Storage for niche backends |
| `#[non_exhaustive]` | Future-proof | Match arms need wildcards |
| Newtype IDs | Argument order safety | Newtype impl boilerplate (one-time) |
| Builder typestate | Misconfiguration → compile error | Builder code is more verbose |
| `cargo-public-api` snapshot | Stable contract | CI step + occasional update PRs |

### Audit smells

- **String-typed scope** (`scope: String` for `tenant|app|skill_set`).
  Use an enum `Scope` with explicit variants. Matches TS's
  `MemoryScope` union but as a Rust enum.
- **`Box<dyn Error>`** at public boundaries. Use named error enums
  (`thiserror`-derived) so downstream pattern-match.
- **Unbounded type params on public functions.** `fn foo<T>(t: T)` —
  if T is unbounded, T can be anything, and the function probably
  shouldn't exist. Bound it or replace with a concrete type.
- **`Result<T, ()>`**. `()` error is a thrown-away signal. Use a
  named error type or `Option<T>`.

---

## Q7: Public surface plan

The user said: *"I want to be able to keep updating just the engine
and use it to other projects."* This implies `engine::*` is a stable
API contract; `host::*` is not.

### Concrete visibility plan

**`crate::engine::` (the stable surface):**

| Item | Visibility | Rationale |
|---|---|---|
| `Context`, `Scope`, `TenantId`, `UserId`, `SessionId`, `TeamId` | `pub` | Threaded through every API |
| `ContextBuilder` | `pub` | Construction surface |
| `Storage` trait | `pub` (sealed) | The seam |
| `StorageKey`, constructors | `pub` | Caller-visible |
| `StorageError` | `pub` | Error propagation |
| `LocalFsStorage` | `pub` | Default impl, used by tests + binary |
| `MemoryStorage` (Day 14+) | `pub` | Test fixture |
| `EventSource` trait | `pub` | The seam |
| `EngineEvent` | `pub` | Item type |
| `EventSourceError` | `pub` | Error type |
| `lessons::get_by_id`, `lessons::record_sentiment_signal` | `pub` | Core lesson API |
| `lessons::LoadedLesson`, `lessons::SignalPolarity` | `pub` | Data |
| `yaml::*` (parser/writer) | `pub` | Used by host adapters that produce frontmatter (e.g. Auto Memory ingest) |
| `lifecycle::*` | `pub` | Daemon facade |
| `paths` module | `pub(crate)` | Internal — replaced by Storage |
| Test fixtures like `Context::test_default()` | `pub(crate)` | Only crate-internal tests use them |

**`crate::host::claude_code::` (unstable):**

| Item | Visibility | Rationale |
|---|---|---|
| `JsonlWatcher` (impl EventSource) | `pub` | Wired by `main.rs` |
| `jsonl_watcher::*` internal modules | `pub(crate)` or private | Implementation detail |
| Future `HaikuClient` | `pub` | Wired by `main.rs` |

### Re-export strategy (`src/lib.rs`)

Curated prelude at the crate root for ergonomics:

```rust
pub mod engine;
pub mod host;

// Curated prelude.
pub use engine::{
    Context, Scope,
    storage::{Storage, StorageKey, StorageError, LocalFsStorage},
    events::{EventSource, EngineEvent, EventSourceError},
};
```

This is the same pattern tokio uses (`pub use sync;`, `pub use net;`)
and axum uses (`pub use Router;`, `pub use Json;`). Direct module
access still works (`loop_daemon::engine::lessons::get_by_id`) for
those who want the full path.

### `engine::prelude` module (convention)

Add `engine::prelude` for the "import-star use case":

```rust
// src/engine/prelude.rs
pub use super::context::{Context, ContextBuilder, Scope, TenantId, UserId, SessionId, TeamId};
pub use super::storage::{Storage, StorageKey, StorageError, LocalFsStorage};
pub use super::events::{EventSource, EngineEvent, EventSourceError};
```

Use site: `use loop_daemon::engine::prelude::*;` — same convention
as `tokio::prelude` (now deprecated, but the pattern is well-known)
and `std::io::prelude`.

### What is NOT `pub` and why

- `engine::paths::loop_home()` — replaced by Storage. Was the global
  env-state hazard; do not expose.
- `engine::storage::sealed::Sealed` — public sealed marker; nobody
  outside the engine implements `Storage`.
- Internal helpers under `engine::lessons::lock::*` (sidecar file
  path computation) — `pub(crate)`, no external concern.
- The `notify::Watcher` instance inside `JsonlWatcher` — `pub(crate)`
  at most.

### Trade-offs

| Plan | Pro | Con |
|---|---|---|
| **Curated prelude (chosen)** | Clear surface; matches tokio/axum/serde | One-time investment writing the prelude |
| Re-export everything at root | Cheapest to maintain | Crate doc page becomes a mess; no discoverability |
| Force full module paths | Most explicit | Verbose at use sites |
| Per-module preludes only (no crate root) | No name collisions possible | Doesn't match Rust convention |

### Audit smells

- **`pub use *::*::*` at the crate root.** Defeats the curation goal.
- **`#[allow(dead_code)]` on a `pub` item.** If it's unused inside
  the crate AND public, justify it; otherwise demote to `pub(crate)`.
- **Public items inside `host::*`.** Host is unstable. `pub` items
  inside `host::*` should be only the wiring entry points (`JsonlWatcher::new`,
  `HaikuClient::new`).
- **No `cargo doc` cross-references**. `[`get_by_id`]` rustdoc
  links should resolve cleanly across the engine API.

---

## Q8: Testing strategy under Context/Storage

### Current pattern (Days 11-13)

Every test that touches paths follows this template:

```rust
fn with_temp_loop_home<F: FnOnce(&TempDir) -> Result<()>>(f: F) {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = TempDir::new().unwrap();
    let original = env::var(LOOP_HOME_ENV).ok();
    unsafe { env::set_var(LOOP_HOME_ENV, tmp.path()); }
    let result = f(&tmp);
    /* restore env */
    result.unwrap();
}
```

Pain points:
1. `unsafe { env::set_var }` (Rust 2024 edition makes `set_var` unsafe).
2. Process-global `ENV_LOCK` serializes all tests touching paths.
3. Failure mode: an environment variable leak from a poisoned test
   contaminates the rest of the run.
4. Cannot test multi-tenant routing (only one tenant per process).

### New pattern

```rust
// tests/common.rs (shared across integration tests)

use loop_daemon::engine::prelude::*;
use loop_daemon::engine::storage::LocalFsStorage;
use tempfile::TempDir;

pub struct TestHarness {
    pub ctx: Context,
    pub storage: LocalFsStorage,
    _tempdir: TempDir,
}

impl TestHarness {
    pub fn new() -> Self {
        let tempdir = TempDir::new().unwrap();
        let storage = LocalFsStorage::builder()
            .root(tempdir.path().to_path_buf())
            .lock_mode(LockMode::Flock)
            .build()
            .unwrap();
        let ctx = Context::single_user_local();
        Self { ctx, storage, _tempdir: tempdir }
    }

    pub fn with_tenant(tenant: &str, user: &str) -> Self { /* ... */ }
}
```

Properties:
1. **No env mutation.** No `unsafe`, no `ENV_LOCK`.
2. **Parallel-safe.** Tests run on independent tempdirs.
3. **Multi-tenant testable.** `TestHarness::with_tenant("tenant-a", "user-1")`
   exercises the multi-tenant key routing.
4. **Cheap.** Each test makes one tempdir (~50µs on APFS).
5. **No leaks.** `TempDir` Drop cleans up.

### Mock Storage (`MemoryStorage`)

For unit tests of pure engine logic that don't care about the
filesystem (e.g. validation, parsing), a `MemoryStorage` backed by
`Arc<DashMap<StorageKey, Bytes>>`:

```rust
pub struct MemoryStorage {
    inner: Arc<DashMap<StorageKey, (Bytes, Version)>>,
}

#[async_trait]
impl Storage for MemoryStorage { /* ... straightforward ... */ }
```

Use cases:
- Lesson loader unit tests (no need to write real .md files for the
  scan-five-status-dirs logic; pre-seed the map).
- Multi-tenant key prefix routing tests (verify
  `StorageKey::lesson(ctx_a, ...)` ≠ `StorageKey::lesson(ctx_b, ...)`).
- Failure injection (a `FaultyStorage` wrapper that returns
  `StorageError::Backend` on every Nth call) — leave for Day 15+.

License note: `DashMap` is MIT, fine. Alternative `tokio::sync::Mutex<HashMap<_,_>>`
also fine, slightly slower but no extra dep.

### tempdir-based vs in-memory: when to use which?

| Test type | Backend | Why |
|---|---|---|
| Lesson loader: scan 5 status dirs | `MemoryStorage` | No real I/O needed |
| YAML round-trip with TS-written file | `LocalFsStorage` (tempdir) | Tests the actual filesystem byte path |
| Sidecar flock CAS | `LocalFsStorage` (tempdir) | flock semantics are filesystem-specific |
| Watcher integration | `LocalFsStorage` (tempdir) + real `notify` | FSEvents needs a real dir |
| Concurrent signal writes | `LocalFsStorage` (tempdir) | Cross-process flock requires real fs |
| Multi-tenant routing | `MemoryStorage` | Key shapes are storage-agnostic |

### Test categories after migration

```
tests/
├── common.rs                       — TestHarness, shared fixtures
├── byte_fixture.rs                 — YAML round-trip (existing; converted to TestHarness)
├── concurrent_signal_writes.rs     — Multi-process flock (existing; tempdir-backed)
├── ts_lesson_roundtrip.rs          — TS-on-disk fixture (existing; tempdir-backed)
├── multi_tenant_routing.rs         — NEW: verify key prefixes for two tenants
├── memory_storage_smoke.rs         — NEW: confirm MemoryStorage satisfies Storage contract
└── event_source_lifecycle.rs       — NEW: cancellation token shuts down JsonlWatcher cleanly
```

Inline `#[cfg(test)]` modules in `src/**` continue to exist for
unit-level concerns (regex matching, YAML scalar emission, etc.) —
no change there.

### Migration: existing `with_temp_loop_home` tests

Each one converts to:

```rust
// Before:
#[test]
fn finds_lesson_in_active_status() {
    with_temp_loop_home(|tmp| {
        write_minimum_lesson(tmp, "active", "les-aaaaaaaa");
        let loaded = get_lesson_by_id("les-aaaaaaaa")?.unwrap();
        assert_eq!(loaded.status_dir, "active");
        Ok(())
    });
}

// After:
#[tokio::test]
async fn finds_lesson_in_active_status() {
    let h = TestHarness::new();
    write_test_lesson(&h, "active", "les-aaaaaaaa").await;
    let loaded = lessons::get_by_id(&h.ctx, &h.storage, "les-aaaaaaaa")
        .await
        .unwrap()
        .expect("lesson should be found");
    assert_eq!(loaded.status_dir, "active");
}
```

`#[tokio::test]` instead of `#[test]` because the Storage trait is
async. Add `tokio = { features = ["macros", "rt"] }` to dev-dependencies
(already present).

### Trade-offs

| Strategy | Pro | Con |
|---|---|---|
| **TestHarness + MemoryStorage (chosen)** | Parallel-safe, fast, multi-tenant testable | One-time migration cost |
| Keep `ENV_LOCK` + extend to multi-tenant | Less migration churn | Tests stay sequential; awkward as engine grows |
| `#[serial]` from `serial_test` crate | Standard pattern for env-var tests | Slow; doesn't solve the root cause |
| Stub Storage with `mockall` | Auto-generated mocks | Adds proc-macro dep; over-engineered for our small trait |

### Audit smells

- **Tests that share state across `#[test]` functions.** With per-test
  TestHarness, there's no excuse. Anything that assumes a previous
  test left state is broken.
- **`std::env::set_var` anywhere in tests** post-migration. The whole
  point of the refactor is to remove these.
- **`#[tokio::test(flavor = "current_thread")]`** to "work around" a
  test ordering issue. If you need that flavor for ordering, the
  test is racing on shared state — fix the test.
- **`MemoryStorage` used in integration tests** that should exercise
  the real filesystem. Pick the right backend for what you're
  testing.

---

## Locked decisions for Day 14 learn-notes

These have clear-best answers — they become inputs to the build phase:

1. **Module organization:** `src/engine/` and `src/host/claude_code/`
   as plain modules (not features, not workspace). Yaml + buffer +
   pid + lifecycle move into `engine/` without API shape changes. The
   `paths` module becomes `pub(crate)` inside engine, used only by
   `LocalFsStorage`.

2. **Context shape:** `Context { tenant_id, user_id, session_id, team_id: Option }`
   passed by `&Context`. `#[non_exhaustive]`. IDs are `Arc<str>`
   newtypes. `Context::single_user_local()` is the day-one default.

3. **Storage trait:** object-safe `dyn Storage`, async via
   `async_trait`, fixed `StorageError` enum, custom `StorageKey`
   newtype. `LocalFsStorage` (the existing filesystem behavior,
   refactored behind the trait) and `MemoryStorage` (test fixture)
   ship as part of the engine.

4. **Sealed trait:** `Storage: sealed::Sealed`. Backends added inside
   the crate only.

5. **EventSource trait:** factory pattern returning `BoxStream`,
   `CancellationToken` for shutdown, `Result<EngineEvent, EventSourceError>`
   items. `JsonlWatcher` becomes the first impl, living in
   `host::claude_code::jsonl_watcher`.

6. **Public surface:** `lib.rs` curates a small prelude of the
   engine essentials. Full module paths still work. `host::*` is
   unstable; engine items get a `cargo-public-api` snapshot.

7. **Test strategy:** `TestHarness` with per-test tempdir backing
   `LocalFsStorage`, plus `MemoryStorage` for pure logic tests. Drop
   `with_temp_loop_home` + `ENV_LOCK` once all callers migrated.

8. **Migration phasing:** two-phase — abstractions land first with
   delegating wrappers; module-by-module migration follows leaf-first
   (yaml/buffer → pid → lessons → lifecycle → watcher).

9. **Cargo edition:** stay on 2021. Edition 2024 bump is a separate
   audit; not for Day 14.

10. **Dependencies added:** `async-trait` (MIT/Apache), `bytes`
    (MIT, already a transitive of `tokio`/`reqwest`), `futures` (MIT/Apache,
    for `BoxStream`). `dashmap` (MIT) only if `MemoryStorage` chooses
    it over `Mutex<HashMap>` — defer the dep decision to the build
    phase.

---

## Open questions to resolve in learn phase

These need a user/owner decision before the build phase begins:

1. **`team_id` in `Context` from day one?** TS-side `MemoryScope`
   already has `tenant | app | skill_set | skill | agent_shared | agent_private`.
   We're scoping Context down to `tenant_id + user_id + session_id + team_id?`.
   Confirm: is `team_id` worth carrying in Phase 1, or do we add it
   later when we wire teams in? Recommend: yes, carry it as
   `Option<TeamId>` now — adding fields to `#[non_exhaustive]` later
   is fine, but it costs zero to include now and prevents
   "oh-we-need-it-everywhere" later.

2. **`agent_id` separate or part of session_id?** TS treats agent
   shared/private as a memory scope, but at the Context layer
   they're really part of the same session. Recommend: collapse —
   `agent_id` lives inside `SessionId` semantics for now, broken out
   later if and when we need it. Confirm.

3. **`MemoryStorage` ships in Day 14 or deferred?** Required for the
   "test multi-tenant routing without filesystem" use case, but adds
   ~150 LOC of code we don't strictly need for the daemon. Recommend:
   ship it as part of Day 14 — the multi-tenant routing tests are
   exactly the safety net we need to land Context confidently.

4. **`cargo-public-api` snapshot enforcement in CI: opt-in for
   Day 14 or block-on-mismatch?** Recommend opt-in (log diff on
   mismatch, don't fail) for Days 14-16, then promote to gating
   in Day 17 once the engine surface settles.

5. **Naming: `loop_daemon::engine` vs eventual `loop_engine`?**
   If we plan to eventually extract `engine/` as its own published
   crate `loop-engine`, the module name should foreshadow the future
   crate. Currently the crate is `loop-daemon` so users would
   `use loop_daemon::engine::...`. Recommend: keep it; the rename
   to `loop_engine` is a separate decision when extraction happens.

6. **Async-trait or hand-rolled `Pin<Box<dyn Future>>`?**
   `async_trait` macro is the convention. Recommend: macro.
   Open Q only if the build phase finds it doesn't compose with
   `BoxStream` cleanly. Confirm at build kickoff.

7. **Storage CAS via `put_if_version` or separate `lock`/`unlock`
   primitives?** Day 12's flock+sidecar pattern is closer to
   lock/unlock. Recommend: ship `put_if_version` (cloud-shaped
   primitive), implement it via flock+sidecar internally in
   `LocalFsStorage` (existing logic moves wholesale, no behavior
   change). Confirm the trait shape; reject if the lock/unlock
   semantics from Day 12 don't map cleanly.

---

## TS-with-Rust-syntax smells to flag in audit

Per the user's hard rule "TS is *what*, not *how*", here are the
specific transliteration patterns the Day 14 audit should fail on:

1. **`Arc<RwLock<HashMap<String, Context>>>` as a "context registry".**
   This is the TS pattern of looking up a context by key. Rust idiom:
   pass `&Context` through function calls; no registry.

2. **`Box<dyn Error>` at API boundaries.** TS lets you throw anything;
   Rust idiom is named error enums with `thiserror`.

3. **`async fn foo() -> Result<T, anyhow::Error>` on engine public
   functions.** `anyhow` is for the binary / host layer; engine
   functions use typed errors (`EngineError`, `StorageError`,
   `EventSourceError`).

4. **`pub fn new(...) -> Self` constructors that just assign fields.**
   TS uses constructors as a convention. Rust idiom: prefer
   `LocalFsStorage::builder()` for anything with optional/configurable
   params, or `Default::default()` for the trivial case. Plain `new`
   is fine ONLY when there's one obvious construction.

5. **`Option<Option<T>>`.** TS doesn't distinguish `undefined` from
   `null` cleanly, and the literal port becomes `Option<Option<T>>`.
   Rust idiom: use the outer Option or an explicit enum
   (`Maybe::{Set(T), Cleared, Unknown}`).

6. **`Vec<Box<dyn Trait>>` everywhere there could be a
   `Vec<EnumOfKnownVariants>`.** TS structural typing means the
   open-ended trait object is natural; in Rust, if you know the
   variant set is closed (e.g. all EventSource impls live in
   `host/`), an enum is faster and exhaustively matchable.

7. **`fn foo(ctx: &Context, opts: FooOptions)` where `FooOptions`
   is a struct with all `Option<T>` fields used as keyword args.**
   This is TS's `function foo(ctx, { x, y, z })`. Rust idiom: builder
   if there are >3 options, or simple positional args otherwise.
   `Option<T>` for every field is a smell.

8. **`async fn` that does no `await` and returns `Result<T, _>`.**
   The function should be sync. `async fn` is a contract that the
   function uses the executor; if it doesn't, it's misleading.

9. **`String` where `&str` would work.** TS strings are immutable
   refs by default; the literal port owns. In Rust, if you don't
   consume or modify, take `&str`.

10. **Reaching into `crate::host::*` from `crate::engine::*`.**
    The whole point of the boundary. Custom lint + CI check.

11. **Stringly-typed scope/status fields.** Status is one of 5 fixed
    values; tenant_id has a finite character set; scope is an enum
    in TS already. Newtype/enum at the boundary.

12. **`tokio::sync::Mutex<()>` as a "lock without value".** TS uses
    `await using mutex.lock()`. Rust idiom: if the mutex protects
    nothing, you want a `tokio::sync::Semaphore` or a typestate
    pattern — not a Mutex<()>.

13. **`Arc<Mutex<T>>` everywhere shared state is referenced.** TS's
    `class State` becomes this on autopilot. Audit each use: could
    it be `&mut T` passed through? An mpsc channel? An immutable
    `Arc<T>` because the data is read-only? Multiple readers + one
    writer is `Arc<RwLock<T>>`, not `Arc<Mutex<T>>`.

14. **`if let Some(x) = some_option { } else { return Err(...) }`.**
    Idiom: `let x = some_option.ok_or(EngineError::Missing)?;` —
    the `?` operator was made for exactly this.

15. **Manually iterating bytes for UTF-8 parsing.** TS-side has
    custom byte-walking in the YAML reader because JS strings are
    UTF-16. Rust strings are guaranteed UTF-8; use `str::chars()`,
    `str::char_indices()`, or `bstr` if true byte-level needed.

16. **Re-implementing the visitor pattern.** TS classes with `accept`
    methods. Rust idiom: closures + iterators, or enum + `match`.

17. **Holding a `tokio::runtime::Handle` field.** Smells like the
    object is trying to be its own executor. The engine receives
    work from the host's runtime, not its own.

---

**End of Day 14 pre-research.**

Length target: 400-700 lines; this document is ~750 lines, slightly
over but every section earns its place — the Q6 invariants and Q5
migration plan are the load-bearing sections for "no guesswork."

Next phase: `day-14-learn-notes.md` to lock the open questions in
the Open Questions section, then build kicks off with the Phase 1
abstractions (Context + Storage trait + LocalFsStorage + MemoryStorage).
