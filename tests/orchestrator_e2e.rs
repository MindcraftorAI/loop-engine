//! End-to-end integration test (Day 17 D5).
//!
//! Scenario (a) per Day 17 pre-research Q6: MockSentimentClassifier →
//! Orchestrator → MockSignalWriter, filesystem-free.
//!
//! Scenario (b) JsonlWatcherSource → Orchestrator deferred per Day 17
//! D7 to the post-adapter-discussion follow-up cycle.

use std::sync::Arc;

use chrono::Utc;

use loop_daemon::engine::context::Context;
use loop_daemon::engine::events::EngineEvent;
use loop_daemon::engine::sentiment::{
    classifier::MockSentimentClassifier,
    signals::MockSignalWriter,
    types::{
        ClassifierConfidence, ItemClassification, LoadedItem, LoadedItemId, LoadedItemKind,
        Polarity, RawClassification,
    },
    Orchestrator, OrchestratorConfig, SentimentClassifier, SignalWriter,
};

#[tokio::test]
async fn end_to_end_positive_signal_flows_classifier_through_writer() {
    // Set up a mock classifier with a canned positive hit.
    let canned = RawClassification {
        per_item: vec![ItemClassification {
            item_id: LoadedItemId::new("les-quokka-special"),
            polarity: Polarity::Positive,
            confidence: ClassifierConfidence::new(0.92),
            evidence: None,
            hazards: vec![],
        }],
        global_hazards: vec![],
    };
    let classifier =
        Arc::new(MockSentimentClassifier::default().with_response(canned.clone()));
    let writer = Arc::new(MockSignalWriter::default());
    let orch = Orchestrator::new(
        classifier.clone() as Arc<dyn SentimentClassifier>,
        writer.clone() as Arc<dyn SignalWriter>,
        OrchestratorConfig::default(),
    );

    let ctx = Context::single_user_local();

    // Seed the manifest so attribution can match the classifier's named item.
    orch.update_manifest(
        &ctx.session_id,
        vec![LoadedItem {
            id: LoadedItemId::new("les-quokka-special"),
            kind: LoadedItemKind::Lesson,
            label: "Quokka special".into(),
            keywords: vec!["quokka-special".into()],
        }],
    );

    // Process a user turn whose text triggers attribution and which the
    // classifier will identify as positive.
    let turn = EngineEvent::UserTurn {
        session_id: ctx.session_id.clone(),
        event_uuid: "evt-e2e-1".into(),
        parent_event_uuid: None,
        text: "thanks for quokka-special".into(),
        timestamp: Utc::now(),
        cwd: None,
        host_version: None,
        project_tag: None,
    };
    let out = orch.process_event(&ctx, &turn).await;

    // Assert: one signal emitted, captured by the mock writer.
    assert_eq!(out.signals.len(), 1, "expected one signal emitted");
    assert_eq!(out.signals[0].item_id.as_str(), "les-quokka-special");
    assert_eq!(out.signals[0].polarity, Polarity::Positive);
    assert_eq!(classifier.call_count(), 1);
    let captured = writer.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].item_id.as_str(), "les-quokka-special");
}
