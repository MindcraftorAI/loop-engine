//! Signal writer — records sentiment-derived signals on a lesson.
//!
//! Mirrors TS-side `recordLessonSentimentSignal`. Adds
//! `sentiment_positive` or `sentiment_negative` to the lesson's
//! `external_signal_sources` (idempotent Set semantics).
//!
//! Atomic write: stage into `.tmp.<pid>.<ts>`, fsync, rename. The rename
//! is atomic on Unix so a reader will see either the old file or the new
//! file, never a half-written one.
//!
//! Body normalization: Day 11 documented a one-newline-per-cycle drift
//! when load-modify-save passes through `combine_frontmatter`. To prevent
//! unbounded accumulation across many signal-emit cycles, we strip
//! leading newlines from the body before recombining — making
//! signal-write a no-op on the body bytes after the first call.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};

use crate::engine::yaml::reader::parse_lesson_frontmatter;
use crate::engine::yaml::writer::serialize_lesson_frontmatter;
use crate::engine::yaml::{combine_frontmatter, split_frontmatter_normalized};

use super::loader::{get_lesson_by_id, LoadedLesson};
use super::lock::with_lock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalPolarity {
    Positive,
    Negative,
}

impl SignalPolarity {
    pub fn signal_source(self) -> &'static str {
        match self {
            Self::Positive => "sentiment_positive",
            Self::Negative => "sentiment_negative",
        }
    }
}

/// Add a sentiment signal to a lesson's `external_signal_sources`.
/// Returns the updated lesson. Acquires an advisory flock on the lesson
/// file for the duration of the read-modify-write.
///
/// Idempotent: if the signal source is already present, the file is
/// rewritten with no change to the source set (but `updated_at` advances).
pub fn record_sentiment_signal(id: &str, polarity: SignalPolarity) -> Result<LoadedLesson> {
    let initial = get_lesson_by_id(id)?.ok_or_else(|| anyhow!("lesson not found: {id}"))?;
    let path = initial.path.clone();

    with_lock(&path, || {
        // Re-read inside the lock — the cached `initial` could be stale if
        // another process wrote since.
        let fresh = load_locked(&path, &initial.status_dir)?;
        let updated = apply_sentiment_signal(fresh, polarity)?;
        write_lesson_atomic(&updated)?;
        Ok(updated)
    })
}

fn load_locked(path: &Path, status_dir: &str) -> Result<LoadedLesson> {
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

fn apply_sentiment_signal(
    mut lesson: LoadedLesson,
    polarity: SignalPolarity,
) -> Result<LoadedLesson> {
    let source = polarity.signal_source();
    let sources = &mut lesson.frontmatter.external_signal_sources;
    if !sources.iter().any(|s| s == source) {
        sources.push(source.to_string());
    }
    lesson.frontmatter.updated_at = Some(now_iso());
    Ok(lesson)
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn write_lesson_atomic(lesson: &LoadedLesson) -> Result<()> {
    let yaml = serialize_lesson_frontmatter(&lesson.frontmatter);
    // Body-drift guard: strip the leading newlines that accumulate across
    // load-modify-save cycles via TS-compat combiner behavior (Day 11
    // documented quirk). Strip ALL leading \n so that combine_frontmatter
    // produces a stable result.
    let normalized_body = lesson.body.trim_start_matches('\n');
    let contents = combine_frontmatter(&yaml, normalized_body);

    let parent = lesson
        .path
        .parent()
        .ok_or_else(|| anyhow!("lesson path has no parent: {}", lesson.path.display()))?;
    fs::create_dir_all(parent)?;

    let tmp = staged_tmp_path(&lesson.path)?;
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("creating temp file at {}", tmp.display()))?;
        f.write_all(contents.as_bytes())?;
        f.sync_all().ok(); // best-effort; macOS APFS doesn't always honor
    }
    fs::rename(&tmp, &lesson.path).with_context(|| {
        format!(
            "atomic rename {} → {}",
            tmp.display(),
            lesson.path.display()
        )
    })?;
    Ok(())
}

fn staged_tmp_path(target: &Path) -> Result<PathBuf> {
    let stem = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("target path has no filename: {}", target.display()))?;
    let parent = target.parent().ok_or_else(|| anyhow!("no parent"))?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    Ok(parent.join(format!(".{stem}.tmp.{pid}.{ts}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::paths;
    use crate::engine::paths::ENV_LOCK;
    use crate::engine::yaml::{combine_frontmatter, LessonFrontmatter, LessonStatus};
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

    fn write_lesson(
        home: &TempDir,
        status: &str,
        id: &str,
        initial_signals: Vec<String>,
    ) -> PathBuf {
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
            external_signal_sources: initial_signals,
            promotion_eligible_at: None,
            superseded_by: None,
            superseded_at: None,
            ingest_provenance: None,
            updated_at: None,
        };
        let yaml = serialize_lesson_frontmatter(&fm);
        let contents = combine_frontmatter(&yaml, "body\n");
        let path = dir.join(format!("{id}.md"));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn errors_when_lesson_not_found() {
        with_temp_loop_home(|_| {
            let result = record_sentiment_signal("les-missing", SignalPolarity::Positive);
            assert!(result.is_err());
            Ok(())
        });
    }

    #[test]
    fn adds_sentiment_positive_to_empty_sources() {
        with_temp_loop_home(|tmp| {
            write_lesson(tmp, "active", "les-emptysig", vec![]);
            let updated = record_sentiment_signal("les-emptysig", SignalPolarity::Positive)?;
            assert_eq!(
                updated.frontmatter.external_signal_sources,
                vec!["sentiment_positive"]
            );
            assert!(updated.frontmatter.updated_at.is_some());
            Ok(())
        });
    }

    #[test]
    fn adds_sentiment_negative_with_correct_source() {
        with_temp_loop_home(|tmp| {
            write_lesson(tmp, "active", "les-negsig01", vec![]);
            let updated = record_sentiment_signal("les-negsig01", SignalPolarity::Negative)?;
            assert_eq!(
                updated.frontmatter.external_signal_sources,
                vec!["sentiment_negative"]
            );
            Ok(())
        });
    }

    #[test]
    fn preserves_existing_signal_sources() {
        with_temp_loop_home(|tmp| {
            write_lesson(
                tmp,
                "active",
                "les-existing1",
                vec!["user_thumbs_up".into()],
            );
            let updated = record_sentiment_signal("les-existing1", SignalPolarity::Positive)?;
            assert_eq!(
                updated.frontmatter.external_signal_sources,
                vec!["user_thumbs_up", "sentiment_positive"]
            );
            Ok(())
        });
    }

    #[test]
    fn idempotent_when_signal_already_present() {
        with_temp_loop_home(|tmp| {
            write_lesson(
                tmp,
                "active",
                "les-dedup001",
                vec!["sentiment_positive".into()],
            );
            let updated = record_sentiment_signal("les-dedup001", SignalPolarity::Positive)?;
            assert_eq!(
                updated.frontmatter.external_signal_sources,
                vec!["sentiment_positive"]
            );
            Ok(())
        });
    }

    /// Body-drift guard from Day 11 known limitation. Re-running the
    /// signal write multiple times should NOT accumulate leading
    /// newlines in the body.
    #[test]
    fn signal_writes_do_not_accumulate_leading_newlines_in_body() {
        with_temp_loop_home(|tmp| {
            let path = write_lesson(tmp, "active", "les-driftguard", vec![]);

            for _ in 0..5 {
                record_sentiment_signal("les-driftguard", SignalPolarity::Positive)?;
            }

            let on_disk = std::fs::read_to_string(&path)?;
            // Find the body part (after the closing `---\n`).
            let after_close = on_disk.split_once("\n---\n").map(|(_, rest)| rest).unwrap();
            let leading_newlines = after_close.chars().take_while(|&c| c == '\n').count();
            // We expect at most one leading newline (the post-delimiter
            // blank line from combine_frontmatter).
            assert!(
                leading_newlines <= 1,
                "body accumulated {leading_newlines} leading newlines after 5 cycles",
            );
            Ok(())
        });
    }

    #[test]
    fn body_content_survives_signal_writes() {
        with_temp_loop_home(|tmp| {
            let path = write_lesson(tmp, "active", "les-bodyok01", vec![]);
            // Hand-replace the body to a known multi-line content.
            let on_disk = std::fs::read_to_string(&path)?;
            let (header, _body) = on_disk.split_once("\n---\n").unwrap();
            let new_contents =
                format!("{header}\n---\n\n## Heading\n\nparagraph one\n\nparagraph two\n",);
            std::fs::write(&path, new_contents)?;

            record_sentiment_signal("les-bodyok01", SignalPolarity::Positive)?;

            let after = std::fs::read_to_string(&path)?;
            assert!(after.contains("## Heading"));
            assert!(after.contains("paragraph one"));
            assert!(after.contains("paragraph two"));
            Ok(())
        });
    }

    #[test]
    fn writer_atomic_rename_replaces_file_in_place() {
        with_temp_loop_home(|tmp| {
            let path = write_lesson(tmp, "active", "les-atomic01", vec![]);
            let original_inode = std::fs::metadata(&path).ok().map(|m| {
                use std::os::unix::fs::MetadataExt;
                m.ino()
            });
            record_sentiment_signal("les-atomic01", SignalPolarity::Positive)?;
            // After atomic rename, the inode is different — but the path
            // is the same file from the user's perspective.
            let new_inode = std::fs::metadata(&path).ok().map(|m| {
                use std::os::unix::fs::MetadataExt;
                m.ino()
            });
            assert_ne!(
                original_inode, new_inode,
                "atomic write should swap the inode"
            );
            Ok(())
        });
    }

    #[test]
    fn signal_polarity_has_correct_string_form() {
        assert_eq!(
            SignalPolarity::Positive.signal_source(),
            "sentiment_positive"
        );
        assert_eq!(
            SignalPolarity::Negative.signal_source(),
            "sentiment_negative"
        );
    }
}
