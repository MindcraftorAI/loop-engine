//! Claude Code host adapter.
//!
//! Bridges the Claude Code transcript surface into engine-shaped events.
//! Lives entirely under `host::*` and is free to depend on Claude
//! Code-specific encodings (JSONL schema, encoded cwd paths, sidechain
//! filtering rules, Anthropic API shapes).

// Unix-only: the watcher uses `std::os::unix::fs::MetadataExt::ino()`
// for atomic-rename rotation detection — Windows file IDs work
// differently and a faithful port needs `nt_file_index` + a Windows-
// specific rotation strategy. Out-of-scope for v0.5 since the watcher
// is only ever spawned alongside the detached daemon (also Unix-only,
// cfg-gated in main.rs). Windows binary ships without the watcher.
#[cfg(unix)]
pub mod jsonl_watcher;
