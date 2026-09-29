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

/// The ledger is part of the result on success and failure, including absent usage.
#[expect(
    clippy::expect_used,
    reason = "conformance fixture asserts recorded outcomes"
)]
pub async fn attempt_history_terminal_variants_survive_result_replay<F>(make: F)
where
    F: FnOnce() -> ConformanceInvocation,
{
    fn ledger(outcome: &RuntimeEffectOutcome) -> &crate::LlmCallRecord {
        let RuntimeEffectOutcome::LlmCall {
            call_record: Some(record),
            ..
        } = outcome
        else {
            panic!("a model result retains its typed attempt ledger");
        };
        record
    }
    use crate::{AttemptOutcome, AttemptRecord, AttemptUsageOutcome, ProtocolPosition};
    let invocation = make();
    let mut recorded = Vec::new();
    for (index, terminal) in [
        AttemptOutcome::Completed,
        AttemptOutcome::Failed,
        AttemptOutcome::Aborted,
        AttemptOutcome::Interrupted,
    ]
    .into_iter()
    .enumerate()
    {
        let key = format!("attempt-terminal-{index}");
        let mut envelope =
            journaled_conformance_envelope(invocation.execution_scope(), &key, "unused");
        envelope.command = RuntimeEffectCommand::LlmCall {
            provider_id: "attempt-ledger".into(),
            request: Box::new(crate::runtime::LlmRequestSpec {
                instructions: None,
                model: "ledger-model".into(),
                messages: Vec::new(),
                tools: Arc::new(Vec::new()),
                tool_choice: crate::llm::types::LlmToolChoice::None,
                model_variant: Default::default(),
                model_capability: Default::default(),
                extra_body: Default::default(),
                generation: Default::default(),
                scope: crate::LlmRequestScope::new("session", "session:frame", &key),
                output_spec: None,
            }),
        };
        let record = crate::LlmCallRecord {
            call_id: crate::LlmCallId(key),
            label: Some("ledger-proof".into()),
            replay_drops: Vec::new(),
            attempts: [AttemptOutcome::Failed, terminal]
                .into_iter()
                .enumerate()
                .map(|(ordinal, outcome)| {
                    let usage = (ordinal == 1).then(crate::llm::types::LlmUsage::default);
                    AttemptRecord {
                        ordinal: ordinal as u32 + 1,
                        outcome,
                        protocol_position: if ordinal == 0 {
                            ProtocolPosition::OutputStarted
                        } else {
                            ProtocolPosition::TerminalObserved
                        },
                        retry_budget_consumed: ordinal == 0,
                        retry_decision: Some(crate::RetryDecision {
                            scheduled: ordinal == 0,
                            delay: (ordinal == 0).then(|| Duration::from_millis(31)),
                            reason: Some("transport".into()),
                            charge_safety: None,
                        }),
                        error: (outcome == AttemptOutcome::Failed).then(|| {
                            crate::NormalizedError {
                                class: "transport".into(),
                                code: Some(crate::FailureCode::provider("overloaded")),
                                http_status: Some(503),
                                provider_request_id: Some(format!("provider-{index}-{ordinal}")),
                                retry_after: Some(Duration::from_millis(31)),
                            }
                        }),
                        evidence: None,
                        generation_disposition: None,
                        usage_disposition: AttemptUsageOutcome::for_attempt(
                            outcome,
                            usage.as_ref(),
                        ),
                        usage,
                    }
                })
                .collect(),
        };
        let result = if terminal == AttemptOutcome::Failed {
            Err(crate::sansio::LlmCallError {
                message: "provider call failed".into(),
                retryable: false,
                kind: crate::ProviderFailureKind::Transport,
                raw: None,
                code: Some(crate::FailureCode::provider("overloaded")),
                terminal_reason: crate::LlmTerminalReason::ProviderError,
                request_body: None,
                partial_response: None,
            })
        } else {
            Ok(crate::LlmResponse {
                terminal_reason: match terminal {
                    AttemptOutcome::Aborted => crate::LlmTerminalReason::Cancelled,
                    AttemptOutcome::Interrupted => crate::LlmTerminalReason::Unknown,
                    _ => crate::LlmTerminalReason::Stop,
                },
                ..Default::default()
            })
        };
        let expected_record = record.clone();
        let outcome = RuntimeEffectOutcome::LlmCall {
            result: Box::new(result),
            text_streamed: true,
            call_record: Some(record),
            stream: Box::default(),
        };
        let expected = serde_json::to_value(&outcome).expect("result JSON");
        let first = invocation
            .controller()
            .execute_effect(
                envelope.clone(),
                RuntimeEffectLocalExecutor::testing(move |_| async move { Ok(outcome) }),
            )
            .await
            .expect("record result");
        assert_eq!(ledger(&first), &expected_record);
        assert_eq!(serde_json::to_value(first).expect("live JSON"), expected);
        recorded.push((envelope, expected, expected_record));
    }
    let invocation = invocation.redrive();
    for (envelope, expected, expected_record) in recorded {
        let replayed = invocation
            .controller()
            .execute_effect(
                envelope,
                RuntimeEffectLocalExecutor::testing(|_| async {
                    panic!("a recorded result must never re-invoke the provider")
                }),
            )
            .await
            .expect("replay result");
        assert_eq!(ledger(&replayed), &expected_record);
        assert_eq!(
            serde_json::to_value(replayed).expect("replayed JSON"),
            expected
        );
    }
    invocation.end();
}
