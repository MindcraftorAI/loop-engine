//! Phase E2 — `derived_from` cycle + depth detection.
//!
//! Phase E2 Cx1 ships a stub `detect_cycle_in_window` that always
//! returns Ok(()) — the compression core needs SOMETHING to call,
//! but the full cycle-walk implementation (D-Cx8) lands in Cx2 with
//! the chase-helper. Cx2 replaces this stub.

use crate::engine::context::Context;
use crate::engine::error::EngineError;
use crate::engine::memory::Memory;
use crate::engine::storage::Storage;

/// Walk each predecessor's `derived_from` chain back, detect cycles
/// + cap depth at [`super::compress::COMPRESSION_MAX_CHAIN_DEPTH`].
///
/// **Cx1 stub** — returns Ok(()) unconditionally. Full impl in Cx2.
/// Tests covering cycle/depth detection ship with Cx2.
#[allow(unused_variables)] // Cx2 wires the real impl
pub(crate) async fn detect_cycle_in_window(
    ctx: &Context,
    storage: &dyn Storage,
    predecessors: &[Memory],
) -> Result<(), EngineError> {
    Ok(())
}
