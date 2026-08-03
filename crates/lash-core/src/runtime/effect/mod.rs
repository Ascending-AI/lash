mod await_events;
mod envelope;
pub(crate) mod executor;
mod inline_host;
mod outcome;
pub mod promise_semantics;
mod validation;

pub use envelope::{
    LlmAttachmentSpec, LlmRequestSpec, ProcessCommand, ProcessEffectOutcome,
    RuntimeDirectLlmOutcome, RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectKind,
    RuntimeEffectOutcome, RuntimeInvocation, RuntimeLlmCallOutcome, RuntimeReplay, RuntimeScope,
    RuntimeSubject, ToolAttemptEffectOutcome, ToolAttemptLaunch, ToolBatchEffectOutcome,
    ToolCallLaunch,
};
pub use executor::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, BoundaryReason, EffectHost,
    EffectJournalIdentity, EffectJournalRetirement, ExecutionScope, ExternalCompletionError,
    InlineRuntimeEffectController, ProcessTurnCancellation, Resolution, ResolveOutcome,
    RuntimeAwaitEventOptions, RuntimeEffectController, RuntimeEffectControllerError,
    RuntimeEffectLocalExecutor, RuntimeSleepOptions, ScopedEffectController, SegmentProgress,
};
pub use inline_host::InlineEffectHost;
pub use lash_sansio::CausalRef;
pub use validation::{
    CanonicalRuntimeEffectEnvelope, RuntimeEffectReplayMismatchSummary, RuntimeEffectReplayTrace,
    validate_replayed_effect_envelope,
};

pub(crate) use executor::{
    EffectTaskController, ProcessRunner, RuntimeEffectControllerHandle, TurnEffectStateUpdate,
    drive_effect_controller_task,
};
pub(crate) use outcome::{
    LlmTraceFailure, apply_direct_outcome, emit_llm_trace_completed, emit_llm_trace_failed,
    emit_llm_trace_started, token_usage_from_llm,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmRequest as CoreLlmRequest;
    use crate::llm::types::{
        AttachmentSource, LlmEventSender, LlmMessage, LlmProviderTraceSender, LlmToolChoice,
    };
    use std::sync::Arc;

    struct ControllerOwnedReplay;

    impl AwaitEventResolver for ControllerOwnedReplay {
        fn replay_ownership(&self) -> crate::EffectReplayOwnership {
            crate::EffectReplayOwnership::Controller
        }
    }

    #[async_trait::async_trait]
    impl RuntimeEffectController for ControllerOwnedReplay {
        async fn execute_effect(
            &self,
            _envelope: RuntimeEffectEnvelope,
            _local_executor: RuntimeEffectLocalExecutor<'_>,
        ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
            unreachable!("ownership propagation does not execute effects")
        }
    }

    #[test]
    fn inline_host_and_scoped_controller_forward_replay_ownership_without_inferring_key_lifetime() {
        let host = InlineEffectHost::new(Arc::new(ControllerOwnedReplay));
        assert_eq!(
            host.replay_ownership(),
            crate::EffectReplayOwnership::Controller
        );
        assert!(!host.allows_process_lifetime_completion_keys());
        let scoped = host
            .scoped(ExecutionScope::turn("ownership-session", "ownership-turn"))
            .expect("scope wrapped controller");
        assert_eq!(
            scoped.controller().replay_ownership(),
            crate::EffectReplayOwnership::Controller
        );
        assert!(
            !scoped
                .controller()
                .allows_process_lifetime_completion_keys()
        );
    }

    #[tokio::test]
    async fn runtime_effect_envelope_and_request_specs_round_trip_without_live_fields() {
        let attachment_store = crate::SessionAttachmentStore::in_memory();
        let llm_request = CoreLlmRequest {
            model: "model".to_string(),
            messages: vec![LlmMessage::text(crate::llm::types::LlmRole::User, "hello")],
            attachments: vec![AttachmentSource::inline(
                crate::MediaType::parse("image/png").unwrap(),
                vec![1, 2, 3, 4],
            )],
            resolved_stored: Default::default(),
            tools: Arc::new(Vec::new()),
            tool_choice: LlmToolChoice::None,
            model_variant: crate::ReasoningSelection::Effort("fast".to_string()),
            model_capability: crate::ModelCapability::default(),
            scope: crate::LlmRequestScope::new(
                "session",
                "session:frame:test",
                "session:turn:test:llm:0",
            ),
            output_spec: None,
            stream_events: Some(LlmEventSender::new(|_| {})),
            generation: crate::GenerationOptions::default(),
            provider_trace: Some(LlmProviderTraceSender::new(|_| {})),
        };
        let spec = LlmRequestSpec::from_request(&llm_request, &attachment_store)
            .await
            .expect("llm spec");
        let encoded = serde_json::to_string(&spec).expect("serialize llm spec");
        assert!(!encoded.contains("stream_events"));
        assert!(!encoded.contains("provider_trace"));
        assert!(!encoded.contains("\"data\""));
        assert!(encoded.contains(crate::attachments::content_id(&[1, 2, 3, 4]).as_str()));
        let decoded: LlmRequestSpec = serde_json::from_str(&encoded).expect("decode llm spec");
        let live = decoded.into_request(None, None);
        assert_eq!(live.model, "model");
        assert!(matches!(
            live.attachments[0],
            AttachmentSource::Stored { .. }
        ));
        assert!(live.stream_events.is_none());
        assert!(live.provider_trace.is_none());

        let invocation = crate::runtime::causal::direct_effect_invocation(
            "session",
            "test",
            "request:direct".to_string(),
            Some("turn"),
            None,
        );
        let envelope = RuntimeEffectEnvelope::new(
            invocation,
            RuntimeEffectCommand::Direct {
                request: Box::new(
                    LlmRequestSpec::from_request(&llm_request, &attachment_store)
                        .await
                        .expect("normalized spec"),
                ),
                usage_source: "test".to_string(),
            },
        );
        let hash = envelope.stable_hash().expect("stable hash");
        assert!(!hash.is_empty());
        let encoded = serde_json::to_string(&envelope).expect("serialize envelope");
        let decoded: RuntimeEffectEnvelope =
            serde_json::from_str(&encoded).expect("decode envelope");
        assert_eq!(
            decoded.invocation.replay_key(),
            envelope.invocation.replay_key()
        );
        assert_eq!(decoded.command.kind(), RuntimeEffectKind::Direct);
    }

    #[tokio::test]
    async fn inline_host_owns_registry_and_shares_it_only_with_its_scoped_controllers() {
        let host_a = InlineEffectHost::default();
        let host_b = InlineEffectHost::default();
        let scope = ExecutionScope::turn("owned-inline-session", "owned-inline-turn");
        let key = host_a
            .await_event_key(
                &scope,
                AwaitEventWaitIdentity::tool_completion("owned-inline-call"),
            )
            .await
            .expect("host A key");
        let scoped_a = host_a.scoped(scope).expect("host A scoped controller");
        let terminal = Resolution::Ok(serde_json::json!("owned"));
        assert_eq!(
            scoped_a
                .controller()
                .resolve_await_event(&key, terminal.clone())
                .await
                .expect("scoped A resolves host A key"),
            ResolveOutcome::Accepted
        );
        assert_eq!(
            host_a
                .peek_await_event(&key)
                .await
                .expect("host A observes scoped terminal"),
            Some(terminal)
        );
        assert_eq!(
            host_b
                .resolve_await_event(&key, Resolution::Cancelled)
                .await
                .expect("independent host rejects key"),
            ResolveOutcome::UnknownOrRevoked,
            "independent Inline hosts must not rendezvous through process-global state"
        );
    }

    #[tokio::test]
    async fn resolver_defaults_refuse_turn_control_without_an_explicit_host() {
        struct UnsupportedResolver;
        impl AwaitEventResolver for UnsupportedResolver {}

        let resolver = UnsupportedResolver;
        let scope = ExecutionScope::turn("unsupported-session", "unsupported-turn");
        for wait in [
            AwaitEventWaitIdentity::TurnCancelGate,
            AwaitEventWaitIdentity::TurnTerminal,
            AwaitEventWaitIdentity::tool_completion("unsupported-call"),
        ] {
            let error = resolver
                .await_event_key(&scope, wait)
                .await
                .expect_err("default resolver must refuse every identity");
            assert_eq!(error.code.as_str(), "await_event_unsupported");
        }

        let key = InlineRuntimeEffectController::default()
            .await_event_key(&scope, AwaitEventWaitIdentity::TurnCancelGate)
            .await
            .expect("explicit inline controller key");
        assert_eq!(
            resolver
                .resolve_await_event(&key, Resolution::Cancelled)
                .await
                .expect("default resolution has one opaque shape"),
            ResolveOutcome::UnknownOrRevoked
        );
        for error in [
            resolver
                .peek_await_event(&key)
                .await
                .expect_err("default resolver must refuse reads"),
            resolver
                .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None)
                .await
                .expect_err("default resolver must refuse waits"),
            resolver
                .revoke_await_events_for_session("unsupported-session")
                .await
                .expect_err("default resolver must refuse revocation"),
        ] {
            assert_eq!(error.code.as_str(), "await_event_unsupported");
        }
    }

    #[test]
    fn process_effect_envelope_round_trips_prepared_tool_call() {
        let registration = crate::ProcessRegistration::new(
            "call-123",
            crate::ProcessInput::ToolCall {
                call: crate::PreparedToolCall {
                    call_id: "call-123".to_string(),
                    tool_id: crate::ToolId::from("tool:echo"),
                    tool_name: "echo".to_string(),
                    args: serde_json::json!({"value": "hi"}),
                    replay: None,
                    prepared_payload: serde_json::json!({"context": "prepared"}),
                },
            },
            crate::RecoveryDisposition::Rerunnable,
            crate::ProcessProvenance::host(),
        );
        let invocation = RuntimeInvocation::effect(
            RuntimeScope::for_turn("session", "turn", 0, 0),
            "process:start:call-123",
            RuntimeEffectKind::Process,
            "session:turn:process:start:call-123",
        );
        let envelope = RuntimeEffectEnvelope::new(
            invocation,
            RuntimeEffectCommand::process(ProcessCommand::Start {
                registration,
                observers: Vec::new(),
                execution_context: Box::new(crate::ProcessExecutionContext::default()),
            }),
        );

        let hash = envelope.stable_hash().expect("hash");
        let decoded: RuntimeEffectEnvelope =
            serde_json::from_str(&serde_json::to_string(&envelope).expect("serialize"))
                .expect("decode");

        assert_eq!(decoded.command.kind(), RuntimeEffectKind::Process);
        assert_eq!(decoded.stable_hash().expect("decoded hash"), hash);
        let RuntimeEffectCommand::Process { command } = decoded.command else {
            panic!("wrong process command");
        };
        let ProcessCommand::Start {
            registration,
            observers,
            execution_context,
        } = *command
        else {
            panic!("wrong process command");
        };
        assert!(observers.is_empty());
        assert!(execution_context.is_empty());
        let crate::ProcessInput::ToolCall { call } = registration.input.as_ref() else {
            panic!("wrong process input");
        };
        assert_eq!(call.call_id, "call-123");
        assert_eq!(call.tool_name, "echo");
        assert_eq!(call.args, serde_json::json!({"value": "hi"}));
        assert_eq!(
            call.prepared_payload,
            serde_json::json!({"context": "prepared"})
        );
    }

    fn prepared_tool_call(call_id: &str, tool_name: &str) -> crate::PreparedToolCall {
        crate::PreparedToolCall {
            call_id: call_id.to_string(),
            tool_id: crate::ToolId::from(format!("tool:{tool_name}")),
            tool_name: tool_name.to_string(),
            args: serde_json::json!({"value": call_id}),
            replay: None,
            prepared_payload: serde_json::json!({"prepared": true}),
        }
    }

    #[test]
    fn tool_batch_effect_envelope_round_trips_and_hashes_stably() {
        let batch = crate::PreparedToolBatch::new(
            "batch-123",
            vec![
                prepared_tool_call("call-1", "echo"),
                prepared_tool_call("call-2", "lookup"),
            ],
        );
        let invocation = RuntimeInvocation::effect(
            RuntimeScope::for_turn("session", "turn", 0, 0),
            "tool-batch:batch-123",
            RuntimeEffectKind::ToolBatch,
            "session:turn:tool-batch:batch-123",
        );
        let envelope = RuntimeEffectEnvelope::new(
            invocation,
            RuntimeEffectCommand::ToolBatch {
                batch: batch.clone(),
            },
        );

        let hash = envelope.stable_hash().expect("hash");
        let decoded: RuntimeEffectEnvelope =
            serde_json::from_str(&serde_json::to_string(&envelope).expect("serialize"))
                .expect("decode");

        assert_eq!(decoded.command.kind(), RuntimeEffectKind::ToolBatch);
        assert_eq!(decoded.stable_hash().expect("decoded hash"), hash);
        let RuntimeEffectCommand::ToolBatch {
            batch: decoded_batch,
        } = decoded.command
        else {
            panic!("wrong command");
        };
        assert_eq!(decoded_batch.batch_id, batch.batch_id);
        assert_eq!(decoded_batch.calls.len(), 2);
        assert_eq!(decoded_batch.calls[0].call.call_id, "call-1");
        assert_eq!(decoded_batch.calls[0].replay_suffix, "child:0:call-1");
        assert_eq!(decoded_batch.calls[1].call.call_id, "call-2");
        assert_eq!(decoded_batch.calls[1].replay_suffix, "child:1:call-2");
    }

    #[test]
    fn tool_batch_outcome_rejects_wrong_effect_kind() {
        let error = RuntimeEffectOutcome::ToolAttempt {
            launch: Box::new(ToolAttemptLaunch::Done {
                record: Box::new(crate::ToolCallRecord {
                    call_id: Some("call-1".to_string()),
                    tool: "echo".to_string(),
                    args: serde_json::json!({"value": "call-1"}),
                    output: crate::ToolCallOutput::success(serde_json::json!({"done": "call-1"})),
                    duration_ms: 7,
                }),
            }),
            triggers: Vec::new(),
        }
        .into_tool_batch_effect()
        .expect_err("tool attempt is not a tool batch outcome");

        assert_eq!(error.code, "runtime_effect_wrong_outcome");
        assert!(error.message.contains("expected tool_batch outcome"));
        assert!(error.message.contains("got tool_attempt"));
    }

    #[tokio::test]
    async fn await_event_key_is_stable_for_scope_and_wait_identity() {
        let host = InlineEffectHost::default();
        let scope = ExecutionScope::turn("session", "turn");
        let wait = AwaitEventWaitIdentity::tool_completion("call");

        let first = host
            .await_event_key(&scope, wait.clone())
            .await
            .expect("first key");
        let second = host
            .await_event_key(&scope, wait)
            .await
            .expect("second key");

        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn duplicate_await_event_resolution_reports_existing_terminal() {
        let host = InlineEffectHost::default();
        let scope = ExecutionScope::turn("session-dupe", "turn-dupe");
        let key = host
            .await_event_key(&scope, AwaitEventWaitIdentity::tool_completion("call-dupe"))
            .await
            .expect("key");
        let resolution = Resolution::Ok(serde_json::json!({"done": true}));

        let first = host
            .resolve_await_event(&key, resolution.clone())
            .await
            .expect("first resolve");
        let second = host
            .resolve_await_event(&key, Resolution::Ok(serde_json::json!({"ignored": true})))
            .await
            .expect("duplicate resolve");

        assert_eq!(first, ResolveOutcome::Accepted);
        assert_eq!(
            second,
            ResolveOutcome::AlreadyResolved {
                terminal: resolution
            }
        );
    }
}
