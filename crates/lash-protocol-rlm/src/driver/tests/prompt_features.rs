use super::*;

/// With images off, the history schema has no image fields. The budget's
/// decomposition rules are the context-budget section's.
#[test]
fn second_round_respects_disabled_images() {
    for typescript in [false, true] {
        let mut projector = projector(1000);
        if typescript {
            projector.dialect = Arc::new(crate::dialect::typescript_test_dialect());
        }
        projector.prompt_features.images = false;
        let request =
            project_iteration_request(&projector, &[step_event(0, "1", "1")], 1, "test-model");
        let tail = message_text(request.messages.last().unwrap());
        assert!(tail.contains("type HistoryItem ="), "{tail}");
        for forbidden in ["HistoryImage", "images?"] {
            assert!(!tail.contains(forbidden), "{forbidden}: {tail}");
        }
    }
}
