//! Host adapters. Consumes the engine; not consumed by it.
//!
//! Anything under `host::*` is unstable — break freely. Host adapters
//! translate host-specific event sources (JSONL transcripts, MCP RPC,
//! HTTP webhooks) into engine-shaped events, and host-specific LLM
//! APIs into engine-shaped classifier results.
//!
//! Current adapters:
//! - `claude_code` — Claude Code JSONL session watcher; (future) Anthropic
//!   Haiku classifier; (future) Auto Memory ingest.

pub mod claude_code;
