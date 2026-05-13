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
pub mod lessons;
pub mod lifecycle;
pub mod paths;
pub mod pid;
pub mod yaml;

// Day 14 Phase 2 (planned, lands when callers migrate):
// pub mod context;
// pub mod storage;
// pub mod events;
