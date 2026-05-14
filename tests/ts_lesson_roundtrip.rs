//! Integration test: round-trip a real lesson file written by the
//! TypeScript side of Loop. The bytes that come back out of the Rust
//! writer must parse the same field-set as the original.
//!
//! Run only when LOOP_HOME is set and contains lessons (skipped
//! otherwise so CI without a real Loop install still passes).

use std::fs;
use std::path::PathBuf;

use loop_engine::yaml::reader::parse_lesson_frontmatter;
use loop_engine::yaml::writer::serialize_lesson_frontmatter;
use loop_engine::yaml::{combine_frontmatter, split_frontmatter};

fn locate_real_lesson() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let active = home.join(".loop").join("lessons").join("active");
    if !active.exists() {
        return None;
    }
    fs::read_dir(active).ok()?.find_map(|entry| {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            Some(path)
        } else {
            None
        }
    })
}

#[test]
fn real_ts_written_lesson_parses_and_roundtrips_field_set() {
    let Some(path) = locate_real_lesson() else {
        eprintln!("skipping: no real lesson found at ~/.loop/lessons/active/");
        return;
    };
    let source = fs::read_to_string(&path).expect("read lesson file");
    let split = split_frontmatter(&source).expect("split frontmatter");
    let fm = parse_lesson_frontmatter(split.yaml).expect("parse frontmatter");

    // Round-trip 1: serialize the frontmatter back, parse again, assert
    // FIELD-SET equality. This is the load-bearing property — Rust
    // produces YAML that, when re-parsed, gives back the same fields.
    let reserialized_yaml = serialize_lesson_frontmatter(&fm);
    let fm2 = parse_lesson_frontmatter(&reserialized_yaml).expect("parse our own output");
    assert_eq!(fm, fm2, "field-set must be stable across Rust round-trip");

    // Round-trip 2: combine + split again. Body content is preserved
    // verbatim apart from the known one-newline-per-cycle TS-compat
    // quirk documented in `combine_frontmatter`. So we assert the body
    // CONTAINS the original body's content (modulo leading whitespace),
    // not byte-equality.
    let combined = combine_frontmatter(&reserialized_yaml, split.body);
    let split2 = split_frontmatter(&combined).expect("split combined output");
    let original_body_trimmed = split.body.trim_start_matches('\n');
    let new_body_trimmed = split2.body.trim_start_matches('\n');
    assert_eq!(
        new_body_trimmed, original_body_trimmed,
        "body content must round-trip unchanged (apart from leading newlines)"
    );
}
