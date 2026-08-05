use super::effect::{RecordingEffectController, host_with_effect_recorder, scoped_test_turn};
use super::*;

#[tokio::test]
async fn lifecycle_hook_concurrency_rejection_is_host_observable() {
    let gate = Arc::new((
        tokio::sync::Notify::new(),
        tokio::sync::Notify::new(),
        AtomicBool::new(true),
    ));
    let recorder = RecordingEffectController::default().with_direct_gate(Arc::clone(&gate));
    let hook_gate = Arc::clone(&gate);
    let plugin: Arc<dyn crate::PluginFactory> = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(move |_| {
            let hook_gate = Arc::clone(&hook_gate);
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: None,
                tool_result_projector: None,
                runtime_event: Some(Arc::new(move |event| {
                    let hook_gate = Arc::clone(&hook_gate);
                    Box::pin(async move {
                        let crate::PluginLifecycleEvent::TurnPersisted(ctx) = event else {
                            return Ok(());
                        };
                        let first = ctx.direct_completions.clone();
                        let second = ctx.direct_completions.clone();
                        let (first_result, overlap_result) = tokio::join! {
                            biased;
                            first.direct_completion(
                                crate::DirectRequest::text("mock-model", "first"),
                                "same-plugin-hook",
                            ),
                            async {
                                hook_gate.0.notified().await;
                                let result = second.direct_completion(
                                    crate::DirectRequest::text("mock-model", "overlap"),
                                    "same-plugin-hook",
                                ).await;
                                hook_gate.1.notify_one();
                                result
                            },
                        };
                        first_result?;
                        overlap_result?;
                        Ok(())
                    })
                })),
                external_registrar: None,
            }))
        }),
    });
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        vec![plugin],
        Arc::new(EmptyTools),
        mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "finished".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "finished".to_string(),
                    response_meta: None,
                }],
                ..LlmResponse::default()
            }),
        }]),
        host_with_effect_recorder(recorder.clone()),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            scoped_test_turn(&recorder, "hook-error-surfacing"),
        )
        .await
        .expect("turn remains committed despite an observer-hook failure");

    assert!(turn.errors.iter().any(|issue| {
        issue.kind == "plugin"
            && issue.code.as_deref() == Some("lifecycle_hook_failed")
            && issue.retryable == Some(false)
            && issue.message.contains("explicit replay keys")
    }));
    assert!(
        !runtime.resident_session_state_valid,
        "a failed post-commit hook invalidates resident plugin state"
    );
}
