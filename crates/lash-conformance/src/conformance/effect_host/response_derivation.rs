use super::*;
use pretty_assertions::assert_eq;

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
                stream_hook_states: Vec::new(),
            },
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectAssistantResponseHook,
                "ordinary hook-code error",
            ),
        ),
        (
            RuntimeEffectCommand::LanguageRuntimeValue {
                operation: "effect".into(),
            },
            RuntimeEffectControllerError::retryable_response_derivation("wrong command"),
        ),
        (
            RuntimeEffectCommand::AssistantResponseHooks {
                response: Box::default(),
                stream_hook_states: Vec::new(),
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
        let mut envelope = journaled_conformance_envelope(
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
