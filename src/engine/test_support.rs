//! Test fixtures: `TestHarness` for cross-cycle engine + lessons-module tests.
//!
//! Phase A C3 (Day 16b D6 / Day 17 D7): replaces the legacy
//! `with_temp_loop_home` + `ENV_LOCK` pattern. Each test gets its own
//! `Context` + `Arc<dyn Storage>` — no shared mutable env-var state,
//! tests run in parallel without serialization.
//!
//! Gated behind the `test-fixtures` Cargo feature (same gate as
//! `MockSentimentClassifier`, `MockSignalWriter`) so production builds
//! don't pull this code.
//!
//! Field ORDER MATTERS for `Drop`: `storage` must drop BEFORE `_tempdir`
//! so any `Arc<MemoryStorage>` is released cleanly before the `TempDir`
//! removes the on-disk fixture directory (S57 / S79). Declaration order
//! in Rust matches drop order; do not reorder.

// Phase A C3: `#[cfg(test)]`-only for now. When external integration
// tests need this (a future cycle), promote to `#[cfg(any(test, feature
// = "test-fixtures"))]` AND move `tempfile` from `[dev-dependencies]` to
// an optional `[dependencies]` entry gated by the feature.
#![cfg(test)]

use std::sync::Arc;

use bytes::Bytes;
use tempfile::TempDir;

use crate::engine::context::Context;
use crate::engine::error::EngineError;
use crate::engine::storage::{LocalFsStorage, MemoryStorage, Storage, StorageKey};

/// Test fixture bundling a `Context` + `Arc<dyn Storage>`. Construct via
/// [`TestHarness::in_memory`] (no filesystem) or [`TestHarness::on_disk`]
/// (TempDir-backed `LocalFsStorage`).
///
/// Both constructors are SYNC. `seed_lesson` is async (Storage methods
/// are async). Drop order: `storage → ctx → _tempdir` per declaration
/// order below — `_tempdir` LAST so any storage backed by it is fully
/// dropped before the tempdir is removed.
pub struct TestHarness {
    pub ctx: Context,
    pub storage: Arc<dyn Storage>,
    // Hold the TempDir until the harness drops. `Option<TempDir>` so
    // `in_memory` can leave it `None` without paying for an unused
    // tempdir allocation.
    _tempdir: Option<TempDir>,
}

impl TestHarness {
    /// In-memory storage — no filesystem. Fastest; use for pure-logic
    /// tests that don't care about on-disk layout.
    pub fn in_memory() -> Self {
        Self {
            ctx: Context::single_user_local(),
            storage: Arc::new(MemoryStorage::default()),
            _tempdir: None,
        }
    }

    /// `LocalFsStorage` rooted at a fresh `TempDir`. The TempDir is owned
    /// by the harness and removed on drop (RAII). Use this for tests that
    /// exercise the actual filesystem write path (CAS, atomic rename,
    /// sidecar locks).
    pub fn on_disk() -> Self {
        let tempdir = TempDir::new().expect("TestHarness::on_disk: TempDir::new failed");
        let storage = Arc::new(LocalFsStorage::new(tempdir.path()));
        Self {
            ctx: Context::single_user_local(),
            storage,
            _tempdir: Some(tempdir),
        }
    }

    /// Write a lesson into storage at `lessons/<status>/<id>.md` with
    /// minimal valid YAML frontmatter + the provided `body`. Returns
    /// the `StorageKey` of the written lesson per Phase A D6 (OQ-A5).
    ///
    /// The generated frontmatter has the required fields (`id`, `description`,
    /// `status`, `created_at`) populated; counters default to 0. Tests
    /// that need richer frontmatter should call `storage.put` directly
    /// with their own YAML.
    pub async fn seed_lesson(
        &self,
        status: &str,
        id: &str,
        body: &str,
    ) -> Result<StorageKey, EngineError> {
        let key = StorageKey::lesson(&self.ctx, status, id);
        let content = format!(
            "---\n\
             id: {id}\n\
             description: \"seeded by TestHarness\"\n\
             status: {status}\n\
             created_at: \"2026-05-13T00:00:00Z\"\n\
             applied_count: 0\n\
             thumbs_up_count: 0\n\
             thumbs_down_count: 0\n\
             external_signal_sources: []\n\
             ---\n\
             {body}\n"
        );
        self.storage
            .put(&key, Bytes::from(content))
            .await
            .map_err(EngineError::from)?;
        Ok(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_constructor_is_synchronous_and_works() {
        let harness = TestHarness::in_memory();
        assert_eq!(harness.ctx.tenant_id.as_str(), "local");
        // Storage is functional.
        let key = harness
            .seed_lesson("active", "les-mem-1", "body content")
            .await
            .unwrap();
        let stored = harness.storage.get(&key).await.unwrap().unwrap();
        let body = std::str::from_utf8(&stored).unwrap();
        assert!(body.contains("id: les-mem-1"));
        assert!(body.contains("status: active"));
        assert!(body.contains("body content"));
    }

    #[tokio::test]
    async fn on_disk_constructor_creates_tempdir() {
        let harness = TestHarness::on_disk();
        let key = harness
            .seed_lesson("active", "les-disk-1", "disk-body")
            .await
            .unwrap();
        let stored = harness.storage.get(&key).await.unwrap().unwrap();
        let body = std::str::from_utf8(&stored).unwrap();
        assert!(body.contains("id: les-disk-1"));
        // _tempdir is set on the on_disk variant.
        assert!(harness._tempdir.is_some());
    }

    #[tokio::test]
    async fn harnesses_are_independent_across_tests() {
        // Two harnesses created back-to-back must not see each other's
        // lessons — proves no shared global state (ENV_LOCK-free).
        let a = TestHarness::in_memory();
        let b = TestHarness::in_memory();
        a.seed_lesson("active", "les-only-in-a", "x")
            .await
            .unwrap();
        let key_b = StorageKey::lesson(&b.ctx, "active", "les-only-in-a");
        // b's storage is a separate MemoryStorage — should not have it.
        assert!(b.storage.get(&key_b).await.unwrap().is_none());
    }
}
