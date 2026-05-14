//! Concurrency test for record_sentiment_signal (legacy sync wrapper).
//!
//! Spawns multiple threads that each call record_sentiment_signal on the
//! SAME lesson concurrently. Verifies:
//!   - All threads succeed (no lock contention errors)
//!   - The final file has both expected signal sources (no lost updates)
//!   - The body has not accumulated leading newlines
//!
//! This simulates the cross-process race shape: the daemon may be writing
//! at the same instant the TS MCP server is. fd_lock is process-level, so
//! threads here exercise the same code path. A real two-process test
//! would need a separate binary; this is the in-crate equivalent.
//!
//! Phase A C6: `record_sentiment_signal` is `#[deprecated]` — this
//! legacy concurrency test stays as a backward-compat regression. The
//! async equivalent (`record_signal` with bounded CAS) has its own
//! parallel-safety test in `engine::lessons::signals` (Phase A C5).

#![allow(deprecated)]

use std::sync::{Mutex, OnceLock};
use std::thread;

use loop_engine::lessons::{record_sentiment_signal, SignalPolarity};
use loop_engine::paths;
use loop_engine::yaml::writer::serialize_lesson_frontmatter;
use loop_engine::yaml::{combine_frontmatter, LessonFrontmatter, LessonStatus};
use tempfile::TempDir;

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn write_initial_lesson(home: &TempDir, id: &str) -> std::path::PathBuf {
    let dir = home.path().join("lessons").join("active");
    std::fs::create_dir_all(&dir).unwrap();
    let fm = LessonFrontmatter {
        id: id.into(),
        description: "concurrency test lesson".into(),
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
        authored_by: Default::default(),
        updated_at: None,
    };
    let yaml = serialize_lesson_frontmatter(&fm);
    let contents = combine_frontmatter(&yaml, "test body\n");
    let path = dir.join(format!("{id}.md"));
    std::fs::write(&path, contents).unwrap();
    path
}

#[test]
fn concurrent_signal_writes_to_same_lesson_dont_lose_updates() {
    let _g = env_lock().lock().unwrap_or_else(|p| p.into_inner());
    let tmp = TempDir::new().unwrap();
    let original = std::env::var(paths::LOOP_HOME_ENV).ok();
    unsafe {
        std::env::set_var(paths::LOOP_HOME_ENV, tmp.path());
    }

    let lesson_path = write_initial_lesson(&tmp, "les-concurrent");

    let handles: Vec<_> = (0..8)
        .map(|i| {
            thread::spawn(move || {
                let polarity = if i % 2 == 0 {
                    SignalPolarity::Positive
                } else {
                    SignalPolarity::Negative
                };
                record_sentiment_signal("les-concurrent", polarity)
            })
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // Restore env BEFORE assertions so we don't leak state on failure.
    match original {
        Some(v) => unsafe { std::env::set_var(paths::LOOP_HOME_ENV, v) },
        None => unsafe { std::env::remove_var(paths::LOOP_HOME_ENV) },
    }

    // All writes succeeded
    for r in &results {
        assert!(
            r.is_ok(),
            "concurrent record_sentiment_signal failed: {r:?}"
        );
    }

    // Final file has BOTH signal sources (no lost updates)
    let final_contents = std::fs::read_to_string(&lesson_path).unwrap();
    assert!(
        final_contents.contains("sentiment_positive"),
        "lost sentiment_positive after concurrent writes\n{final_contents}"
    );
    assert!(
        final_contents.contains("sentiment_negative"),
        "lost sentiment_negative after concurrent writes\n{final_contents}"
    );

    // Each signal source should appear EXACTLY ONCE (idempotent Set semantics)
    let pos_count = final_contents.matches("sentiment_positive").count();
    let neg_count = final_contents.matches("sentiment_negative").count();
    assert_eq!(pos_count, 1, "sentiment_positive appears {pos_count} times");
    assert_eq!(neg_count, 1, "sentiment_negative appears {neg_count} times");

    // Body did not accumulate newlines under contention
    let after_close = final_contents
        .split_once("\n---\n")
        .map(|(_, body)| body)
        .unwrap();
    let leading_newlines = after_close.chars().take_while(|&c| c == '\n').count();
    assert!(
        leading_newlines <= 1,
        "body has {leading_newlines} leading newlines after 8 concurrent writes",
    );
}
