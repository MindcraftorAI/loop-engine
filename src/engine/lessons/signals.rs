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
//!
//! Phase A C5: NEW async `record_signal(&Context, &dyn Storage, id,
//! polarity)` with bounded 5-retry CAS loop using
//! `Storage::put_if_version`. The legacy sync `record_sentiment_signal`
//! stays `#[deprecated]` for one cycle. The CAS loop **re-reads the
//! lesson on every iteration** (no stale-bytes reuse) and never holds
//! any locks across `.await` points. Caller-visible budget exhaustion
//! surfaces as `EngineError::CasContended { key, retries: 5 }`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context as _, Result};
use bytes::Bytes;

use crate::engine::context::Context;
use crate::engine::error::EngineError;
use crate::engine::storage::{Storage, StorageKey};
use crate::engine::yaml::reader::parse_lesson_frontmatter;
use crate::engine::yaml::writer::serialize_lesson_frontmatter;
use crate::engine::yaml::{combine_frontmatter, split_frontmatter_normalized};

#[allow(deprecated)] // sync wrapper deliberately keeps the deprecated import
use super::loader::get_lesson_by_id;
use super::loader::{get_by_id, LoadedLesson};
#[allow(deprecated)] // sync wrapper still consumes the deprecated lock helper
use super::lock::with_lock;

/// Phase A C5: bounded retry budget for the CAS loop in `record_signal`.
/// 5 retries is enough to absorb realistic cross-process contention in
/// single-user mode; exhaustion surfaces as `EngineError::CasContended`.
const RECORD_SIGNAL_MAX_RETRIES: u32 = 5;

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

/// **DEPRECATED** — use [`record_signal`] which takes `&Context + &dyn Storage`.
///
/// Add a sentiment signal to a lesson's `external_signal_sources`.
/// Returns the updated lesson. Acquires an advisory flock on the lesson
/// file for the duration of the read-modify-write.
///
/// Idempotent: if the signal source is already present, the file is
/// rewritten with no change to the source set (but `updated_at` advances).
///
/// Phase A C5: retained for one cycle while existing callers migrate.
/// Retires in Phase F or G.
#[deprecated(
    since = "0.0.1-phase-a",
    note = "Use `record_signal(ctx, storage, id, polarity)` — Storage::put_if_version CAS path"
)]
pub fn record_sentiment_signal(id: &str, polarity: SignalPolarity) -> Result<LoadedLesson> {
    #[allow(deprecated)]
    let initial = get_lesson_by_id(id)?.ok_or_else(|| anyhow!("lesson not found: {id}"))?;
    let path = initial.path.clone();

    #[allow(deprecated)]
    with_lock(&path, || {
        let fresh = load_locked(&path, &initial.status_dir)?;
        let updated = apply_sentiment_signal(fresh, polarity)?;
        write_lesson_atomic(&updated)?;
        Ok(updated)
    })
}

/// Phase A C5: Storage-trait-based async signal recorder with bounded
/// 5-retry CAS loop. Idempotent: if the signal source is already in
/// `external_signal_sources`, no-op on the set but `updated_at` advances.
///
/// **Async-safety discipline (per 2026-05-13 user reminder):**
/// - Re-reads `(bytes, version)` on EVERY iteration. No stale-bytes reuse.
/// - No locks held across `.await` — `Storage::put_if_version` handles
///   cross-process serialization internally via sidecar flock.
/// - Bounded retry: exits with `EngineError::CasContended` after
///   `RECORD_SIGNAL_MAX_RETRIES` (5) failed compare-and-swaps.
/// - Lesson location (status_dir) is captured ONCE up-front so a
///   concurrent supersession/discard doesn't redirect the write.
///
/// Returns:
/// - `Ok(LoadedLesson)` on success (the updated lesson). The returned
///   `LoadedLesson.path` is a SYNTHETIC `PathBuf` derived from the
///   resolved `StorageKey` (matches `get_by_id`'s contract). It is
///   NOT a real filesystem path for in-memory backends — callers must
///   not pass it to `std::fs` ops; use Storage trait methods instead.
/// - `Err(EngineError::LessonNotFound { id })` if the lesson is absent
///   at any point during the CAS loop. This includes BOTH the deletion
///   case (lesson removed via discard) AND the moved-to-different-status
///   case (lesson was in `active/` when we started, now in `archived/`
///   — `get_with_version` on the original key returns None).
/// - `Err(EngineError::CasContended { key, retries })` on retry-budget
///   exhaustion (5 failed CAS attempts in a row).
/// - `Err(EngineError::Storage(_)/Parse(_)/Yaml(_))` on read/parse failures
///   from the Storage backend or YAML pipeline.
///
/// Phase A audit M1/M2 doc clarifications (2026-05-13).
pub async fn record_signal(
    ctx: &Context,
    storage: &dyn Storage,
    id: &str,
    polarity: SignalPolarity,
) -> Result<LoadedLesson, EngineError> {
    // Find the lesson + its status dir once. If a concurrent writer
    // moves the lesson to a different status mid-CAS, the put_if_version
    // version-mismatch will reject our write — that's the intended
    // safety behavior, not a bug.
    let initial = get_by_id(ctx, storage, id)
        .await?
        .ok_or_else(|| EngineError::LessonNotFound { id: id.to_string() })?;
    let status_dir = initial.status_dir.clone();
    let key = StorageKey::lesson(ctx, &status_dir, id);

    for _attempt in 0..RECORD_SIGNAL_MAX_RETRIES {
        // RE-READ on every iteration. Pre-research D1 + user-stated
        // async caution.
        let Some((bytes, version)) = storage.get_with_version(&key).await? else {
            // Lesson deleted between iterations.
            return Err(EngineError::LessonNotFound { id: id.to_string() });
        };

        let content = std::str::from_utf8(&bytes)
            .map_err(|e| EngineError::Parse(format!("non-utf8 lesson bytes for {key}: {e}")))?;
        let split = split_frontmatter_normalized(content)
            .map_err(|e| EngineError::Parse(format!("split frontmatter {key}: {e}")))?;
        let mut frontmatter = parse_lesson_frontmatter(&split.yaml)
            .map_err(|e| EngineError::Yaml(e.into()))?;

        // Idempotent: only push if not present.
        let source = polarity.signal_source();
        if !frontmatter
            .external_signal_sources
            .iter()
            .any(|s| s == source)
        {
            frontmatter.external_signal_sources.push(source.to_string());
        }
        frontmatter.updated_at = Some(now_iso());

        // Reserialize with body-drift guard.
        let new_yaml = serialize_lesson_frontmatter(&frontmatter);
        let normalized_body = split.body.trim_start_matches('\n');
        let new_contents = combine_frontmatter(&new_yaml, normalized_body);

        // CAS: write only if the on-disk version still matches what we
        // just read. On mismatch, loop and retry from scratch.
        let written = storage
            .put_if_version(&key, Bytes::from(new_contents), Some(&version))
            .await?;
        if written {
            return Ok(LoadedLesson {
                path: PathBuf::from(key.as_str()),
                status_dir,
                frontmatter,
                body: normalized_body.to_string(),
            });
        }
        // CAS lost the race — another writer beat us. Loop and re-read.
    }

    Err(EngineError::CasContended {
        key: key.as_str().to_string(),
        retries: RECORD_SIGNAL_MAX_RETRIES,
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
    // Sync-path tests use the legacy with_temp_loop_home + ENV_LOCK
    // pattern; they exercise `record_sentiment_signal` which is now
    // deprecated. New `record_signal` async path tests below use
    // TestHarness (Phase A C3) for ENV_LOCK-free parallel execution.
    #![allow(deprecated)]

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

    // =================================================================
    // Phase A C5 — async record_signal tests via TestHarness
    // =================================================================

    use crate::engine::test_support::TestHarness;
    use bytes::Bytes as _Bytes;

    fn seeded_lesson_yaml(id: &str) -> String {
        format!(
            "---\n\
             id: {id}\n\
             description: \"test\"\n\
             status: active\n\
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
    async fn record_signal_errors_when_lesson_not_found() {
        let h = TestHarness::in_memory();
        let result = record_signal(
            &h.ctx,
            h.storage.as_ref(),
            "les-nofile99",
            SignalPolarity::Positive,
        )
        .await;
        assert!(matches!(
            result,
            Err(EngineError::LessonNotFound { ref id }) if id == "les-nofile99"
        ));
    }

    #[tokio::test]
    async fn record_signal_adds_sentiment_positive_to_empty_sources() {
        let h = TestHarness::in_memory();
        let key = StorageKey::lesson(&h.ctx, "active", "les-emptyas1");
        h.storage
            .put(&key, _Bytes::from(seeded_lesson_yaml("les-emptyas1")))
            .await
            .unwrap();

        let updated = record_signal(
            &h.ctx,
            h.storage.as_ref(),
            "les-emptyas1",
            SignalPolarity::Positive,
        )
        .await
        .unwrap();
        assert_eq!(
            updated.frontmatter.external_signal_sources,
            vec!["sentiment_positive".to_string()]
        );
        assert!(updated.frontmatter.updated_at.is_some());
    }

    #[tokio::test]
    async fn record_signal_idempotent_when_already_present() {
        let h = TestHarness::in_memory();
        let key = StorageKey::lesson(&h.ctx, "active", "les-dedupasy");
        h.storage
            .put(&key, _Bytes::from(seeded_lesson_yaml("les-dedupasy")))
            .await
            .unwrap();

        // First write adds it.
        let _ = record_signal(
            &h.ctx,
            h.storage.as_ref(),
            "les-dedupasy",
            SignalPolarity::Positive,
        )
        .await
        .unwrap();
        // Second write is idempotent (no second push).
        let updated = record_signal(
            &h.ctx,
            h.storage.as_ref(),
            "les-dedupasy",
            SignalPolarity::Positive,
        )
        .await
        .unwrap();
        assert_eq!(
            updated.frontmatter.external_signal_sources,
            vec!["sentiment_positive".to_string()]
        );
    }

    #[tokio::test]
    async fn record_signal_adds_negative_with_correct_source() {
        let h = TestHarness::in_memory();
        let key = StorageKey::lesson(&h.ctx, "active", "les-negasync");
        h.storage
            .put(&key, _Bytes::from(seeded_lesson_yaml("les-negasync")))
            .await
            .unwrap();
        let updated = record_signal(
            &h.ctx,
            h.storage.as_ref(),
            "les-negasync",
            SignalPolarity::Negative,
        )
        .await
        .unwrap();
        assert_eq!(
            updated.frontmatter.external_signal_sources,
            vec!["sentiment_negative".to_string()]
        );
    }

    #[tokio::test]
    async fn record_signal_preserves_existing_sources() {
        let h = TestHarness::in_memory();
        let key = StorageKey::lesson(&h.ctx, "active", "les-presrvsg");
        let yaml = "---\n\
             id: les-presrvsg\n\
             description: \"test\"\n\
             status: active\n\
             created_at: \"2026-05-13T00:00:00.000Z\"\n\
             applied_count: 0\n\
             thumbs_up_count: 0\n\
             thumbs_down_count: 0\n\
             external_signal_sources: [\"user_thumbs_up\"]\n\
             ---\n\
             body\n";
        h.storage.put(&key, _Bytes::from(yaml)).await.unwrap();
        let updated = record_signal(
            &h.ctx,
            h.storage.as_ref(),
            "les-presrvsg",
            SignalPolarity::Positive,
        )
        .await
        .unwrap();
        assert_eq!(
            updated.frontmatter.external_signal_sources,
            vec!["user_thumbs_up".to_string(), "sentiment_positive".to_string()]
        );
    }

    /// CAS-loop correctness: 5 consecutive writes succeed with a fresh
    /// version on each iteration. (No retry budget exhaustion at 5
    /// SUCCESSFUL writes — each one bumps the version, then the next
    /// iteration reads the new version and CASes on it.)
    #[tokio::test]
    async fn record_signal_handles_repeated_calls() {
        let h = TestHarness::in_memory();
        let key = StorageKey::lesson(&h.ctx, "active", "les-repeatsg");
        h.storage
            .put(&key, _Bytes::from(seeded_lesson_yaml("les-repeatsg")))
            .await
            .unwrap();
        for _ in 0..5 {
            let _ = record_signal(
                &h.ctx,
                h.storage.as_ref(),
                "les-repeatsg",
                SignalPolarity::Positive,
            )
            .await
            .unwrap();
        }
        // Final state still has just one source (idempotent set).
        let final_loaded = get_by_id(&h.ctx, h.storage.as_ref(), "les-repeatsg")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            final_loaded.frontmatter.external_signal_sources,
            vec!["sentiment_positive".to_string()]
        );
    }

    /// Parallel harnesses don't share state — proves the async path
    /// doesn't depend on ENV_LOCK.
    #[tokio::test]
    async fn record_signal_parallel_harnesses_isolated() {
        let (h1, h2) = (TestHarness::in_memory(), TestHarness::in_memory());
        for h in [&h1, &h2] {
            let key = StorageKey::lesson(&h.ctx, "active", "les-parallel");
            h.storage
                .put(&key, _Bytes::from(seeded_lesson_yaml("les-parallel")))
                .await
                .unwrap();
        }
        let r1 = record_signal(
            &h1.ctx,
            h1.storage.as_ref(),
            "les-parallel",
            SignalPolarity::Positive,
        );
        let r2 = record_signal(
            &h2.ctx,
            h2.storage.as_ref(),
            "les-parallel",
            SignalPolarity::Negative,
        );
        let (r1, r2) = tokio::join!(r1, r2);
        let r1 = r1.unwrap();
        let r2 = r2.unwrap();
        assert_eq!(
            r1.frontmatter.external_signal_sources,
            vec!["sentiment_positive".to_string()]
        );
        assert_eq!(
            r2.frontmatter.external_signal_sources,
            vec!["sentiment_negative".to_string()]
        );
    }

    /// On-disk storage exercises the real put_if_version CAS path
    /// (sidecar flock + atomic rename) under spawn_blocking.
    #[tokio::test]
    async fn record_signal_on_disk_end_to_end() {
        let h = TestHarness::on_disk();
        let key = StorageKey::lesson(&h.ctx, "active", "les-ondsk001");
        h.storage
            .put(&key, _Bytes::from(seeded_lesson_yaml("les-ondsk001")))
            .await
            .unwrap();
        let updated = record_signal(
            &h.ctx,
            h.storage.as_ref(),
            "les-ondsk001",
            SignalPolarity::Positive,
        )
        .await
        .unwrap();
        assert_eq!(
            updated.frontmatter.external_signal_sources,
            vec!["sentiment_positive".to_string()]
        );
    }
}
