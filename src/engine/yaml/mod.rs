//! Purpose-built YAML reader/writer for Loop lesson frontmatter.
//!
//! Scope: the narrow YAML shape Loop's lessons use. NOT a general YAML
//! library. Sidesteps the serde_yaml deprecation (the upstream is
//! archived; replacements have weak round-trip control).
//!
//! Approach (Day 11 audit notes welcome here):
//!   - Reader (`reader.rs`) — wraps `serde_yml` for parsing. Deserialization
//!     is well-trusted; the deprecation concern is maintenance, not
//!     correctness on already-shipped versions.
//!   - Writer (`writer.rs`) — hand-rolled to match the TS-side `yaml`
//!     library's output byte-for-byte given identical inputs:
//!     `blockQuote: 'literal', lineWidth: 0, defaultStringType: 'PLAIN',
//!     defaultKeyType: 'PLAIN'`. The Phase 2 audit A3 finding (multi-paragraph
//!     causal narrative refold) is the reason we don't want format drift.
//!
//! Both processes (Rust daemon + TS MCP server) read/write the same
//! lesson files. Round-trip parity is the load-bearing property here.

mod frontmatter;
pub mod reader;
mod scalar;
mod schema;
pub mod writer;

pub use frontmatter::{
    combine_frontmatter, split_frontmatter, split_frontmatter_normalized, FrontmatterSplit,
    OwnedFrontmatterSplit,
};
pub use schema::{
    CausalNarrative, IngestProvenance, IngestSourceType, LessonFrontmatter, LessonStatus,
};
