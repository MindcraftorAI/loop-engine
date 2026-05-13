//! `loop-daemon` library root.
//!
//! Two consumer layers:
//! - [`engine`] — host-agnostic. The "to-be-extracted-as-loop-engine"
//!   surface. Anything `pub` inside is part of the stable engine API.
//! - [`host`] — host-specific adapters. Unstable; break freely.
//!
//! Top-level binary glue (`cli`, `config`, `observability`) is not part
//! of the engine surface and is not re-exported.
//!
//! Boundary contract: code under `engine::*` may not reference
//! `crate::host::*`. Enforced by lint + CI grep, not the type system
//! (cheaper, single compile unit). See `docs/research/day-14-learn-notes.md`.

pub mod engine;
pub mod host;

// Top-level binary modules (not part of the engine surface):
pub mod cli;
pub mod config;
pub mod observability;

// Backward-compat re-exports (Day 14 Phase 1 — delegating wrappers).
//
// External callers — integration tests, future external consumers — can
// continue using `loop_daemon::lessons::*` etc. while the underlying
// modules live at `loop_daemon::engine::lessons::*`. Phase 2 callers will
// migrate to the explicit `engine::*` path and these re-exports retire.
pub use engine::{buffer, lessons, lifecycle, paths, pid, yaml};
