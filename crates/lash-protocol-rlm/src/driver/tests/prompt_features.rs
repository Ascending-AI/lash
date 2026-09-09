use super::*;

#[test]
fn rlm_projector_renders_a_reachable_threshold_for_a_small_context_window() {
    let mut projector = projector(1000);
    projector.max_budget_tokens = Some(100_000);
    *projector.last_prompt_usage.write_recover() = Some(lash_core::PromptUsage {
        context_budget_tokens: 40_999,
        ..Default::default()
    });

    let request = project_iteration_request_with_generation(
        &projector,
        &[],
        0,
        "test-model",
        Default::default(),
        Some(41_000),
    );
    let tail = message_text(request.messages.last().expect("current iteration tail"));

    assert!(tail.contains("frame switch threshold: 40999"));
    assert!(!tail.contains("frame switch threshold: 100000"));
}

#[test]
fn second_round_respects_disabled_images_and_decomposition() {
    for typescript in [false, true] {
        let mut projector = projector(1000);
        if typescript {
            projector.dialect = Arc::new(crate::dialect::typescript_test_dialect());
        }
        projector.prompt_features.images = false;
        projector.prompt_features.decomposition = false;
        projector.max_budget_tokens = Some(100);
        *projector.last_prompt_usage.write_recover() = Some(lash_core::PromptUsage {
            context_budget_tokens: 100,
            ..Default::default()
        });
        let request =
            project_iteration_request(&projector, &[step_event(0, "1", "1")], 1, "test-model");
        let tail = message_text(request.messages.last().unwrap());
        assert!(tail.contains("type HistoryItem ="), "{tail}");
        assert!(tail.contains("finish concisely"), "{tail}");
        for forbidden in ["HistoryImage", "images?", "continue_as"] {
            assert!(!tail.contains(forbidden), "{forbidden}: {tail}");
        }
    }
}
