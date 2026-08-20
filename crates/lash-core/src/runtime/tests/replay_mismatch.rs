use super::effect::{RejectingEffectController, runtime_host_config_with_inline_controller};
use super::*;

#[tokio::test]
async fn controller_owned_replay_mismatch_reaches_host_with_structured_summary() {
    let controller = Arc::new(RejectingEffectController::default().with_replay_mismatch());
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(runtime_host_config_with_inline_controller(
            controller.clone(),
        )),
    )
    .await;

    let error = runtime
        .run_turn_assembled(
            TurnInput::text("hello"),
            CancellationToken::new(),
            ScopedEffectController::shared(
                controller,
                ExecutionScope::turn("root", "replay-mismatch-controller"),
            )
            .expect("replay-mismatch execution scope"),
        )
        .await
        .expect_err("controller-owned replay mismatch must abort to the host");

    assert!(error.code.is_replay_mismatch());
    assert_eq!(
        error.summary,
        Some(RuntimeEffectReplayMismatchReport {
            divergent_path_count: 1,
            first_divergent_paths: vec!["command.request.model".to_string()],
        }),
        "the outer host error must retain the controller's structured divergence summary"
    );
}
