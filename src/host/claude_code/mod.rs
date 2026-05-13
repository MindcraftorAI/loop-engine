//! Claude Code host adapter.
//!
//! Bridges the Claude Code transcript surface into engine-shaped events.
//! Lives entirely under `host::*` and is free to depend on Claude
//! Code-specific encodings (JSONL schema, encoded cwd paths, sidechain
//! filtering rules, Anthropic API shapes).

pub mod jsonl_watcher;
