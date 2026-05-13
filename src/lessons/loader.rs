//! Lesson loader — `get_lesson_by_id` and helpers.
//!
//! Mirrors the TS-side `core/src/lessons/loader.ts::getLessonById`:
//!   - Scans the 5 status directories in canonical order
//!   - Returns the full lesson content + frontmatter when found
//!   - Validates the lesson ID format up-front

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::paths;
use crate::yaml::reader::parse_lesson_frontmatter;
use crate::yaml::{split_frontmatter_normalized, LessonFrontmatter};

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

/// Scan all status directories for a lesson with `id`. Returns None if
/// not found. Errors only on I/O issues, not on missing.
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
    use super::*;
    use crate::paths::ENV_LOCK;
    use crate::yaml::{combine_frontmatter, writer::serialize_lesson_frontmatter, LessonStatus};
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
}
