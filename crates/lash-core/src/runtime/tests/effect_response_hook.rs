use super::effect::{RecordingEffectController, host_with_effect_recorder, scoped_test_turn};
use super::*;
use lash_sansio::sync::MutexExt;

#[tokio::test]
async fn fallible_response_hook_journals_error_and_fresh_retry_calls_provider_twice() {
    let provider_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hook_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let transport = TestProvider::builder()
        .kind("mock")
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            move |_| {
                let provider_calls = Arc::clone(&provider_calls);
                async move {
                    let call = provider_calls.fetch_add(1, Ordering::SeqCst) + 1;
                    let text = format!("paid completion {call}");
                    Ok(LlmResponse {
                        full_text: text.clone(),
                        parts: vec![LlmOutputPart::Text {
                            text,
                            response_meta: None,
                        }],
                        terminal_reason: crate::LlmTerminalReason::Stop,
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let plugin: Arc<dyn crate::PluginFactory> = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new({
            let hook_calls = Arc::clone(&hook_calls);
            move |_| {
                let hook_calls = Arc::clone(&hook_calls);
                Ok(Arc::new(RuntimeTestPlugin {
                    before_turn: None,
                    checkpoint: None,
                    tool_result_projector: None,
                    runtime_event: None,
                    external_registrar: Some(Arc::new(move |reg| {
                        let hook_calls = Arc::clone(&hook_calls);
                        reg.output().response(Arc::new(move |context| {
                            let hook_calls = Arc::clone(&hook_calls);
                            Box::pin(async move {
                                if hook_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                                    return Err(crate::PluginError::Invoke(
                                        "injected assistant response hook failure".to_string(),
                                    ));
                                }
                                Ok(crate::AssistantResponseTransform {
                                    response: context.response,
                                    events: Vec::new(),
                                })
                            })
                        }));
                        Ok(())
                    })),
                }))
            }
        }),
    });
    let recorder = RecordingEffectController::default()
        .with_local_llm_execution()
        .with_replay_by_key();
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        vec![plugin],
        Arc::new(EmptyTools),
        transport,
        host_with_effect_recorder(recorder.clone()),
    )
    .await;

    let failed = runtime
        .run_turn_assembled(
            TurnInput::text("produce a completion"),
            CancellationToken::new(),
            scoped_test_turn(&recorder, "response-hook-failure"),
        )
        .await
        .expect("hook failure is an assembled provider-error turn");

    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        failed.outcome,
        TurnOutcome::Stopped(TurnStop::ProviderError)
    ));
    assert!(failed.assistant_output.safe_text.is_empty());
    assert!(
        active_conversation_messages(&failed.state)
            .iter()
            .flat_map(|message| message.parts.iter())
            .all(|part| !part.content.contains("paid completion 1")),
        "the completed provider response must not survive in committed history"
    );
    assert_eq!(failed.llm_calls.len(), 1);
    assert_eq!(failed.llm_calls[0].attempts.len(), 1);
    assert_eq!(
        failed.llm_calls[0].attempts[0].outcome,
        crate::AttemptOutcome::Completed,
        "the attempt ledger retains that the provider completed"
    );

    let journaled_llm = recorder
        .replay_outcomes
        .lock_recover()
        .values()
        .find(|outcome| matches!(outcome, RuntimeEffectOutcome::LlmCall { .. }))
        .cloned()
        .expect("journaled LLM outcome");
    let RuntimeEffectOutcome::LlmCall {
        result,
        call_record,
        ..
    } = journaled_llm
    else {
        unreachable!("selected LLM outcome")
    };
    assert!(matches!(
        result.as_ref(),
        Err(error) if error.code.as_deref() == Some("plugin_assistant_response")
    ));
    assert_eq!(
        call_record
            .as_ref()
            .and_then(|record| record.attempts.first())
            .map(|attempt| attempt.outcome),
        Some(crate::AttemptOutcome::Completed)
    );

    let retried = runtime
        .run_turn_assembled(
            TurnInput::text("produce a completion"),
            CancellationToken::new(),
            scoped_test_turn(&recorder, "response-hook-logical-retry"),
        )
        .await
        .expect("fresh logical retry succeeds once the injected fault clears");

    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    assert_eq!(hook_calls.load(Ordering::SeqCst), 2);
    assert_eq!(retried.assistant_output.safe_text, "paid completion 2");
}
