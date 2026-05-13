//! `loop-engine`: host-agnostic cognitive memory engine.
//!
//! Anything `pub` in this module tree is part of the stable engine API
//! contract (the "to-be-extracted-as-loop-engine" surface). Anything
//! `pub(crate)` is internal plumbing — engine internals that adapter
//! crates have no business reaching into.
//!
//! Boundary contract (enforced by lint, not type system):
//! - Code under `engine::*` MUST NOT reference `crate::host::*`.
//! - Code under `host::*` MAY freely use `engine::*`.
//! - CI grep verifies this. See [[feedback-workflow-cycle]].

pub mod buffer;
pub mod context;
pub mod error;
pub mod events;
pub mod lessons;
pub mod lifecycle;
pub mod paths;
pub mod pid;
pub mod sentiment;
pub mod storage;
pub mod yaml;

pub use error::EngineError;

// Curated re-exports (engine prelude).
pub use context::{Context, ContextBuilder, SessionId, TeamId, TenantId, UserId};
pub use events::{EngineEvent, EventSource, EventSourceError, HostVersion, ProjectTag};
pub use storage::{LocalFsStorage, MemoryStorage, Storage, StorageError, StorageKey, Version};
