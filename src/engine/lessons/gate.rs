//! Promotion gate — the marketing wedge.
//!
//! Pure synchronous check: given a lesson's frontmatter + filesystem-level
//! `StorageMetadata` (from Phase B C-B1) + a `PromotionConfig` + the
//! current time, decide whether the lesson is eligible for promotion.
//! Returns a `GateDecision` enumerating ALL violations rather than
//! first-failing, so callers (CLI, future transitions::promote, MCP
//! tool surface) can render the complete picture in one shot.
//!
//! The wedge against Anthropic Dreaming + Auto Memory: those layers can
//! self-grade promotion ("I think this lesson is good"). The gate refuses
//! to take frontmatter at face value:
//!   - `TamperedAge`: birthtime > frontmatter `created_at` ⇒ backdating
//!     detected. A self-grading agent can't outsmart the filesystem.
//!   - `ObservedConfidenceWithoutEvidenceRefs`: "observed" without
//!     pointers is a self-grading claim, not evidence.
//!   - `MissingExternalSignalSources`: promotion requires at least one
//!     external signal (thumbs-up from a separate process, capture from
//!     auto-memory, etc) — internal narrative alone is insufficient.
//!   - `ThumbsDownBlock`: a single thumbs-down hard-blocks promotion no
//!     matter how many thumbs-ups accumulated.
//!
//! Phase B C-B2. Side-effect free per learn-notes D5 — status transition
//! lands in Phase G (`transitions::promote`). `EngineError::PromotionBlocked`
//! is added preemptively so Phase G has a typed failure to raise.

use std::fmt;

use chrono::{DateTime, Duration, Utc};

use crate::engine::storage::StorageMetadata;
use crate::engine::yaml::{Confidence, LessonFrontmatter, LessonStatus};

/// Configuration knobs for the promotion gate. Defaults match the TS
/// reference implementation; hosts can override via [`Default`] + struct
/// update or by deserializing a `PromotionConfigYaml` (Phase E).
///
/// `#[non_exhaustive]` so future cycles can add knobs (e.g.
/// `min_thumbs_up`, `min_applied_count`) without breaking SemVer.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PromotionConfig {
    /// Minimum age before a lesson can be promoted. Time-floor sieve
    /// per learn-notes D2 (`BlockReason::TimeFloor`). Default 24 hours
    /// matches the TS implementation; rationale is "a lesson should
    /// survive at least one full day of real use before promotion."
    pub min_age: Duration,
    /// Minimum `applied_count` before a lesson can be promoted —
    /// usage-volume floor. Default 3 matches the TS implementation
    /// (`DEFAULT_MIN_APPLIED_COUNT` in `core-ts/src/lessons/gate.ts`).
    /// Wedge rationale: a lesson must have been USED N times before
    /// the system trusts it; self-grading agents can't pump the
    /// applied_count without external signal sources (which is its
    /// own gate via [`BlockReason::MissingExternalSignalSources`]).
    pub min_applied_count: u64,
    /// Allowed clock skew when comparing frontmatter `created_at` to
    /// filesystem birthtime. If `birthtime - frontmatter_created_at`
    /// exceeds this, fire `BlockReason::TamperedAge`. Default 60s
    /// covers normal NTP drift; tampering tends to be hours/days off.
    /// (Pre-research suggested 1s; bumped to 60s based on real-world
    /// NTP-drift envelope. The 60x increase still catches malicious
    /// minute-scale-or-bigger backdating.)
    pub tamper_skew_tolerance: Duration,
    /// Phase G D-G3 (v0.4): minimum number of distinct sessions that
    /// must have applied this lesson before promotion. Counts
    /// `LessonFrontmatter::applied_session_ids.len()`. Default `0`
    /// (gate disabled — pre-Phase-2 lessons have no session data).
    /// Set to `>=2` to require multi-session reproducibility — the
    /// `origin_diverse` wedge signal that makes self-grading harder
    /// (a single agent in one session can pump applied_count without
    /// touching multiple distinct session_ids).
    pub min_distinct_origins: u32,
}

impl Default for PromotionConfig {
    fn default() -> Self {
        Self {
            min_age: Duration::hours(24),
            min_applied_count: 3,
            tamper_skew_tolerance: Duration::seconds(60),
            // Disabled by default — Phase 2 hooks ship the recording
            // half. Once hosts populate `applied_session_ids` we can
            // raise the floor (likely 2-3 in v0.5).
            min_distinct_origins: 0,
        }
    }
}

/// Phase G D-G3 (v0.4): advisory cap on distinct session_ids tracked
/// per lesson in `applied_session_ids`. Bounds frontmatter growth for
/// lessons applied across many sessions.
///
/// **Enforcement is host-responsibility, not engine-side.** The engine
/// reads `applied_session_ids` (e.g. via `derive_origin_diverse` and
/// the gate's 5b check) but does NOT mutate it on update — the
/// recording path lands in Phase 2 hooks (or any host-side recorder).
/// Hosts SHOULD truncate to this cap before pushing new ids; the gate
/// signal is "≥cap distinct, which is plenty of evidence" so dropping
/// further ids past 50 is acceptable.
///
/// If the cap is exceeded (host bug or malicious input), the gate still
/// produces a correct boolean signal — the only failure mode is
/// frontmatter bloat, not a wedge violation.
pub const MAX_APPLIED_SESSION_IDS: usize = 50;

/// Derived signal: does this lesson have multi-session reproducibility?
/// Returns true when `applied_session_ids.len() >= 2`. Cheap pure
/// function so callers (gate, telemetry, CLI inspection) can read
/// without re-deriving the rule.
pub fn derive_origin_diverse(fm: &LessonFrontmatter) -> bool {
    fm.applied_session_ids.len() >= 2
}

/// Outcome of [`check_promotion_gate`]. Either an enumerated list of
/// pass reasons (Promote) or block reasons (Block) — never an empty
/// list on either side. Construct only via the gate.
///
/// `#[non_exhaustive]` so future calibration variants (e.g.
/// `PromoteWithCaveats`, `AuditOnly`) can land without a SemVer break.
/// External callers wildcard-match (`_ => ...`) instead of relying on
/// today's two-variant set being complete.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GateDecision {
    /// Lesson satisfies all configured gates. `reasons` lists the
    /// individual rules that passed — useful for CLI output and tests
    /// that want to assert specific checks ran.
    Promote { reasons: Vec<PassReason> },
    /// Lesson fails one or more gates. `reasons` is the COMPLETE set
    /// of violations (gate does not first-fail) so callers can render
    /// the full picture.
    Block { reasons: Vec<BlockReason> },
}

impl GateDecision {
    /// Convenience: did the gate promote?
    pub fn is_promote(&self) -> bool {
        matches!(self, GateDecision::Promote { .. })
    }

    /// Convenience: did the gate block?
    pub fn is_block(&self) -> bool {
        matches!(self, GateDecision::Block { .. })
    }
}

/// Specific rule violations. Order in [`GateDecision::Block::reasons`]
/// matches the check order in [`check_promotion_gate`] — stable across
/// versions to keep CLI/UI output predictable.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BlockReason {
    /// Lesson is already in a terminal `superseded` state OR carries a
    /// `superseded_at` timestamp. Promotion is meaningless from here.
    /// Either condition alone suffices — defense-in-depth against a
    /// half-state where `superseded_at` is set without `status:
    /// superseded` (or vice versa), which can arise from a partial
    /// supersession write or cross-process race.
    AlreadySuperseded,
    /// Frontmatter `created_at` couldn't be parsed as RFC 3339 / ISO
    /// 8601. The gate falls back to other rules but flags this so the
    /// author can fix the YAML.
    MalformedCreatedAt { value: String },
    /// Frontmatter `created_at` is in the future relative to `now`.
    /// Could be clock skew or tampering; either way, promotion is
    /// premature. OQ-B5 per learn-notes D8.
    FutureCreatedAt {
        frontmatter_created_at: DateTime<Utc>,
    },
    /// Lesson hasn't existed long enough per `config.min_age`. The
    /// 24-hour time floor catches "lesson captured and immediately
    /// self-promoted" attacks.
    TimeFloor {
        age_seconds: i64,
        required_seconds: i64,
    },
    /// Filesystem birthtime contradicts frontmatter `created_at` —
    /// the storage backend says the key was created LATER than the
    /// frontmatter claims (i.e., backdated to look more mature).
    /// THE marketing-wedge invariant.
    TamperedAge {
        frontmatter_created_at: DateTime<Utc>,
        birthtime: DateTime<Utc>,
    },
    /// `applied_count` is below `config.min_applied_count`. Usage-
    /// volume floor (TS-parity rule). A lesson must have been USED
    /// N times before the system trusts it; a self-grading agent
    /// can't pump `applied_count` without ALSO emitting external
    /// signal events (caught separately by
    /// [`Self::MissingExternalSignalSources`]).
    InsufficientAppliedCount { observed: u64, required: u64 },
    /// At least one thumbs-down has been recorded. Hard block — no
    /// amount of thumbs-ups overrides a recorded negative signal.
    ThumbsDownBlock { count: u64 },
    /// `external_signal_sources` is empty. Promotion requires at least
    /// one external signal (a process outside the lesson author's
    /// control: thumbs-up, applied event, ingest from auto-memory).
    MissingExternalSignalSources,
    /// `causal_narrative` is `None`. Promotion requires a documented
    /// trigger / failure-mode / correction triple.
    MissingCausalNarrative,
    /// `causal_narrative.confidence == speculative`. Speculative lessons
    /// should not be promoted; they must be validated to `inferred` or
    /// `observed` first.
    SpeculativeNarrative,
    /// `causal_narrative.confidence == observed` but `evidence_refs` is
    /// empty. "Observed" without pointers is a self-grading claim,
    /// not evidence — block until refs are attached.
    ObservedConfidenceWithoutEvidenceRefs,
    /// Phase G D-G3 (v0.4): `applied_session_ids.len()` is below
    /// `config.min_distinct_origins`. The lesson lacks multi-session
    /// reproducibility — a self-grading agent in one long session
    /// could pump `applied_count` without ever touching a second
    /// session_id. Promotion held until distinct sessions have
    /// confirmed the lesson reproduces.
    InsufficientOriginDiversity { observed: u32, required: u32 },
}

/// `Display` for [`BlockReason`] renders a stable, single-line label
/// suitable for CLI output and error messages. The strings are part
/// of the SemVer surface — changing them is a breaking change for
/// callers that scrape error text. Keep them short, kebab-style, and
/// include the load-bearing data fields inline.
impl fmt::Display for BlockReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadySuperseded => write!(f, "already-superseded"),
            Self::MalformedCreatedAt { value } => {
                write!(f, "malformed-created-at: {value:?}")
            }
            Self::FutureCreatedAt {
                frontmatter_created_at,
            } => {
                write!(f, "future-created-at: {frontmatter_created_at}")
            }
            Self::TimeFloor {
                age_seconds,
                required_seconds,
            } => {
                write!(
                    f,
                    "time-floor: age={age_seconds}s < required={required_seconds}s"
                )
            }
            Self::TamperedAge {
                frontmatter_created_at,
                birthtime,
            } => {
                write!(
                    f,
                    "tampered-age: frontmatter_created_at={frontmatter_created_at} \
                     birthtime={birthtime} (storage layer says file was created LATER \
                     than the YAML claims)"
                )
            }
            Self::InsufficientAppliedCount { observed, required } => {
                write!(
                    f,
                    "insufficient-applied-count: observed={observed} < required={required}"
                )
            }
            Self::ThumbsDownBlock { count } => {
                write!(f, "thumbs-down-block: count={count}")
            }
            Self::MissingExternalSignalSources => {
                write!(f, "missing-external-signal-sources")
            }
            Self::MissingCausalNarrative => write!(f, "missing-causal-narrative"),
            Self::SpeculativeNarrative => write!(f, "speculative-narrative"),
            Self::ObservedConfidenceWithoutEvidenceRefs => {
                write!(f, "observed-confidence-without-evidence-refs")
            }
            Self::InsufficientOriginDiversity { observed, required } => {
                write!(
                    f,
                    "insufficient-origin-diversity: distinct_sessions={observed} < required={required}"
                )
            }
        }
    }
}

/// Specific rule successes — itemized so CLI/UI can confirm which
/// checks ran. Lean today (5 variants) but expected to grow as the
/// gate adds knobs.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PassReason {
    /// `now - frontmatter_created_at >= config.min_age` — lesson has
    /// survived at least one full configured-floor window of real use.
    AgePassed,
    /// Filesystem birthtime is consistent with frontmatter
    /// `created_at` (within `tamper_skew_tolerance`). When the backend
    /// CANNOT determine birthtime (e.g., FAT32, older Linux kernels),
    /// the gate ABSTAINS and does NOT push this variant — abstention
    /// is not the same as proof-of-no-tamper.
    NoTamperDetected,
    /// `applied_count >= config.min_applied_count` — usage-volume
    /// floor satisfied.
    AppliedCountAboveFloor,
    /// `thumbs_down_count == 0` — no negative signal recorded.
    NoThumbsDown,
    /// `external_signal_sources` is non-empty — at least one external
    /// signal (separate process) has touched the lesson.
    HasExternalSignalSources,
    /// `causal_narrative` is present AND confidence is `inferred`
    /// OR `observed` with non-empty `evidence_refs`. Covers both
    /// branches of the confidence-ladder gate.
    CausalNarrativeOk,
    /// Phase G D-G3 (v0.4): `applied_session_ids.len() >=
    /// config.min_distinct_origins`. The lesson reproduces across
    /// multiple sessions — strong external signal that's hard to
    /// fake within one agent's run.
    OriginDiverse,
}

/// Run the promotion gate.
///
/// Pure, sync, side-effect free. `now` is injected for test determinism
/// (Day 16a D4 pattern) — call sites in production pass `Utc::now()`.
///
/// Accumulates all violations rather than first-failing — callers can
/// display the full picture. If ANY block fires, returns `Block`; only
/// when zero blocks fire does it return `Promote`.
pub fn check_promotion_gate(
    fm: &LessonFrontmatter,
    metadata: &StorageMetadata,
    config: &PromotionConfig,
    now: DateTime<Utc>,
) -> GateDecision {
    let mut blocks: Vec<BlockReason> = Vec::new();
    let mut passes: Vec<PassReason> = Vec::new();

    // 1. Status check — superseded lessons can't be promoted.
    if fm.status == LessonStatus::Superseded || fm.superseded_at.is_some() {
        blocks.push(BlockReason::AlreadySuperseded);
    }

    // 2. created_at parsing + future + time-floor.
    match fm.created_at.parse::<DateTime<Utc>>() {
        Err(_) => {
            blocks.push(BlockReason::MalformedCreatedAt {
                value: fm.created_at.clone(),
            });
        }
        Ok(created) if created > now => {
            blocks.push(BlockReason::FutureCreatedAt {
                frontmatter_created_at: created,
            });
            // Also flag time-floor: a future-dated lesson can't satisfy
            // a positive min_age. Both diagnostics surface (D8).
            blocks.push(BlockReason::TimeFloor {
                age_seconds: (now - created).num_seconds(),
                required_seconds: config.min_age.num_seconds(),
            });
        }
        Ok(created) => {
            // 2a. Time-floor.
            let age = now - created;
            if age < config.min_age {
                blocks.push(BlockReason::TimeFloor {
                    age_seconds: age.num_seconds(),
                    required_seconds: config.min_age.num_seconds(),
                });
            } else {
                passes.push(PassReason::AgePassed);
            }

            // 2b. Tampered-age (the wedge invariant). Skip if backend
            // can't determine birthtime (`metadata.birthtime == None`):
            // the gate abstains rather than firing a false positive.
            // A backend that DOES report birthtime and reports it AFTER
            // the frontmatter `created_at` (beyond tolerance) is proof
            // of backdating.
            if let Some(birth) = metadata.birthtime {
                let skew = birth - created;
                if skew > config.tamper_skew_tolerance {
                    blocks.push(BlockReason::TamperedAge {
                        frontmatter_created_at: created,
                        birthtime: birth,
                    });
                } else {
                    passes.push(PassReason::NoTamperDetected);
                }
            }
        }
    }

    // 3. Usage-volume floor (TS-parity rule, default 3).
    if fm.applied_count < config.min_applied_count {
        blocks.push(BlockReason::InsufficientAppliedCount {
            observed: fm.applied_count,
            required: config.min_applied_count,
        });
    } else {
        passes.push(PassReason::AppliedCountAboveFloor);
    }

    // 4. Thumbs-down hard block.
    if fm.thumbs_down_count > 0 {
        blocks.push(BlockReason::ThumbsDownBlock {
            count: fm.thumbs_down_count,
        });
    } else {
        passes.push(PassReason::NoThumbsDown);
    }

    // 5. External signal sources required.
    if fm.external_signal_sources.is_empty() {
        blocks.push(BlockReason::MissingExternalSignalSources);
    } else {
        passes.push(PassReason::HasExternalSignalSources);
    }

    // 5b. (Phase G D-G3, v0.4) Origin-diversity floor. Disabled by
    // default (`min_distinct_origins == 0`); when set, requires the
    // lesson to have applied across N distinct sessions. Recording
    // half ships in Phase 2 (hooks); until then this stays inert
    // unless callers explicitly opt-in.
    if config.min_distinct_origins > 0 {
        let observed = fm.applied_session_ids.len() as u32;
        if observed < config.min_distinct_origins {
            blocks.push(BlockReason::InsufficientOriginDiversity {
                observed,
                required: config.min_distinct_origins,
            });
        } else {
            passes.push(PassReason::OriginDiverse);
        }
    }

    // 6. Causal narrative checks (presence + confidence rules).
    match &fm.causal_narrative {
        None => {
            blocks.push(BlockReason::MissingCausalNarrative);
        }
        Some(cn) => match cn.confidence {
            Confidence::Speculative => {
                blocks.push(BlockReason::SpeculativeNarrative);
            }
            Confidence::Observed if cn.evidence_refs.is_empty() => {
                blocks.push(BlockReason::ObservedConfidenceWithoutEvidenceRefs);
            }
            _ => {
                passes.push(PassReason::CausalNarrativeOk);
            }
        },
    }

    if blocks.is_empty() {
        GateDecision::Promote { reasons: passes }
    } else {
        GateDecision::Block { reasons: blocks }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::yaml::{CausalNarrative, EvidenceRef, GeneratedBy};

    fn now() -> DateTime<Utc> {
        "2026-05-13T12:00:00Z".parse().unwrap()
    }

    /// Build a minimally-passing frontmatter: 2 days old, has narrative
    /// with inferred confidence, has external signal source, no thumbs
    /// down. Tests mutate from this baseline to exercise individual rules.
    fn passing_fm() -> LessonFrontmatter {
        LessonFrontmatter {
            id: "les-pass1234".into(),
            description: "passing baseline".into(),
            status: LessonStatus::Active,
            created_at: "2026-05-11T12:00:00Z".into(), // 2 days before `now()`
            causal_narrative: Some(CausalNarrative {
                trigger: "trig".into(),
                failure_mode: "fm".into(),
                correction: "cor".into(),
                confidence: Confidence::Inferred,
                evidence_refs: vec![],
                generated_by: GeneratedBy::Llm,
                generated_at: "2026-05-11T12:00:00Z".into(),
            }),
            target_skill: None,
            source_feedback_ids: None,
            // Clearly above the default `min_applied_count: 3`. Tests
            // that exercise the applied-count rule (s23/s24/s25) mutate
            // this baseline.
            applied_count: 5,
            last_applied_at: None,
            thumbs_up_count: 2,
            thumbs_down_count: 0,
            external_signal_sources: vec!["thumbs_up".into()],
            applied_session_ids: vec![],
            promotion_eligible_at: None,
            superseded_by: None,
            superseded_at: None,
            ingest_provenance: None,
            authored_by: Default::default(),
            updated_at: None,
        }
    }

    /// Birthtime that matches frontmatter created_at (no tamper signal).
    fn matching_metadata(fm: &LessonFrontmatter) -> StorageMetadata {
        let bt: DateTime<Utc> = fm.created_at.parse().unwrap();
        StorageMetadata {
            birthtime: Some(bt),
            mtime: Some(bt),
            size_bytes: 0,
        }
    }

    // -----------------------------------------------------------------
    // Happy path
    // -----------------------------------------------------------------

    #[test]
    fn s01_happy_path_promotes() {
        let fm = passing_fm();
        let md = matching_metadata(&fm);
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        assert!(dec.is_promote(), "expected promote, got {dec:?}");
        if let GateDecision::Promote { reasons } = dec {
            assert!(reasons.contains(&PassReason::AgePassed));
            assert!(reasons.contains(&PassReason::NoTamperDetected));
            assert!(reasons.contains(&PassReason::AppliedCountAboveFloor));
            assert!(reasons.contains(&PassReason::NoThumbsDown));
            assert!(reasons.contains(&PassReason::HasExternalSignalSources));
            assert!(reasons.contains(&PassReason::CausalNarrativeOk));
        }
    }

    // -----------------------------------------------------------------
    // Status sieve
    // -----------------------------------------------------------------

    #[test]
    fn s02_superseded_status_blocks() {
        let mut fm = passing_fm();
        fm.status = LessonStatus::Superseded;
        let md = matching_metadata(&fm);
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        match dec {
            GateDecision::Block { reasons } => {
                assert!(reasons.contains(&BlockReason::AlreadySuperseded));
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[test]
    fn s03_superseded_at_set_blocks_even_if_status_active() {
        let mut fm = passing_fm();
        fm.superseded_at = Some("2026-05-12T00:00:00Z".into());
        let md = matching_metadata(&fm);
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        match dec {
            GateDecision::Block { reasons } => {
                assert!(reasons.contains(&BlockReason::AlreadySuperseded));
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Created-at parsing + future + time-floor
    // -----------------------------------------------------------------

    #[test]
    fn s04_malformed_created_at_blocks() {
        let mut fm = passing_fm();
        fm.created_at = "not-a-date".into();
        let md = StorageMetadata {
            birthtime: None,
            mtime: None,
            size_bytes: 0,
        };
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        match dec {
            GateDecision::Block { reasons } => {
                assert!(reasons
                    .iter()
                    .any(|r| matches!(r, BlockReason::MalformedCreatedAt { .. })));
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[test]
    fn s05_future_created_at_blocks_with_diagnostics() {
        let mut fm = passing_fm();
        fm.created_at = "2099-01-01T00:00:00Z".into();
        let md = StorageMetadata {
            birthtime: None,
            mtime: None,
            size_bytes: 0,
        };
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        match dec {
            GateDecision::Block { reasons } => {
                // Both FutureCreatedAt and TimeFloor fire (D8 — both surfaces are diagnostic).
                assert!(reasons
                    .iter()
                    .any(|r| matches!(r, BlockReason::FutureCreatedAt { .. })));
                assert!(reasons
                    .iter()
                    .any(|r| matches!(r, BlockReason::TimeFloor { .. })));
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[test]
    fn s06_time_floor_blocks_lesson_under_24h() {
        let mut fm = passing_fm();
        // 1 hour ago — well under 24h floor.
        fm.created_at = (now() - Duration::hours(1)).to_rfc3339();
        let md = matching_metadata(&fm);
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        match dec {
            GateDecision::Block { reasons } => {
                assert!(reasons
                    .iter()
                    .any(|r| matches!(r, BlockReason::TimeFloor { .. })));
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[test]
    fn s07_time_floor_boundary_exactly_min_age_passes() {
        let mut fm = passing_fm();
        // EXACTLY 24h ago — `age >= min_age` so AgePassed.
        let exact = now() - Duration::hours(24);
        fm.created_at = exact.to_rfc3339();
        let mut md = matching_metadata(&fm);
        md.birthtime = Some(exact);
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        assert!(dec.is_promote(), "expected promote, got {dec:?}");
    }

    // -----------------------------------------------------------------
    // Tampered-age (the wedge)
    // -----------------------------------------------------------------

    #[test]
    fn s08_tampered_age_blocks_when_birthtime_after_frontmatter() {
        let mut fm = passing_fm();
        // Frontmatter SAYS 2 days ago...
        fm.created_at = "2026-05-11T12:00:00Z".into();
        // ...but the storage layer says the key was actually created just now.
        let md = StorageMetadata {
            birthtime: Some(now()),
            mtime: Some(now()),
            size_bytes: 0,
        };
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        match dec {
            GateDecision::Block { reasons } => {
                assert!(reasons
                    .iter()
                    .any(|r| matches!(r, BlockReason::TamperedAge { .. })));
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[test]
    fn s09_no_birthtime_means_no_tamper_check() {
        // Backend can't determine birthtime → gate abstains on tamper.
        // Other rules can still pass.
        let fm = passing_fm();
        let md = StorageMetadata {
            birthtime: None,
            mtime: None,
            size_bytes: 0,
        };
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        assert!(dec.is_promote(), "expected promote, got {dec:?}");
        if let GateDecision::Promote { reasons } = dec {
            // NoTamperDetected NOT in reasons (abstained).
            assert!(!reasons.contains(&PassReason::NoTamperDetected));
            // But AgePassed is.
            assert!(reasons.contains(&PassReason::AgePassed));
        }
    }

    #[test]
    fn s10_small_clock_skew_within_tolerance_does_not_block() {
        let mut fm = passing_fm();
        let claimed: DateTime<Utc> = "2026-05-11T12:00:00Z".parse().unwrap();
        fm.created_at = claimed.to_rfc3339();
        // birthtime is 30s after claimed — under default 60s tolerance.
        let md = StorageMetadata {
            birthtime: Some(claimed + Duration::seconds(30)),
            mtime: Some(claimed + Duration::seconds(30)),
            size_bytes: 0,
        };
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        assert!(dec.is_promote(), "expected promote, got {dec:?}");
    }

    // -----------------------------------------------------------------
    // Thumbs-down hard block
    // -----------------------------------------------------------------

    #[test]
    fn s11_single_thumbs_down_blocks() {
        let mut fm = passing_fm();
        fm.thumbs_down_count = 1;
        let md = matching_metadata(&fm);
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        match dec {
            GateDecision::Block { reasons } => {
                assert!(reasons
                    .iter()
                    .any(|r| matches!(r, BlockReason::ThumbsDownBlock { count: 1 })));
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[test]
    fn s12_thumbs_down_blocks_regardless_of_thumbs_up() {
        let mut fm = passing_fm();
        fm.thumbs_up_count = 100;
        fm.thumbs_down_count = 1;
        let md = matching_metadata(&fm);
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        assert!(
            dec.is_block(),
            "100 thumbs up does not override 1 thumbs down"
        );
    }

    // -----------------------------------------------------------------
    // External signal sources
    // -----------------------------------------------------------------

    #[test]
    fn s13_empty_external_signal_sources_blocks() {
        let mut fm = passing_fm();
        fm.external_signal_sources = vec![];
        let md = matching_metadata(&fm);
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        match dec {
            GateDecision::Block { reasons } => {
                assert!(reasons.contains(&BlockReason::MissingExternalSignalSources));
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Causal narrative + confidence ladder
    // -----------------------------------------------------------------

    #[test]
    fn s14_missing_causal_narrative_blocks() {
        let mut fm = passing_fm();
        fm.causal_narrative = None;
        let md = matching_metadata(&fm);
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        match dec {
            GateDecision::Block { reasons } => {
                assert!(reasons.contains(&BlockReason::MissingCausalNarrative));
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[test]
    fn s15_speculative_narrative_blocks() {
        let mut fm = passing_fm();
        if let Some(cn) = fm.causal_narrative.as_mut() {
            cn.confidence = Confidence::Speculative;
        }
        let md = matching_metadata(&fm);
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        match dec {
            GateDecision::Block { reasons } => {
                assert!(reasons.contains(&BlockReason::SpeculativeNarrative));
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[test]
    fn s16_observed_without_evidence_refs_blocks() {
        let mut fm = passing_fm();
        if let Some(cn) = fm.causal_narrative.as_mut() {
            cn.confidence = Confidence::Observed;
            cn.evidence_refs = vec![];
        }
        let md = matching_metadata(&fm);
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        match dec {
            GateDecision::Block { reasons } => {
                assert!(reasons.contains(&BlockReason::ObservedConfidenceWithoutEvidenceRefs));
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[test]
    fn s17_observed_with_evidence_refs_passes_confidence_check() {
        let mut fm = passing_fm();
        if let Some(cn) = fm.causal_narrative.as_mut() {
            cn.confidence = Confidence::Observed;
            cn.evidence_refs = vec![EvidenceRef::Quote("session:abc123#tool_use_1".into())];
        }
        let md = matching_metadata(&fm);
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        assert!(dec.is_promote(), "expected promote, got {dec:?}");
    }

    #[test]
    fn s18_inferred_narrative_passes_regardless_of_evidence_refs() {
        let mut fm = passing_fm();
        if let Some(cn) = fm.causal_narrative.as_mut() {
            cn.confidence = Confidence::Inferred;
            cn.evidence_refs = vec![]; // empty OK for inferred
        }
        let md = matching_metadata(&fm);
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        assert!(dec.is_promote(), "expected promote, got {dec:?}");
    }

    // -----------------------------------------------------------------
    // Multi-block accumulation
    // -----------------------------------------------------------------

    #[test]
    fn s19_multiple_violations_all_surface() {
        let mut fm = passing_fm();
        fm.thumbs_down_count = 1;
        fm.causal_narrative = None;
        fm.external_signal_sources = vec![];
        let md = matching_metadata(&fm);
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        match dec {
            GateDecision::Block { reasons } => {
                assert!(reasons
                    .iter()
                    .any(|r| matches!(r, BlockReason::ThumbsDownBlock { .. })));
                assert!(reasons.contains(&BlockReason::MissingCausalNarrative));
                assert!(reasons.contains(&BlockReason::MissingExternalSignalSources));
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[test]
    fn s20_malformed_created_at_does_not_short_circuit_other_rules() {
        let mut fm = passing_fm();
        fm.created_at = "garbage".into();
        if let Some(cn) = fm.causal_narrative.as_mut() {
            cn.confidence = Confidence::Speculative;
        }
        let md = StorageMetadata {
            birthtime: None,
            mtime: None,
            size_bytes: 0,
        };
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        match dec {
            GateDecision::Block { reasons } => {
                assert!(reasons
                    .iter()
                    .any(|r| matches!(r, BlockReason::MalformedCreatedAt { .. })));
                assert!(reasons.contains(&BlockReason::SpeculativeNarrative));
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Usage-volume floor (InsufficientAppliedCount)
    // -----------------------------------------------------------------

    #[test]
    fn s23_applied_count_below_min_blocks() {
        let mut fm = passing_fm();
        fm.applied_count = 1; // default min is 3
        let md = matching_metadata(&fm);
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        match dec {
            GateDecision::Block { reasons } => {
                assert!(reasons.iter().any(|r| matches!(
                    r,
                    BlockReason::InsufficientAppliedCount {
                        observed: 1,
                        required: 3
                    }
                )));
            }
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[test]
    fn s24_applied_count_at_min_boundary_promotes() {
        // EXACTLY min_applied_count — `applied_count >= min` so
        // AppliedCountAboveFloor pushes.
        let mut fm = passing_fm();
        fm.applied_count = 3;
        let md = matching_metadata(&fm);
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        assert!(
            dec.is_promote(),
            "expected promote at exact boundary, got {dec:?}"
        );
        if let GateDecision::Promote { reasons } = dec {
            assert!(reasons.contains(&PassReason::AppliedCountAboveFloor));
        }
    }

    #[test]
    fn s25_applied_count_zero_blocks_with_default_config() {
        // Freshly-captured lesson, never applied — the wedge case the
        // TS-parity rule defends against.
        let mut fm = passing_fm();
        fm.applied_count = 0;
        let md = matching_metadata(&fm);
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        match dec {
            GateDecision::Block { reasons } => {
                assert!(reasons.iter().any(|r| matches!(
                    r,
                    BlockReason::InsufficientAppliedCount { observed: 0, .. }
                )));
            }
            other => panic!("expected block on zero applied_count, got {other:?}"),
        }
    }

    #[test]
    fn s26_custom_min_applied_count_one_promotes_at_one() {
        // Custom config: min_applied_count = 1. applied_count = 1 should pass.
        let mut fm = passing_fm();
        fm.applied_count = 1;
        let md = matching_metadata(&fm);
        let config = PromotionConfig {
            min_applied_count: 1,
            ..PromotionConfig::default()
        };
        let dec = check_promotion_gate(&fm, &md, &config, now());
        assert!(
            dec.is_promote(),
            "custom min=1 + applied=1 should promote, got {dec:?}"
        );
    }

    // -----------------------------------------------------------------
    // Wedge integration test — on-disk birthtime regression
    // -----------------------------------------------------------------
    // Per learn-notes D7 "2 on-disk integration tests via TestHarness":
    // the marketing-wedge regression. A lesson seeded with a backdated
    // frontmatter still gets caught because the storage layer records
    // the actual creation time.
    #[tokio::test]
    async fn s21_wedge_backdated_frontmatter_caught_via_birthtime() {
        use crate::engine::storage::StorageKey;
        use crate::engine::test_support::TestHarness;
        use crate::engine::yaml::reader::parse_lesson_frontmatter;
        use crate::engine::yaml::split_frontmatter_normalized;

        let h = TestHarness::in_memory();
        // Seed with backdated frontmatter — frontmatter claims 30 days
        // ago, but TestHarness writes RIGHT NOW so birthtime is fresh.
        let id = "les-wedge001";
        let backdated_iso = "2026-04-13T00:00:00Z";
        h.seed_lesson_with_created_at("active", id, "test body", backdated_iso)
            .await
            .unwrap();

        let key = StorageKey::lesson(&h.ctx, "active", id);
        let bytes = h.storage.get(&key).await.unwrap().unwrap();
        let split = split_frontmatter_normalized(std::str::from_utf8(&bytes).unwrap()).unwrap();
        let mut fm = parse_lesson_frontmatter(&split.yaml).unwrap();
        // Round out frontmatter so OTHER rules pass — ISOLATE the
        // tamper signal as THE SOLE cause of the block. If anything
        // else fires, the wedge claim is over-passing: the assertion
        // below requires reasons == [TamperedAge].
        fm.causal_narrative = Some(CausalNarrative {
            trigger: "t".into(),
            failure_mode: "f".into(),
            correction: "c".into(),
            confidence: Confidence::Inferred,
            evidence_refs: vec![],
            generated_by: GeneratedBy::Llm,
            generated_at: "2026-05-13T00:00:00Z".into(),
        });
        fm.external_signal_sources = vec!["thumbs_up".into()];
        // applied_count must clear the volume floor too — otherwise
        // InsufficientAppliedCount co-fires and we can't prove the
        // wedge caught the backdate IN ISOLATION.
        fm.applied_count = 5;

        let md = h.storage.metadata(&key).await.unwrap().unwrap();
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());

        match dec {
            GateDecision::Block { reasons } => {
                assert_eq!(
                    reasons.len(),
                    1,
                    "wedge regression FAILED: expected exactly 1 block reason (TamperedAge), \
                     got {} reasons: {reasons:?}. Either the wedge over-passes (other rules \
                     co-fire) or the fixture rounding-out is incomplete.",
                    reasons.len()
                );
                assert!(
                    matches!(reasons[0], BlockReason::TamperedAge { .. }),
                    "wedge invariant FAILED: the sole block reason should be TamperedAge, \
                     got {:?}",
                    reasons[0]
                );
            }
            other => panic!("wedge: expected block on backdated lesson, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Origin diversity (Phase G D-G3, v0.4) — `InsufficientOriginDiversity`
    // / `OriginDiverse` / `derive_origin_diverse` helper.
    // -----------------------------------------------------------------

    #[test]
    fn s27_default_config_origin_diversity_check_inert() {
        // Default `PromotionConfig::min_distinct_origins == 0` →
        // gate must NOT push `OriginDiverse` (or `InsufficientOriginDiversity`)
        // even on a lesson with empty `applied_session_ids`. The
        // 5b block at gate.rs:410 is wrapped in `if > 0`.
        let fm = passing_fm(); // applied_session_ids = vec![] by default
        let md = matching_metadata(&fm);
        let dec = check_promotion_gate(&fm, &md, &PromotionConfig::default(), now());
        assert!(
            dec.is_promote(),
            "default config should still promote, got {dec:?}"
        );
        if let GateDecision::Promote { reasons } = dec {
            assert!(
                !reasons.contains(&PassReason::OriginDiverse),
                "OriginDiverse must NOT fire when min_distinct_origins=0, got {reasons:?}"
            );
        }
    }

    #[test]
    fn s28_origin_diversity_below_required_blocks() {
        // min_distinct_origins=2, applied_session_ids=[] → observed=0, required=2.
        let fm = passing_fm();
        let md = matching_metadata(&fm);
        let config = PromotionConfig {
            min_distinct_origins: 2,
            ..PromotionConfig::default()
        };
        let dec = check_promotion_gate(&fm, &md, &config, now());
        match dec {
            GateDecision::Block { reasons } => {
                assert!(reasons.iter().any(|r| matches!(
                    r,
                    BlockReason::InsufficientOriginDiversity {
                        observed: 0,
                        required: 2
                    }
                )));
            }
            other => panic!("expected block on missing diversity, got {other:?}"),
        }
    }

    #[test]
    fn s29_origin_diversity_at_or_above_required_promotes() {
        // min_distinct_origins=2, applied_session_ids=["s1","s2"] → observed=2.
        let mut fm = passing_fm();
        fm.applied_session_ids = vec!["sess1234".into(), "sessabcd".into()];
        let md = matching_metadata(&fm);
        let config = PromotionConfig {
            min_distinct_origins: 2,
            ..PromotionConfig::default()
        };
        let dec = check_promotion_gate(&fm, &md, &config, now());
        assert!(
            dec.is_promote(),
            "expected promote with diverse origins, got {dec:?}"
        );
        if let GateDecision::Promote { reasons } = dec {
            assert!(reasons.contains(&PassReason::OriginDiverse));
        }
    }

    #[test]
    fn s30_derive_origin_diverse_predicate() {
        // Pure helper: true iff applied_session_ids.len() >= 2.
        let mut fm = passing_fm();
        assert!(!derive_origin_diverse(&fm), "len 0 → false");
        fm.applied_session_ids = vec!["a".into()];
        assert!(!derive_origin_diverse(&fm), "len 1 → false");
        fm.applied_session_ids = vec!["a".into(), "b".into()];
        assert!(derive_origin_diverse(&fm), "len 2 → true");
        fm.applied_session_ids = vec!["a".into(), "b".into(), "c".into()];
        assert!(derive_origin_diverse(&fm), "len 3 → true");
    }

    #[tokio::test]
    async fn s22_on_disk_lesson_with_matched_birthtime_promotes() {
        use crate::engine::storage::StorageKey;
        use crate::engine::test_support::TestHarness;

        // Real LocalFsStorage roundtrip. The on-disk fs uses the real
        // wall clock for birthtime, so we anchor BOTH the frontmatter
        // and the gate's `now` to `Utc::now()` for this single test
        // (the rest of the matrix uses the fixed `now()` helper for
        // determinism — that doesn't work here because we can't force
        // the filesystem's wall clock).
        let h = TestHarness::on_disk();
        let id = "les-ondisk09";
        h.seed_lesson("active", id, "body").await.unwrap();
        let key = StorageKey::lesson(&h.ctx, "active", id);
        let md = h.storage.metadata(&key).await.unwrap().unwrap();
        let mut fm = passing_fm();
        fm.created_at = md
            .birthtime
            .expect("LocalFsStorage should report birthtime on macOS/APFS")
            .to_rfc3339();
        let config = PromotionConfig {
            min_age: Duration::seconds(0), // disable time floor for this test
            ..PromotionConfig::default()
        };
        // Use real `Utc::now()` here — the filesystem birthtime was
        // stamped against the real clock, not our fixed test-clock.
        let dec = check_promotion_gate(&fm, &md, &config, Utc::now());
        assert!(dec.is_promote(), "expected on-disk promote, got {dec:?}");
    }
}
