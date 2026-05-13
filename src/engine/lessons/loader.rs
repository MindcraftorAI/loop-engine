//! Lesson loader — `get_lesson_by_id` (sync, deprecated) and
//! `get_by_id` (async, post-Phase-A canonical).
//!
//! Mirrors the TS-side `core/src/lessons/loader.ts::getLessonById`:
//!   - Scans the 5 status directories in canonical order
//!   - Returns the full lesson content + frontmatter when found
//!   - Validates the lesson ID format up-front
//!
//! Phase A C4: introduces the new async `get_by_id(ctx, storage, id)`
//! signature that consumes the `Storage` trait. The legacy
//! `get_lesson_by_id(id)` stays as `#[deprecated]` for one cycle —
//! existing callers retire in Phase F or G. Each path tested
//! independently per the Phase A learn-notes scope-tightening
//! (no runtime-detection sync-wraps-async shim).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context as _, Result};

use crate::engine::context::Context;
use crate::engine::error::EngineError;
use crate::engine::paths;
use crate::engine::storage::{Storage, StorageKey};
use crate::engine::yaml::reader::parse_lesson_frontmatter;
use crate::engine::yaml::{split_frontmatter_normalized, LessonFrontmatter};

const LESSON_FILE_EXT: &str = ".md";
/// Loose ID format guard. TS side uses generateLessonId which produces
/// `les-<10-hex-ish>` style IDs; we accept anything starting with `les-`.
const LESSON_ID_PREFIX: &str = "les-";

/// Lesson plus the parent directory name (= status) it was found in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedLesson {
    pub path: PathBuf,
    pub status_dir: String,
    pub frontmatter: LessonFrontmatter,
    pub body: String,
}

/// Alias matching the TS-side `LessonFullContent` for clarity at call sites.
pub type LessonFullContent = LoadedLesson;

pub fn is_valid_lesson_id(id: &str) -> bool {
    id.starts_with(LESSON_ID_PREFIX)
        && id.len() > LESSON_ID_PREFIX.len()
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// **DEPRECATED** — use [`get_by_id`] which takes `&Context + &dyn Storage`.
///
/// Scan all status directories via `paths::loop_home()` for a lesson with
/// `id`. Returns None if not found OR invalid id. Errors only on I/O.
/// Retained for one cycle while existing callers migrate; will be
/// removed in Phase F or G.
#[deprecated(
    since = "0.0.1-phase-a",
    note = "Use `get_by_id(ctx, storage, id)` which goes through the Storage trait. \
            This sync wrapper retires in Phase F or G."
)]
pub fn get_lesson_by_id(id: &str) -> Result<Option<LoadedLesson>> {
    if !is_valid_lesson_id(id) {
        return Ok(None);
    }
    for status in paths::LESSON_STATUS_DIRS {
        let candidate = paths::lessons_status_dir(status)?.join(format!("{id}{LESSON_FILE_EXT}"));
        if !candidate.exists() {
            continue;
        }
        return Ok(Some(load_lesson_file(&candidate, status)?));
    }
    Ok(None)
}

/// Phase A C4: Storage-trait-based async lesson lookup. Canonical
/// going forward; the sync wrapper above retires in Phase F or G.
///
/// Returns `Ok(None)` on:
/// - Invalid id format (TS-parity behavior)
/// - Lesson not present in any status directory
///
/// Returns `Err(EngineError)` on:
/// - `EngineError::Storage(_)` — backend I/O error
/// - `EngineError::Parse(_)` — frontmatter split failure
/// - `EngineError::Yaml(_)` — frontmatter YAML parse failure
///
/// The returned `LoadedLesson.path` is set to a synthetic PathBuf
/// derived from the resolved StorageKey for diagnostic purposes —
/// callers should NOT rely on it being a real filesystem path (it
/// isn't, for in-memory backends).
pub async fn get_by_id(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
) -> Result<Option<LoadedLesson>, EngineError> {
    if !is_valid_lesson_id(id) {
        return Ok(None);
    }
    for status in paths::LESSON_STATUS_DIRS {
        let key = StorageKey::lesson(ctx, status, id);
        let Some(bytes) = storage.get(&key).await? else {
            continue;
        };
        let content = std::str::from_utf8(&bytes).map_err(|e| {
            EngineError::Parse(format!("non-utf8 lesson bytes for {key}: {e}"))
        })?;
        let split = split_frontmatter_normalized(content)
            .map_err(|e| EngineError::Parse(format!("split frontmatter {key}: {e}")))?;
        // anyhow::Error doesn't impl std::error::Error directly, but it
        // does impl Into<Box<dyn Error + Send + Sync>> — use the variant
        // constructor directly rather than the EngineError::yaml() helper.
        let frontmatter = parse_lesson_frontmatter(&split.yaml)
            .map_err(|e| EngineError::Yaml(e.into()))?;
        return Ok(Some(LoadedLesson {
            path: PathBuf::from(key.as_str()),
            status_dir: (*status).to_string(),
            frontmatter,
            body: split.body,
        }));
    }
    Ok(None)
}

/// Load a specific lesson file by absolute path. Used by callers that
/// already know the file location (the signal writer takes the lock
/// before reading, so it knows the path).
pub fn load_lesson_file(path: &Path, status_dir: &str) -> Result<LoadedLesson> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("reading lesson at {}", path.display()))?;
    let split = split_frontmatter_normalized(&source)
        .with_context(|| format!("splitting frontmatter for {}", path.display()))?;
    let frontmatter = parse_lesson_frontmatter(&split.yaml)
        .with_context(|| format!("parsing frontmatter for {}", path.display()))?;
    Ok(LoadedLesson {
        path: path.to_path_buf(),
        status_dir: status_dir.to_string(),
        frontmatter,
        body: split.body,
    })
}

/// Compute the canonical file path for a lesson given its id + status.
pub fn lesson_file_path(status: &str, id: &str) -> Result<PathBuf> {
    if !is_valid_lesson_id(id) {
        return Err(anyhow!("invalid lesson id: {id}"));
    }
    Ok(paths::lessons_status_dir(status)?.join(format!("{id}{LESSON_FILE_EXT}")))
}

#[cfg(test)]
mod tests {
    // Sync-path tests use the legacy with_temp_loop_home + ENV_LOCK
    // pattern — they exercise `get_lesson_by_id` which still walks
    // `paths::loop_home()`. The new `get_by_id` async path has its own
    // tests below using TestHarness (the ENV_LOCK-free pattern from
    // Phase A C3).
    #![allow(deprecated)]

    use super::*;
    use crate::engine::paths::ENV_LOCK;
    use crate::engine::yaml::{combine_frontmatter, writer::serialize_lesson_frontmatter, LessonStatus};
    use tempfile::TempDir;

    fn with_temp_loop_home<F: FnOnce(&TempDir) -> Result<()>>(f: F) {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = TempDir::new().unwrap();
        let original = std::env::var(paths::LOOP_HOME_ENV).ok();
        unsafe {
            std::env::set_var(paths::LOOP_HOME_ENV, tmp.path());
        }
        let result = f(&tmp);
        match original {
            Some(v) => unsafe { std::env::set_var(paths::LOOP_HOME_ENV, v) },
            None => unsafe { std::env::remove_var(paths::LOOP_HOME_ENV) },
        }
        result.unwrap();
    }

    fn write_minimum_lesson(home: &TempDir, status: &str, id: &str) -> PathBuf {
        let dir = home.path().join("lessons").join(status);
        std::fs::create_dir_all(&dir).unwrap();
        let fm = LessonFrontmatter {
            id: id.into(),
            description: "test lesson".into(),
            status: LessonStatus::Active,
            created_at: "2026-05-13T00:00:00.000Z".into(),
            causal_narrative: None,
            target_skill: None,
            source_feedback_ids: None,
            applied_count: 0,
            last_applied_at: None,
            thumbs_up_count: 0,
            thumbs_down_count: 0,
            external_signal_sources: vec![],
            promotion_eligible_at: None,
            superseded_by: None,
            superseded_at: None,
            ingest_provenance: None,
            updated_at: None,
        };
        let yaml = serialize_lesson_frontmatter(&fm);
        let contents = combine_frontmatter(&yaml, "test body\n");
        let path = dir.join(format!("{id}.md"));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn rejects_invalid_id_formats() {
        assert!(!is_valid_lesson_id(""));
        assert!(!is_valid_lesson_id("les-"));
        assert!(!is_valid_lesson_id("not-a-lesson"));
        assert!(!is_valid_lesson_id("les-bad/path"));
        assert!(is_valid_lesson_id("les-aaaaaaaa"));
        assert!(is_valid_lesson_id("les-dfs24ojt"));
    }

    #[test]
    fn returns_none_when_lesson_missing() {
        with_temp_loop_home(|_| {
            let result = get_lesson_by_id("les-missing")?;
            assert!(result.is_none());
            Ok(())
        });
    }

    #[test]
    fn returns_none_for_invalid_id() {
        with_temp_loop_home(|_| {
            let result = get_lesson_by_id("not-a-lesson-id")?;
            assert!(result.is_none());
            Ok(())
        });
    }

    #[test]
    fn finds_lesson_in_active_status() {
        with_temp_loop_home(|tmp| {
            write_minimum_lesson(tmp, "active", "les-aaaaaaaa");
            let loaded = get_lesson_by_id("les-aaaaaaaa")?.expect("lesson should be found");
            assert_eq!(loaded.status_dir, "active");
            assert_eq!(loaded.frontmatter.id, "les-aaaaaaaa");
            assert_eq!(loaded.frontmatter.description, "test lesson");
            Ok(())
        });
    }

    #[test]
    fn finds_lesson_in_each_status_dir() {
        for status in paths::LESSON_STATUS_DIRS {
            with_temp_loop_home(|tmp| {
                write_minimum_lesson(tmp, status, "les-pertest1");
                let loaded = get_lesson_by_id("les-pertest1")?.expect("should find in any status");
                assert_eq!(&loaded.status_dir, status);
                Ok(())
            });
        }
    }

    #[test]
    fn lesson_file_path_uses_status_dir() {
        with_temp_loop_home(|_| {
            let path = lesson_file_path("active", "les-aaaaaaaa")?;
            assert!(path
                .to_string_lossy()
                .ends_with("/lessons/active/les-aaaaaaaa.md"));
            Ok(())
        });
    }

    #[test]
    fn lesson_file_path_rejects_invalid_id() {
        with_temp_loop_home(|_| {
            let result = lesson_file_path("active", "bogus");
            assert!(result.is_err());
            Ok(())
        });
    }

    // =================================================================
    // Phase A C4 — async `get_by_id` tests via TestHarness (no ENV_LOCK)
    // =================================================================

    use crate::engine::test_support::TestHarness;
    use bytes::Bytes;

    fn lesson_yaml(id: &str, status: &str) -> String {
        format!(
            "---\n\
             id: {id}\n\
             description: \"test\"\n\
             status: {status}\n\
             created_at: \"2026-05-13T00:00:00.000Z\"\n\
             applied_count: 0\n\
             thumbs_up_count: 0\n\
             thumbs_down_count: 0\n\
             external_signal_sources: []\n\
             ---\n\
             test body\n"
        )
    }

    #[tokio::test]
    async fn get_by_id_returns_none_for_invalid_id() {
        let h = TestHarness::in_memory();
        let result = get_by_id(&h.ctx, h.storage.as_ref(), "not-a-valid-id")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn get_by_id_returns_none_when_lesson_missing() {
        let h = TestHarness::in_memory();
        let result = get_by_id(&h.ctx, h.storage.as_ref(), "les-aaaaaaaa")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn get_by_id_finds_lesson_in_active_status() {
        let h = TestHarness::in_memory();
        let key = StorageKey::lesson(&h.ctx, "active", "les-zzzzzzzz");
        h.storage
            .put(&key, Bytes::from(lesson_yaml("les-zzzzzzzz", "active")))
            .await
            .unwrap();
        let loaded = get_by_id(&h.ctx, h.storage.as_ref(), "les-zzzzzzzz")
            .await
            .unwrap()
            .expect("should be found");
        assert_eq!(loaded.status_dir, "active");
        assert_eq!(loaded.frontmatter.id, "les-zzzzzzzz");
        assert!(loaded.body.contains("test body"));
    }

    #[tokio::test]
    async fn get_by_id_finds_lesson_in_each_status_dir() {
        for status in paths::LESSON_STATUS_DIRS {
            let h = TestHarness::in_memory();
            let key = StorageKey::lesson(&h.ctx, status, "les-pertestx");
            h.storage
                .put(&key, Bytes::from(lesson_yaml("les-pertestx", status)))
                .await
                .unwrap();
            let loaded = get_by_id(&h.ctx, h.storage.as_ref(), "les-pertestx")
                .await
                .unwrap()
                .expect("should be found");
            assert_eq!(&loaded.status_dir, status);
        }
    }

    #[tokio::test]
    async fn get_by_id_on_disk_storage_works_end_to_end() {
        let h = TestHarness::on_disk();
        let key = StorageKey::lesson(&h.ctx, "active", "les-ondisk1");
        h.storage
            .put(&key, Bytes::from(lesson_yaml("les-ondisk1", "active")))
            .await
            .unwrap();
        let loaded = get_by_id(&h.ctx, h.storage.as_ref(), "les-ondisk1")
            .await
            .unwrap()
            .expect("should be found");
        assert_eq!(loaded.frontmatter.id, "les-ondisk1");
    }

    #[tokio::test]
    async fn get_by_id_returns_parse_error_on_malformed_frontmatter() {
        let h = TestHarness::in_memory();
        let key = StorageKey::lesson(&h.ctx, "active", "les-broken1");
        h.storage
            .put(&key, Bytes::from_static(b"no frontmatter at all\n"))
            .await
            .unwrap();
        let result = get_by_id(&h.ctx, h.storage.as_ref(), "les-broken1").await;
        assert!(matches!(result, Err(EngineError::Parse(_))));
    }

    /// Harness-driven tests run in parallel — proves ENV_LOCK isn't needed
    /// for the new async path (one of Phase A's design goals).
    #[tokio::test]
    async fn get_by_id_parallel_harnesses_dont_collide() {
        let (h1, h2) = (TestHarness::in_memory(), TestHarness::in_memory());
        let key1 = StorageKey::lesson(&h1.ctx, "active", "les-onlyinh1");
        h1.storage
            .put(&key1, Bytes::from(lesson_yaml("les-onlyinh1", "active")))
            .await
            .unwrap();
        // h2 has no lessons — should return None for the same id.
        assert!(get_by_id(&h2.ctx, h2.storage.as_ref(), "les-onlyinh1")
            .await
            .unwrap()
            .is_none());
        assert!(get_by_id(&h1.ctx, h1.storage.as_ref(), "les-onlyinh1")
            .await
            .unwrap()
            .is_some());
    }
}
