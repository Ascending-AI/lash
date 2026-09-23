use super::*;
use pretty_assertions::assert_eq;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A failed response derivation retries without repeating the recorded completion.
#[expect(
    clippy::expect_used,
    reason = "conformance fixture asserts its public outcomes"
)]
pub async fn effect_controller_response_derivation_retry<F>(make: F)
where
    F: FnOnce() -> ConformanceInvocation,
{
    let invocation = make();
    let scope = invocation.execution_scope().clone();
    let mut completion = exec_code_conformance_envelope(&scope, "completion", "unused");
    completion.command = RuntimeEffectCommand::LlmCall {
        request: Box::new(crate::LlmRequestSpec {
            instructions: None,
            model: "counting-provider".to_string(),
            messages: Vec::new(),
            tools: Arc::new(Vec::new()),
            tool_choice: Default::default(),
            model_variant: Default::default(),
            model_capability: Default::default(),
            generation: Default::default(),
            scope: crate::LlmRequestScope::new("journaled-session", "frame", "completion"),
            output_spec: None,
        }),
    };
    let mut hook = exec_code_conformance_envelope(&scope, "response-hook", "unused");
    hook.command = RuntimeEffectCommand::AssistantResponseHooks {
        response: Box::default(),
    };
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::new(AtomicUsize::new(0));
    async fn run(
        controller: &dyn RuntimeEffectController,
        completion: RuntimeEffectEnvelope,
        hook: RuntimeEffectEnvelope,
        provider_calls: Arc<AtomicUsize>,
        hook_calls: Arc<AtomicUsize>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        controller
            .execute_effect(
                completion,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    provider_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(RuntimeEffectOutcome::LlmCall {
                        result: Box::new(Ok(Default::default())),
                        text_streamed: false,
                        call_record: None,
                    })
                }),
            )
            .await?;
        controller
            .execute_effect(
                hook,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    if hook_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Err(RuntimeEffectControllerError::retryable_response_derivation(
                            "transient response derivation failure",
                        ));
                    }
                    Ok(RuntimeEffectOutcome::AssistantResponseHooks {
                        response: Box::default(),
                        events: Vec::new(),
                    })
                }),
            )
            .await
    }
    let first = run(
        invocation.controller(),
        completion.clone(),
        hook.clone(),
        provider_calls.clone(),
        hook_calls.clone(),
    )
    .await;
    assert_eq!(
        first.expect_err("first hook fails").code,
        crate::RuntimeErrorCode::RuntimeEffectAssistantResponseHook
    );
    let invocation = invocation.redrive();
    let recovered = run(
        invocation.controller(),
        completion,
        hook,
        provider_calls.clone(),
        hook_calls.clone(),
    )
    .await;
    recovered.expect("the uncommitted derivation must execute again and recover");
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        1,
        "phase 1 must replay"
    );
    assert_eq!(hook_calls.load(Ordering::SeqCst), 2, "phase 2 must retry");
}

/// Retry authority must come from the derivation executor and match its command.
#[expect(
    clippy::expect_used,
    reason = "conformance fixture asserts its public outcomes"
)]
pub async fn effect_controller_response_derivation_terminals<F>(make: F)
where
    F: FnOnce() -> ConformanceInvocation,
{
    let invocation = make();
    let mut recorded = Vec::new();
    for (index, (command, error)) in [
        (
            RuntimeEffectCommand::AssistantResponseHooks {
                response: Box::default(),
            },
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectAssistantResponseHook,
                "ordinary hook-code error",
            ),
        ),
        (
            RuntimeEffectCommand::ExecCode {
                language: "conformance".into(),
                code: "effect".into(),
            },
            RuntimeEffectControllerError::retryable_response_derivation("wrong command"),
        ),
        (
            RuntimeEffectCommand::AssistantResponseHooks {
                response: Box::default(),
            },
            serde_json::from_value(
                serde_json::to_value(RuntimeEffectControllerError::retryable_response_derivation(
                    "decoded terminal",
                ))
                .expect("encode"),
            )
            .expect("decode"),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mut envelope = exec_code_conformance_envelope(
            invocation.execution_scope(),
            &format!("terminal-{index}"),
            "unused",
        );
        envelope.command = command;
        let first_error = error.clone();
        invocation
            .controller()
            .execute_effect(
                envelope.clone(),
                RuntimeEffectLocalExecutor::testing(move |_| async move { Err(first_error) }),
            )
            .await
            .expect_err("terminal failure");
        recorded.push((envelope, error));
    }
    let invocation = invocation.redrive();
    for (envelope, error) in recorded {
        let replayed = invocation
            .controller()
            .execute_effect(
                envelope,
                RuntimeEffectLocalExecutor::testing(|_| async {
                    panic!("a terminal error must replay")
                }),
            )
            .await
            .expect_err("terminal replays");
        assert_eq!(replayed.code, error.code);
        assert_eq!(replayed.message, error.message);
    }
}
