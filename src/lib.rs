//! `loop-daemon` library root.
//!
//! Exposes daemon internals for integration testing. The binary itself
//! lives in `src/main.rs`.

pub mod buffer;
pub mod cli;
pub mod config;
pub mod lifecycle;
pub mod observability;
pub mod paths;
pub mod pid;
