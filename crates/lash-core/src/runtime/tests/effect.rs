use super::*;
use crate::facade_support::SessionGraphFacadeOps;
use crate::llm::types::{AttachmentSource, LlmContentBlock, LlmMessage, LlmRole, LlmToolChoice};
use crate::plugin::{ProtocolDriverPlugin, ProtocolSessionPlugin};
use lash_sansio::sync::MutexExt;
mod controller_doubles;
pub(in crate::runtime::tests) use controller_doubles::RejectingEffectController;
use controller_doubles::{SerialOnlyEffectController, WrongOutcomeEffectController};
mod fig1127;
mod fig1416;
mod fig1535;
#[derive(Clone, Debug)]
struct EffectControllerRecord {
    kind: RuntimeEffectKind,
    turn_id: Option<String>,
    replay_key: String,
}
#[derive(Clone, Default)]
pub(super) struct RecordingEffectController {
    records: Arc<Mutex<Vec<EffectControllerRecord>>>,
    envelopes: Arc<Mutex<Vec<String>>>,
    llm_calls: Arc<Mutex<usize>>,
    inline: InlineRuntimeEffectController,
    cancel_after_llm: bool,
    controller_owned_replay: bool,
    durable_workflow_controller: bool,
    replay_by_key: bool,
    execute_llm_locally: bool,
    /// Model a host crash in the window between the journaled raw provider
    /// completion (phase 1) and hook post-processing (phase 2).
    crash_before_first_response_hooks: bool,
    response_hook_crash_fired: Arc<std::sync::atomic::AtomicBool>,
    pub(super) replay_outcomes:
        Arc<Mutex<std::collections::BTreeMap<String, RuntimeEffectOutcome>>>,
    direct_gate: Option<
        Arc<(
            tokio::sync::Notify,
            tokio::sync::Notify,
            std::sync::atomic::AtomicBool,
        )>,
    >,
}

impl RecordingEffectController {
    fn with_cancel_after_llm(mut self) -> Self {
        self.cancel_after_llm = true;
        self
    }

    pub(super) fn with_controller_owned_replay(mut self) -> Self {
        self.controller_owned_replay = true;
        self
    }

    /// Opt into the durable-workflow-controller capability the aliveness-aware
    /// queued-drain busy policy keys on. Deliberately separate from
    /// [`with_controller_owned_replay`](Self::with_controller_owned_replay), so
    /// a test can hold the two axes apart exactly as the product does.
    pub(super) fn with_durable_workflow_controller(mut self) -> Self {
        self.durable_workflow_controller = true;
        self
    }

    pub(super) fn with_replay_by_key(mut self) -> Self {
        self.replay_by_key = true;
        self
    }

    pub(super) fn with_local_llm_execution(mut self) -> Self {
        self.execute_llm_locally = true;
        self
    }

    /// Fail the first assistant-response-hooks effect without executing it, so
    /// a test can stand where a crashed host would: phase 1 durable, phase 2
    /// never committed.
    pub(super) fn with_crash_before_first_response_hooks(mut self) -> Self {
        self.crash_before_first_response_hooks = true;
        self
    }

    pub(super) fn with_direct_gate(
        mut self,
        gate: Arc<(
            tokio::sync::Notify,
            tokio::sync::Notify,
            std::sync::atomic::AtomicBool,
        )>,
    ) -> Self {
        self.direct_gate = Some(gate);
        self
    }

    fn records(&self) -> Vec<EffectControllerRecord> {
        self.records.lock_recover().clone()
    }

    pub(super) fn envelopes(&self) -> Vec<String> {
        self.envelopes.lock_recover().clone()
    }

    pub(super) fn count_kind(&self, kind: RuntimeEffectKind) -> usize {
        self.records()
            .iter()
            .filter(|record| record.kind == kind)
            .count()
    }

    fn record(&self, invocation: &RuntimeInvocation) {
        self.records.lock_recover().push(EffectControllerRecord {
            kind: invocation.effect_kind().expect("effect kind"),
            turn_id: invocation.scope.turn_id.clone(),
            replay_key: invocation.replay_key().expect("replay key").to_string(),
        });
    }
}

pub(super) fn runtime_host_config_with_inline_controller(
    controller: Arc<dyn RuntimeEffectController>,
) -> RuntimeHostConfig {
    let mut config = test_runtime_host_config();
    config.control.effect_host = Arc::new(InlineEffectHost::new(controller));
    config
}

pub(super) fn scoped_test_turn<'a>(
    controller: &'a dyn RuntimeEffectController,
    turn_id: &str,
) -> ScopedEffectController<'a> {
    ScopedEffectController::borrowed(
        controller,
        ExecutionScope::turn("effect-test-session", turn_id),
    )
    .expect("scoped effect controller")
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for RecordingEffectController {
    fn replay_ownership(&self) -> crate::EffectReplayOwnership {
        if self.controller_owned_replay {
            crate::EffectReplayOwnership::Controller
        } else {
            crate::EffectReplayOwnership::Runtime
        }
    }

    fn durable_workflow_controller(&self) -> bool {
        self.durable_workflow_controller
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.inline.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.inline.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.inline.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.inline.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inline
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inline
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for RecordingEffectController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let replay_key = envelope
            .invocation
            .replay_key()
            .expect("replay key")
            .to_string();
        if self.replay_by_key
            && let Some(outcome) = self
                .replay_outcomes
                .lock_recover()
                .get(&replay_key)
                .cloned()
        {
            return Ok(outcome);
        }
        self.envelopes
            .lock_recover()
            .push(serde_json::to_string(&envelope).expect("serialize effect envelope"));
        self.record(&envelope.invocation);
        if matches!(
            envelope.command,
            RuntimeEffectCommand::AssistantResponseHooks { .. }
        ) && self.crash_before_first_response_hooks
            && !self.response_hook_crash_fired.swap(true, Ordering::SeqCst)
        {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalTaskClosed,
                "simulated host crash between the journaled completion and hook post-processing",
            ));
        }
        let outcome = match envelope.command {
            RuntimeEffectCommand::LlmCall { request } => {
                if self.execute_llm_locally {
                    local_executor
                        .execute(RuntimeEffectEnvelope::new(
                            envelope.invocation,
                            RuntimeEffectCommand::LlmCall { request },
                        ))
                        .await
                } else {
                    let mut llm_calls = self.llm_calls.lock_recover();
                    *llm_calls += 1;
                    let first_call = *llm_calls == 1;
                    let prompt = format!("{:?}", request.messages);
                    let parts = if first_call && prompt.contains("use the tool") {
                        vec![
                            LlmOutputPart::ToolCall {
                                call_id: "call-1".to_string(),
                                tool_name: "echo_tool".to_string(),
                                input_json: serde_json::json!({"value": "hi"}).to_string(),
                                replay: None,
                            },
                            LlmOutputPart::ToolCall {
                                call_id: "call-2".to_string(),
                                tool_name: "echo_tool".to_string(),
                                input_json: serde_json::json!({"value": "there"}).to_string(),
                                replay: None,
                            },
                        ]
                    } else if first_call && prompt.contains("use direct tool") {
                        vec![LlmOutputPart::ToolCall {
                            call_id: "direct-call-1".to_string(),
                            tool_name: "direct_tool".to_string(),
                            input_json: serde_json::json!({}).to_string(),
                            replay: None,
                        }]
                    } else if first_call && prompt.contains("use retry tool") {
                        vec![LlmOutputPart::ToolCall {
                            call_id: "retry-call-1".to_string(),
                            tool_name: "retry_once".to_string(),
                            input_json: serde_json::json!({}).to_string(),
                            replay: None,
                        }]
                    } else {
                        vec![LlmOutputPart::Text {
                            text: "finished".to_string(),
                            response_meta: None,
                        }]
                    };
                    Ok(RuntimeEffectOutcome::LlmCall {
                        result: Box::new(Ok(LlmResponse {
                            full_text: if parts
                                .iter()
                                .any(|part| matches!(part, LlmOutputPart::Text { .. }))
                            {
                                "finished".to_string()
                            } else {
                                String::new()
                            },
                            parts,
                            usage: LlmUsage {
                                input_tokens: 1,
                                output_tokens: 1,
                                cache_read_input_tokens: 0,
                                cache_write_input_tokens: 0,
                                reasoning_output_tokens: 0,
                            },
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        })),
                        text_streamed: false,
                        call_record: None,
                    })
                }
            }
            RuntimeEffectCommand::ToolAttempt {
                call,
                execution_grant,
                attempt,
                max_attempts,
            } => {
                local_executor
                    .execute(RuntimeEffectEnvelope::new(
                        envelope.invocation,
                        RuntimeEffectCommand::ToolAttempt {
                            call,
                            execution_grant,
                            attempt,
                            max_attempts,
                        },
                    ))
                    .await
            }
            RuntimeEffectCommand::ToolBatch { batch } => {
                local_executor
                    .execute(RuntimeEffectEnvelope::new(
                        envelope.invocation,
                        RuntimeEffectCommand::ToolBatch { batch },
                    ))
                    .await
            }
            RuntimeEffectCommand::AssistantResponseHooks { response } => {
                local_executor
                    .execute(RuntimeEffectEnvelope::new(
                        envelope.invocation,
                        RuntimeEffectCommand::AssistantResponseHooks { response },
                    ))
                    .await
            }
            RuntimeEffectCommand::Process { command } => {
                let result = local_executor.into_process()?.execute(*command).await?;
                Ok(RuntimeEffectOutcome::Process { result })
            }
            RuntimeEffectCommand::Trigger { command } => {
                local_executor
                    .execute(RuntimeEffectEnvelope::new(
                        envelope.invocation,
                        RuntimeEffectCommand::Trigger { command },
                    ))
                    .await
            }
            RuntimeEffectCommand::Checkpoint { .. } => Ok(RuntimeEffectOutcome::Checkpoint {
                result: Ok(crate::CheckpointDelivery::default()),
                claims: Box::default(),
            }),
            RuntimeEffectCommand::SyncExecutionEnvironment { .. } => {
                Ok(RuntimeEffectOutcome::SyncExecutionEnvironment { result: Ok(None) })
            }
            RuntimeEffectCommand::ExecCode { .. } => Ok(RuntimeEffectOutcome::ExecCode {
                result: Box::new(Ok(crate::ExecResponse {
                    observations: Vec::new(),
                    observation_truncation: Vec::new(),
                    tool_calls: Vec::new(),
                    executed_calls: Vec::new(),
                    images: Vec::new(),
                    printed_images: Vec::new(),
                    error: None,
                    duration_ms: 0,
                    terminal_finish: Some(serde_json::json!("ok")),
                })),
            }),
            RuntimeEffectCommand::Sleep { .. } => Ok(RuntimeEffectOutcome::Sleep),
            RuntimeEffectCommand::AwaitEvent { .. } => Ok(RuntimeEffectOutcome::AwaitEvent {
                resolution: crate::Resolution::Ok(serde_json::json!(null)),
            }),
            RuntimeEffectCommand::PeekAwaitEvent { .. }
                if self.cancel_after_llm && *self.llm_calls.lock_recover() > 0 =>
            {
                Ok(RuntimeEffectOutcome::PeekAwaitEvent {
                    resolution: Some(Resolution::Ok(serde_json::json!({
                        "state": "cancel_requested",
                        "cancellation": {
                            "request_id": "cancel-after-llm",
                            "origin": "effect-controller-test",
                            "reason": "cancel landed during the journaled LLM run"
                        }
                    }))),
                })
            }
            RuntimeEffectCommand::PeekAwaitEvent { .. } => {
                Ok(RuntimeEffectOutcome::PeekAwaitEvent { resolution: None })
            }
            RuntimeEffectCommand::LanguageRuntimeValue { operation } => {
                local_executor
                    .execute(RuntimeEffectEnvelope::new(
                        envelope.invocation,
                        RuntimeEffectCommand::LanguageRuntimeValue { operation },
                    ))
                    .await
            }
            RuntimeEffectCommand::Direct { request, .. } => {
                if let Some(gate) = &self.direct_gate
                    && gate.2.swap(false, Ordering::SeqCst)
                {
                    gate.0.notify_one();
                    gate.1.notified().await;
                }
                let prompt = format!("{:?}", request.messages);
                let is_full = prompt.contains("raw prompt") || !request.attachments.is_empty();
                let (text, usage) = if is_full {
                    (
                        "raw direct answer",
                        LlmUsage {
                            input_tokens: 4,
                            output_tokens: 6,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            reasoning_output_tokens: 1,
                        },
                    )
                } else {
                    (
                        "direct answer",
                        LlmUsage {
                            input_tokens: 7,
                            output_tokens: 5,
                            cache_read_input_tokens: 1,
                            cache_write_input_tokens: 0,
                            reasoning_output_tokens: 2,
                        },
                    )
                };
                Ok(RuntimeEffectOutcome::Direct {
                    result: Box::new(Ok(LlmResponse {
                        full_text: text.to_string(),
                        parts: vec![LlmOutputPart::Text {
                            text: text.to_string(),
                            response_meta: None,
                        }],
                        usage,
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })),
                    call_record: Some(crate::LlmCallRecord {
                        call_id: crate::LlmCallId("direct-effect-test".to_string()),
                        label: None,
                        replay_drops: Vec::new(),
                        attempts: Vec::new(),
                    }),
                })
            }
        };
        if self.replay_by_key
            && let Ok(outcome) = &outcome
        {
            self.replay_outcomes
                .lock_recover()
                .insert(replay_key, outcome.clone());
        }
        outcome
    }
}

pub(super) fn host_with_effect_recorder(
    recorder: RecordingEffectController,
) -> EmbeddedRuntimeHost {
    let mut config = runtime_host_config_with_inline_controller(Arc::new(recorder));
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        mock_provider(Vec::new()).into_handle(),
    ));
    EmbeddedRuntimeHost::new(config)
}

#[tokio::test]
async fn standard_turn_llm_and_checkpoint_effects_cross_controller_once() {
    let recorder = RecordingEffectController::default();
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: "Done".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "Done".to_string(),
                response_meta: None,
            }],
            usage: LlmUsage {
                input_tokens: 3,
                output_tokens: 2,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            },
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
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
            scoped_test_turn(&recorder, "standard-effects"),
        )
        .await
        .expect("turn");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    assert_eq!(recorder.count_kind(RuntimeEffectKind::LlmCall), 1);
    assert_eq!(recorder.count_kind(RuntimeEffectKind::Checkpoint), 1);
    assert_eq!(recorder.count_kind(RuntimeEffectKind::PeekAwaitEvent), 1);
    assert!(recorder.records().iter().all(|record| {
        record.turn_id.is_some()
            && if record.kind == RuntimeEffectKind::PeekAwaitEvent {
                record.replay_key == "turn_cancel.start_gate"
            } else {
                record.replay_key.starts_with("root:")
            }
    }));
}

#[tokio::test]
async fn durable_cancel_landing_during_llm_is_observed_after_the_journaled_run() {
    let recorder = RecordingEffectController::default()
        .with_cancel_after_llm()
        .with_controller_owned_replay();
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        host_with_effect_recorder(recorder.clone()),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput::text("cancel while the model is running"),
            CancellationToken::new(),
            scoped_test_turn(&recorder, "llm-cancel-boundary"),
        )
        .await
        .expect("cancelled turn");

    assert!(matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled)
    ));
    assert_eq!(
        recorder
            .records()
            .into_iter()
            .map(|record| (record.kind, record.replay_key))
            .collect::<Vec<_>>(),
        vec![
            (
                RuntimeEffectKind::PeekAwaitEvent,
                "turn_cancel.start_gate".to_string()
            ),
            (
                RuntimeEffectKind::LlmCall,
                "root:llm-cancel-boundary:1:0:llm_call:1".to_string()
            ),
            (
                RuntimeEffectKind::PeekAwaitEvent,
                "turn_cancel.after_llm.0".to_string()
            ),
        ],
        "the deployed LLM command must stay first within the iteration and the durable cancel observation must follow it"
    );
}

#[tokio::test]
async fn turn_effect_envelope_does_not_carry_checkpoint_payload() {
    let recorder = RecordingEffectController::default();
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: "Done".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "Done".to_string(),
                response_meta: None,
            }],
            usage: LlmUsage {
                input_tokens: 3,
                output_tokens: 2,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            },
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        host_with_effect_recorder(recorder.clone()),
    )
    .await;
    let large_marker = format!("large-turn-marker-{}", "x".repeat(16_384));

    let turn = runtime
        .run_turn_assembled(
            TurnInput::text(large_marker.clone()),
            CancellationToken::new(),
            scoped_test_turn(&recorder, "checkpoint-envelope"),
        )
        .await
        .expect("turn");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    let checkpoint_envelope = recorder
        .envelopes()
        .into_iter()
        .find(|encoded| {
            serde_json::from_str::<RuntimeEffectEnvelope>(encoded)
                .expect("decode envelope")
                .command
                .kind()
                == RuntimeEffectKind::Checkpoint
        })
        .expect("checkpoint envelope");
    assert!(!checkpoint_envelope.contains("\"turn_checkpoint\":"));
    assert!(!checkpoint_envelope.contains(&large_marker));
    assert!(!checkpoint_envelope.contains("\"messages\""));
    assert!(!checkpoint_envelope.contains("\"events\""));
}

#[tokio::test]
async fn controller_rejection_fails_turn_explicitly() {
    let controller = Arc::new(RejectingEffectController::default());
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(runtime_host_config_with_inline_controller(
            controller.clone(),
        )),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput::text("hello"),
            CancellationToken::new(),
            ScopedEffectController::shared(
                controller,
                ExecutionScope::turn("root", "rejecting-controller"),
            )
            .expect("rejecting execution scope"),
        )
        .await
        .expect("turn");

    assert!(matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::RuntimeError)
    ));
    assert!(turn.errors.iter().any(|issue| {
        issue.kind == "runtime_effect_controller"
            && issue.code.as_deref() == Some("test_controller_rejected")
    }));
}

#[tokio::test]
async fn wrong_controller_outcome_fails_turn_explicitly() {
    let controller = Arc::new(WrongOutcomeEffectController::default());
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(runtime_host_config_with_inline_controller(
            controller.clone(),
        )),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput::text("hello"),
            CancellationToken::new(),
            ScopedEffectController::shared(
                controller,
                ExecutionScope::turn("root", "wrong-outcome-controller"),
            )
            .expect("wrong outcome execution scope"),
        )
        .await
        .expect("turn");

    assert!(matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::RuntimeError)
    ));
    assert!(turn.errors.iter().any(|issue| {
        issue.kind == "runtime_effect_controller"
            && issue.code.as_deref() == Some("runtime_effect_wrong_outcome")
    }));
}

#[tokio::test]
async fn scoped_borrowed_effect_controller_uses_required_stable_turn_id() {
    let recorder = RecordingEffectController::default();
    assert!(
        ScopedEffectController::borrowed(
            &recorder,
            ExecutionScope::turn("effect-test-session", "")
        )
        .is_err()
    );
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: "Done".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "Done".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        EmbeddedRuntimeHost::new(test_runtime_host_config()),
    )
    .await;

    let scoped_effect_controller = scoped_test_turn(&recorder, "stable-scoped-turn");
    let turn = runtime
        .stream_turn(
            TurnInput::text("hello"),
            TurnOptions::new(CancellationToken::new(), scoped_effect_controller)
                .with_events(&NoopEventSink),
        )
        .await
        .expect("turn");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    assert!(recorder.records().iter().all(|record| {
        record.kind == RuntimeEffectKind::PeekAwaitEvent
            || record.replay_key.contains("stable-scoped-turn")
    }));
}

#[tokio::test]
async fn tool_direct_completion_is_opaque_inside_scoped_attempt() {
    struct DirectTool;

    fn direct_tool_definition() -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            "tool:direct_tool",
            "direct_tool",
            "Issue a direct completion from inside a tool",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object", "additionalProperties": true }),
        )
    }

    #[async_trait::async_trait]
    impl crate::ToolProvider for DirectTool {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            vec![direct_tool_definition().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            (name == "direct_tool").then(|| Arc::new(direct_tool_definition().contract()))
        }

        async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolOutcome {
            let completion = call
                .context
                .direct_completions()
                .complete(
                    crate::DirectRequest::text("mock-model", "nested"),
                    "tool-direct",
                )
                .await
                .expect("tool direct completion");
            crate::ToolOutcome::ok(serde_json::json!({ "text": completion.text }))
        }
    }

    let default_recorder = RecordingEffectController::default();
    let scoped_recorder = RecordingEffectController::default();
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: String::new(),
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "direct-call-1".to_string(),
                    tool_name: "direct_tool".to_string(),
                    input_json: serde_json::json!({}).to_string(),
                    replay: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "nested answer".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "nested answer".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "finished".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "finished".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(DirectTool),
        transport,
        host_with_effect_recorder(default_recorder.clone()),
    )
    .await;

    let scoped_effect_controller = scoped_test_turn(&scoped_recorder, "scoped-tool-direct");
    let turn = runtime
        .stream_turn(
            TurnInput::text("use direct tool"),
            TurnOptions::new(CancellationToken::new(), scoped_effect_controller)
                .with_events(&NoopEventSink),
        )
        .await
        .expect("turn");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    assert_eq!(scoped_recorder.count_kind(RuntimeEffectKind::ToolBatch), 1);
    assert_eq!(
        scoped_recorder.count_kind(RuntimeEffectKind::ToolAttempt),
        1
    );
    assert_eq!(scoped_recorder.count_kind(RuntimeEffectKind::Direct), 0);
    assert_eq!(default_recorder.count_kind(RuntimeEffectKind::Direct), 0);
    assert!(
        scoped_recorder
            .envelopes()
            .iter()
            .filter(|envelope| envelope.contains("tool_attempt"))
            .any(|envelope| envelope.contains("direct-call-1"))
    );
}

#[derive(Clone, Default)]
struct CapturingRuntimeReplayController {
    llm_calls: Arc<Mutex<usize>>,
    tool_outcomes: Arc<Mutex<Vec<serde_json::Value>>>,
    process_starts: Arc<std::sync::atomic::AtomicUsize>,
    /// Tool the first mocked assistant turn calls; defaults to `trigger_tool`.
    called_tool: Option<String>,
    inline: InlineRuntimeEffectController,
}

impl CapturingRuntimeReplayController {
    fn calling(tool_name: &str) -> Self {
        Self {
            called_tool: Some(tool_name.to_string()),
            ..Self::default()
        }
    }

    fn tool_outcomes(&self) -> Vec<serde_json::Value> {
        self.tool_outcomes.lock_recover().clone()
    }

    fn process_starts(&self) -> usize {
        self.process_starts.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for CapturingRuntimeReplayController {
    fn replay_ownership(&self) -> crate::EffectReplayOwnership {
        crate::EffectReplayOwnership::Runtime
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.inline.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.inline.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.inline.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.inline.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inline
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inline
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for CapturingRuntimeReplayController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if matches!(&envelope.command, RuntimeEffectCommand::ToolBatch { .. }) {
            let outcome = local_executor.execute(envelope).await?;
            self.tool_outcomes
                .lock_recover()
                .push(serde_json::to_value(&outcome).expect("serialize tool outcome"));
            return Ok(outcome);
        }

        match envelope.command {
            RuntimeEffectCommand::PeekAwaitEvent { .. } => {
                Ok(RuntimeEffectOutcome::PeekAwaitEvent { resolution: None })
            }
            RuntimeEffectCommand::ToolAttempt {
                call,
                execution_grant,
                attempt,
                max_attempts,
            } => {
                local_executor
                    .execute(RuntimeEffectEnvelope::new(
                        envelope.invocation,
                        RuntimeEffectCommand::ToolAttempt {
                            call,
                            execution_grant,
                            attempt,
                            max_attempts,
                        },
                    ))
                    .await
            }
            RuntimeEffectCommand::LlmCall { .. } => {
                let mut llm_calls = self.llm_calls.lock_recover();
                *llm_calls += 1;
                let parts = if *llm_calls == 1 {
                    vec![LlmOutputPart::ToolCall {
                        call_id: "trigger-call".to_string(),
                        tool_name: self
                            .called_tool
                            .clone()
                            .unwrap_or_else(|| "trigger_tool".to_string()),
                        input_json: serde_json::json!({}).to_string(),
                        replay: None,
                    }]
                } else {
                    vec![LlmOutputPart::Text {
                        text: "finished".to_string(),
                        response_meta: None,
                    }]
                };
                Ok(RuntimeEffectOutcome::LlmCall {
                    result: Box::new(Ok(LlmResponse {
                        full_text: if *llm_calls == 1 {
                            String::new()
                        } else {
                            "finished".to_string()
                        },
                        parts,
                        usage: LlmUsage {
                            input_tokens: 1,
                            output_tokens: 1,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            reasoning_output_tokens: 0,
                        },
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })),
                    text_streamed: false,
                    call_record: None,
                })
            }
            RuntimeEffectCommand::Checkpoint { .. } => Ok(RuntimeEffectOutcome::Checkpoint {
                result: Ok(crate::CheckpointDelivery::default()),
                claims: Box::default(),
            }),
            RuntimeEffectCommand::Process { command } => {
                self.process_starts.fetch_add(1, Ordering::SeqCst);
                local_executor
                    .execute(RuntimeEffectEnvelope::new(
                        envelope.invocation,
                        RuntimeEffectCommand::Process { command },
                    ))
                    .await
            }
            other => Err(RuntimeEffectControllerError::foreign(
                "unexpected_effect",
                format!("unexpected effect {}", other.kind().as_str()),
            )),
        }
    }
}

struct TriggerEventTool;

fn trigger_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:trigger_tool",
        "trigger_tool",
        "Emit a test trigger occurrence.",
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

/// Emitting a trigger reserves and starts deliveries through the effect
/// controller, which is orchestration rather than leaf work: a recorded
/// attempt receives an `AttemptContext` with no route to it. This law is
/// about the runtime-owned emission itself, so the tool registers in the
/// orchestration lane and emits inline, twice, standing in for the first
/// emission and its redrive.
#[async_trait::async_trait]
impl crate::tool_provider::orchestration::OrchestratingToolImplementation for TriggerEventTool {
    fn manifest(&self) -> crate::ToolManifest {
        trigger_tool_definition().manifest()
    }

    fn contract(&self) -> Arc<crate::ToolContract> {
        Arc::new(trigger_tool_definition().contract())
    }

    async fn execute(
        &self,
        _args: &serde_json::Value,
        context: &crate::tool_provider::orchestration::OrchestrationContext<'_>,
    ) -> crate::ToolOutcome {
        let source_type = crate::triggers::trigger_event_type("ui.button", "pressed");
        let source_key =
            crate::empty_trigger_source_key(&source_type).expect("empty trigger source key");
        let idempotency_key = "test-trigger:button-pressed".to_string();
        let request = || {
            crate::TriggerOccurrenceRequest::new(
                source_type.clone(),
                source_key.clone(),
                serde_json::json!({ "pressed": true }),
                idempotency_key.clone(),
            )
            .with_source(serde_json::json!({}))
        };
        context
            .triggers()
            .emit(request())
            .await
            .expect("emit tool trigger occurrence");
        context
            .triggers()
            .emit(request())
            .await
            .expect("redrive tool trigger occurrence");
        crate::ToolOutcome::ok(serde_json::json!({ "emitted": true }))
    }
}

fn trigger_orchestrating_tool() -> crate::tool_provider::orchestration::OrchestratingToolDef {
    let implementation: Arc<
        dyn crate::tool_provider::orchestration::OrchestratingToolImplementation,
    > = Arc::new(TriggerEventTool);
    // SAFETY: lash-core owns this test-only trigger contract and its body.
    unsafe {
        crate::tool_provider::orchestration::OrchestratingToolDef::from_first_party(implementation)
    }
}

/// Calls the trigger-emitting orchestrating tool through `call_tool_batch`, so
/// the occurrence has to survive the inner batch effect boundary before the
/// outer one records it.
struct NestedTriggerBatchTool;

fn nested_trigger_batch_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:trigger_batch_tool",
        "trigger_batch_tool",
        "Emit a test trigger occurrence through a nested tool batch.",
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

#[async_trait::async_trait]
impl crate::tool_provider::orchestration::OrchestratingToolImplementation
    for NestedTriggerBatchTool
{
    fn manifest(&self) -> crate::ToolManifest {
        nested_trigger_batch_tool_definition().manifest()
    }

    fn contract(&self) -> Arc<crate::ToolContract> {
        Arc::new(nested_trigger_batch_tool_definition().contract())
    }

    async fn execute(
        &self,
        _args: &serde_json::Value,
        context: &crate::tool_provider::orchestration::OrchestrationContext<'_>,
    ) -> crate::ToolOutcome {
        let replies = context
            .call_tool_batch(vec![crate::ToolInvocation::new(
                "trigger_tool",
                crate::ToolId::from("tool:trigger_tool"),
                serde_json::json!({}),
            )])
            .await;
        assert_eq!(replies.len(), 1);
        crate::ToolOutcome::ok(serde_json::json!({ "nested": replies[0].output.clone() }))
    }
}

fn nested_trigger_batch_orchestrating_tool()
-> crate::tool_provider::orchestration::OrchestratingToolDef {
    let implementation: Arc<
        dyn crate::tool_provider::orchestration::OrchestratingToolImplementation,
    > = Arc::new(NestedTriggerBatchTool);
    // SAFETY: lash-core owns this test-only trigger contract and its body.
    unsafe {
        crate::tool_provider::orchestration::OrchestratingToolDef::from_first_party(implementation)
    }
}

/// A trigger emitted inside a nested tool batch must reach the outer recorded
/// batch outcome: the inner batch drains its own trigger buffer into its
/// outcome, and the consumer restores it into the enclosing buffer. Without the
/// restore the occurrence is dropped before the outer boundary sees it, and the
/// turn's recorded effects lose an emission that really happened.
#[tokio::test]
async fn tool_batch_child_trigger_reaches_the_enclosing_recorded_batch_outcome() {
    let controller = CapturingRuntimeReplayController::calling("trigger_batch_tool");
    let mut config = runtime_host_config_with_inline_controller(Arc::new(controller.clone()));
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        mock_provider(Vec::new()).into_handle(),
    ));
    let trigger_store = Arc::new(crate::InMemoryTriggerStore::default());
    let source_key =
        crate::empty_trigger_source_key("ui.button.pressed").expect("empty trigger source key");
    crate::TriggerStore::execute_command(
        trigger_store.as_ref(),
        "fig1487-nested-batch-register",
        crate::TriggerCommand::Register {
            owner_scope: crate::TriggerOwnerScope::session("root"),
            actor: crate::ProcessOriginator::session(crate::SessionScope::new("root")),
            draft: crate::TriggerSubscriptionDraft::for_process(
                "fig1487/nested-batch",
                crate::ProcessExecutionEnvRef::new("process-env:fig1487-nested-batch"),
                "ui.button.pressed",
                source_key,
                crate::ProcessInput::Engine {
                    kind: "fig1487-nested-batch-engine".to_string(),
                    payload: serde_json::json!({}),
                },
                crate::ProcessIdentity::new("fig1487-nested-batch-engine"),
            )
            .with_payload_schema(crate::LashSchema::any()),
        },
    )
    .await
    .expect("register tool trigger")
    .expect("tool trigger mutation");
    let trigger =
        crate::TriggerEvent::new("Button", "ui.button", "pressed", crate::LashSchema::any());
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        vec![Arc::new(StaticPluginFactory::new(
            "button-triggers",
            crate::PluginSpec::new()
                .with_trigger_event(trigger)
                .with_orchestrating_tool(trigger_orchestrating_tool())
                .with_orchestrating_tool(nested_trigger_batch_orchestrating_tool()),
        ))],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(config)
            .with_trigger_store(Arc::clone(&trigger_store) as Arc<dyn crate::TriggerStore>),
    )
    .await;

    let turn = runtime
        .stream_turn(
            TurnInput::text("emit trigger through a nested batch"),
            TurnOptions::new(
                CancellationToken::new(),
                ScopedEffectController::shared(
                    Arc::new(controller.clone()),
                    ExecutionScope::turn("root", "trigger-batch-tool"),
                )
                .expect("capturing execution scope"),
            )
            .with_events(&NoopEventSink),
        )
        .await
        .expect("turn");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    let tool_outcomes = controller.tool_outcomes();
    assert_eq!(
        tool_outcomes.len(),
        2,
        "the nested batch and the turn's batch are both recorded"
    );
    let outer = tool_outcomes.last().expect("outer batch outcome");
    assert_eq!(outer["type"], "tool_batch");
    let outer_triggers = outer["triggers"]
        .as_array()
        .expect("outer batch trigger outcomes");
    assert_eq!(
        outer_triggers.len(),
        2,
        "the nested emissions must reach the enclosing recorded batch outcome"
    );
    assert_eq!(
        outer_triggers[0]["source_type"],
        serde_json::json!("ui.button.pressed")
    );
    assert_eq!(
        controller.process_starts(),
        2,
        "restoring the drained outcomes must not re-emit the occurrence"
    );
    assert_eq!(
        crate::TriggerStore::list_deliveries(trigger_store.as_ref())
            .await
            .expect("list tool trigger deliveries")
            .len(),
        1,
        "the repeated occurrence still owns one deterministic delivery"
    );
}

#[tokio::test]
async fn runtime_owned_tool_trigger_redrive_reemits_reserved_start_without_appending_session_node()
{
    let controller = CapturingRuntimeReplayController::default();
    let mut config = runtime_host_config_with_inline_controller(Arc::new(controller.clone()));
    config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
        mock_provider(Vec::new()).into_handle(),
    ));
    let trigger_store = Arc::new(crate::InMemoryTriggerStore::default());
    let source_key =
        crate::empty_trigger_source_key("ui.button.pressed").expect("empty trigger source key");
    let registration = crate::TriggerStore::execute_command(
        trigger_store.as_ref(),
        "fig806-tool-register",
        crate::TriggerCommand::Register {
            owner_scope: crate::TriggerOwnerScope::session("root"),
            actor: crate::ProcessOriginator::session(crate::SessionScope::new("root")),
            draft: crate::TriggerSubscriptionDraft::for_process(
                "fig806/tool",
                crate::ProcessExecutionEnvRef::new("process-env:fig806-tool"),
                "ui.button.pressed",
                source_key,
                crate::ProcessInput::Engine {
                    kind: "fig806-tool-engine".to_string(),
                    payload: serde_json::json!({}),
                },
                crate::ProcessIdentity::new("fig806-tool-engine"),
            )
            .with_payload_schema(crate::LashSchema::any()),
        },
    )
    .await
    .expect("register tool trigger")
    .expect("tool trigger mutation");
    assert!(matches!(
        registration,
        crate::TriggerCommandOutcome::Mutation { .. }
    ));
    let trigger =
        crate::TriggerEvent::new("Button", "ui.button", "pressed", crate::LashSchema::any());
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        vec![Arc::new(StaticPluginFactory::new(
            "button-triggers",
            crate::PluginSpec::new()
                .with_trigger_event(trigger)
                .with_orchestrating_tool(trigger_orchestrating_tool()),
        ))],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(config)
            .with_trigger_store(Arc::clone(&trigger_store) as Arc<dyn crate::TriggerStore>),
    )
    .await;

    let turn = runtime
        .stream_turn(
            TurnInput::text("emit trigger from tool"),
            TurnOptions::new(
                CancellationToken::new(),
                ScopedEffectController::shared(
                    Arc::new(controller.clone()),
                    ExecutionScope::turn("root", "trigger-tool"),
                )
                .expect("capturing execution scope"),
            )
            .with_events(&NoopEventSink),
        )
        .await
        .expect("turn");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    let tool_outcomes = controller.tool_outcomes();
    assert_eq!(tool_outcomes.len(), 1);
    assert_eq!(tool_outcomes[0]["type"], "tool_batch");
    assert_eq!(
        tool_outcomes[0]["triggers"]
            .as_array()
            .expect("tool trigger outcomes")
            .len(),
        2,
        "the tool attempt must retain both the first emission and its redrive"
    );
    assert_eq!(
        tool_outcomes[0]["triggers"][0]["source_type"],
        serde_json::json!("ui.button.pressed")
    );
    assert_eq!(
        tool_outcomes[0]["triggers"][0]["payload"],
        serde_json::json!({ "pressed": true })
    );
    assert!(
        tool_outcomes[0]["triggers"][0]["occurrence_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert_eq!(
        controller.process_starts(),
        2,
        "the already-reserved tool redrive must emit the same process start"
    );
    assert_eq!(
        crate::TriggerStore::list_deliveries(trigger_store.as_ref())
            .await
            .expect("list tool trigger deliveries")
            .len(),
        1,
        "the repeated tool occurrence still owns one deterministic delivery"
    );

    let trigger_nodes = turn
        .state
        .session_graph
        .active_path_nodes()
        .into_iter()
        .filter_map(|node| match &node.payload {
            crate::SessionNodePayload::Plugin { plugin_type, body }
                if plugin_type == "lash.trigger" =>
            {
                Some(body.as_ref().clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(trigger_nodes.is_empty());
}

#[tokio::test]
async fn scoped_retry_sleep_records_turn_and_parent_tool_identity() {
    struct RetryOnceTool {
        attempts: Arc<std::sync::atomic::AtomicUsize>,
    }

    fn retry_once_tool_definition() -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            "tool:retry_once",
            "retry_once",
            "Fails once with a safe retry.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object", "additionalProperties": true }),
        )
        .with_retry_policy(crate::ToolRetryPolicy::safe(2, 1, 1))
    }

    #[async_trait::async_trait]
    impl crate::ToolProvider for RetryOnceTool {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            vec![retry_once_tool_definition().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            (name == "retry_once").then(|| Arc::new(retry_once_tool_definition().contract()))
        }

        async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolOutcome {
            let attempt = self
                .attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if attempt == 0 {
                return crate::ToolOutcome::retryable_failure(
                    crate::ToolFailureClass::External,
                    "transient",
                    "transient failure",
                    Some(1),
                );
            }
            crate::ToolOutcome::ok(serde_json::json!({ "ok": true }))
        }
    }

    let recorder = RecordingEffectController::default();
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: String::new(),
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "retry-call-1".to_string(),
                    tool_name: "retry_once".to_string(),
                    input_json: serde_json::json!({}).to_string(),
                    replay: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "finished".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "finished".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(RetryOnceTool {
            attempts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }),
        transport,
        EmbeddedRuntimeHost::new(test_runtime_host_config()),
    )
    .await;

    let scoped_effect_controller = scoped_test_turn(&recorder, "scoped-retry-sleep");
    let turn = runtime
        .stream_turn(
            TurnInput::text("use retry tool"),
            TurnOptions::new(CancellationToken::new(), scoped_effect_controller)
                .with_events(&NoopEventSink),
        )
        .await
        .expect("turn");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    let attempt_records = recorder
        .records()
        .into_iter()
        .filter(|record| record.kind == RuntimeEffectKind::ToolAttempt)
        .collect::<Vec<_>>();
    assert_eq!(attempt_records.len(), 2);
    let tool = &attempt_records[0];
    assert_eq!(tool.turn_id.as_deref(), Some("scoped-retry-sleep"));
    assert!(tool.replay_key.contains("scoped-retry-sleep"));
    assert!(tool.replay_key.contains("child:0:retry-call-1:attempt:1"));
    assert_eq!(recorder.count_kind(RuntimeEffectKind::Sleep), 1);
    assert!(
        recorder
            .envelopes()
            .iter()
            .any(|envelope| envelope.contains("retry-call-1"))
    );
}

#[tokio::test]
async fn tool_attempt_effect_crosses_controller_per_child_attempt_and_runs_local_tools() {
    let recorder = RecordingEffectController::default();
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: String::new(),
                parts: vec![
                    LlmOutputPart::ToolCall {
                        call_id: "call-1".to_string(),
                        tool_name: "echo_tool".to_string(),
                        input_json: serde_json::json!({"value": "hi"}).to_string(),
                        replay: None,
                    },
                    LlmOutputPart::ToolCall {
                        call_id: "call-2".to_string(),
                        tool_name: "echo_tool".to_string(),
                        input_json: serde_json::json!({"value": "there"}).to_string(),
                        replay: None,
                    },
                ],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "finished".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "finished".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EchoTool),
        transport,
        host_with_effect_recorder(recorder.clone()),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "use the tool".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            scoped_test_turn(&recorder, "tool-replay-effects"),
        )
        .await
        .expect("turn");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    assert_eq!(recorder.count_kind(RuntimeEffectKind::ToolBatch), 1);
    assert_eq!(recorder.count_kind(RuntimeEffectKind::ToolAttempt), 2);
    let tool_keys = recorder
        .records()
        .into_iter()
        .filter(|record| record.kind == RuntimeEffectKind::ToolAttempt)
        .map(|record| record.replay_key)
        .collect::<Vec<_>>();
    assert_eq!(tool_keys.len(), 2);
    assert!(
        tool_keys
            .iter()
            .any(|key| key.contains("child:0:call-1:attempt:1"))
    );
    assert!(
        tool_keys
            .iter()
            .any(|key| key.contains("child:1:call-2:attempt:1"))
    );
    assert!(
        recorder
            .envelopes()
            .iter()
            .any(|envelope| envelope.contains("call-1") && envelope.contains("call-2"))
    );
    assert!(
        turn.tool_calls
            .iter()
            .any(|record| record.tool == "echo_tool" && record.output.is_success())
    );
}

#[tokio::test]
async fn tool_batch_serializes_child_attempts_when_controller_disallows_concurrency() {
    let controller = SerialOnlyEffectController::default();
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: String::new(),
                parts: vec![
                    LlmOutputPart::ToolCall {
                        call_id: "call-1".to_string(),
                        tool_name: "echo_tool".to_string(),
                        input_json: serde_json::json!({"value": "hi"}).to_string(),
                        replay: None,
                    },
                    LlmOutputPart::ToolCall {
                        call_id: "call-2".to_string(),
                        tool_name: "echo_tool".to_string(),
                        input_json: serde_json::json!({"value": "there"}).to_string(),
                        replay: None,
                    },
                ],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                full_text: "finished".to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: "finished".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EchoTool),
        transport,
        EmbeddedRuntimeHost::new(test_runtime_host_config()),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "use the tool".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            scoped_test_turn(&controller, "serial-tool-batch-effects"),
        )
        .await
        .expect("turn");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    assert_eq!(controller.count_kind(RuntimeEffectKind::ToolBatch), 1);
    assert_eq!(controller.count_kind(RuntimeEffectKind::ToolAttempt), 2);
    assert_eq!(
        controller.max_in_flight_tool_attempts(),
        1,
        "controllers that cannot accept concurrent effects must not receive overlapping child attempts"
    );
}

#[tokio::test]
async fn exec_and_execution_environment_effects_cross_controller_once() {
    let recorder = RecordingEffectController::default();
    let policy = SessionPolicy {
        provider_id: "mock".to_string(),
        model: crate::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("valid model spec"),
        ..SessionPolicy::new(crate::TurnBudget::Unbounded)
    };
    let plugin_session =
        crate::PluginHost::new(vec![Arc::new(EffectControllerTestProtocolFactory {
            install_code_executor: true,
        })])
        .build_session("root", None)
        .expect("plugins");
    let mut runtime = LashRuntime::from_embedded_state(
        policy,
        host_with_effect_recorder(recorder.clone()),
        RuntimeServices::new(plugin_session),
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded)),
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "run code".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            scoped_test_turn(&recorder, "exec-surface-effects"),
        )
        .await
        .expect("turn");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    assert_eq!(
        recorder.count_kind(RuntimeEffectKind::SyncExecutionEnvironment),
        1
    );
    assert_eq!(recorder.count_kind(RuntimeEffectKind::ExecCode), 1);
}

#[tokio::test]
async fn start_exec_without_code_executor_stops_as_runtime_error() {
    let policy = SessionPolicy {
        provider_id: "mock".to_string(),
        model: crate::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("valid model spec"),
        ..SessionPolicy::new(crate::TurnBudget::Unbounded)
    };
    let plugin_session =
        crate::PluginHost::new(vec![Arc::new(EffectControllerTestProtocolFactory {
            install_code_executor: false,
        })])
        .build_session("root", None)
        .expect("plugins");
    let mut runtime = LashRuntime::from_embedded_state(
        policy,
        EmbeddedRuntimeHost::new(test_runtime_host_config_with_provider(
            mock_provider(Vec::new()).into_handle(),
        )),
        RuntimeServices::new(plugin_session),
        RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded)),
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");

    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "run code".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: crate::TurnContext::default(),
            },
            CancellationToken::new(),
            named_turn_scope("root", "exec-without-executor"),
        )
        .await
        .expect("turn");

    assert!(matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::RuntimeError)
    ));
    assert!(turn.errors.iter().any(|issue| {
        issue
            .message
            .contains("code execution is not available in this session")
    }));
}

#[tokio::test]
async fn direct_completion_crosses_controller_and_records_usage_and_trace() {
    let recorder = RecordingEffectController::default();
    let trace_path = unique_trace_path("direct-completion");
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: "direct answer".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "direct answer".to_string(),
                response_meta: None,
            }],
            usage: LlmUsage {
                input_tokens: 7,
                output_tokens: 5,
                cache_read_input_tokens: 1,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 2,
            },
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let host = EmbeddedRuntimeHost::new({
        let mut config = runtime_host_config_with_inline_controller(Arc::new(recorder.clone()));
        config.tracing.trace_sink = Some(Arc::new(lash_trace::JsonlTraceSink::new(
            trace_path.clone(),
        )));
        config
    });
    let runtime =
        runtime_with_plugins_and_tools_and_host(Vec::new(), Arc::new(EmptyTools), transport, host)
            .await;

    let manager = runtime.runtime_session_services().expect("session manager");
    let direct = manager.direct_completion_client(
        RuntimeEffectControllerHandle::shared(Arc::new(recorder.clone())),
        None,
    );
    let mut request = crate::DirectRequest::text("mock-model", "summarize");
    let caused_by = CausalRef::ToolCall {
        session_id: "root".to_string(),
        call_id: "originating-tool-call".to_string(),
    };
    request.caused_by = Some(caused_by.clone());
    let completion = direct
        .direct_completion(request, "direct-test")
        .await
        .expect("direct completion");

    assert_eq!(completion.text, "direct answer");
    assert_eq!(completion.usage.input_tokens, 7);
    assert_eq!(completion.llm_call.call_id.0, "direct-effect-test");
    assert_eq!(recorder.count_kind(RuntimeEffectKind::Direct), 1);
    let discriminator =
        crate::runtime::causal::direct_request_discriminator(None, Some(&caused_by), 1);
    let expected_replay_key = crate::runtime::causal::direct_effect_invocation(
        "root",
        "direct-test",
        discriminator,
        None,
        Some(caused_by),
    )
    .replay_key()
    .expect("derived direct-effect replay key")
    .to_string();
    assert!(recorder.records().iter().any(|record| {
        record.kind == RuntimeEffectKind::Direct && record.replay_key == expected_replay_key
    }));
    let ledger = runtime.shared_token_ledger.lock_recover();
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].source, "direct-test");
    assert_eq!(ledger[0].model, "mock-model");
    assert_eq!(ledger[0].usage.input_tokens, 7);
}

#[tokio::test]
async fn in_turn_direct_completion_uses_effect_controller_without_out_of_band_commit() {
    let recorder = RecordingEffectController::default();
    let store = Arc::new(RecordingStore::default());
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: "direct answer".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "direct answer".to_string(),
                response_meta: None,
            }],
            usage: LlmUsage {
                input_tokens: 7,
                output_tokens: 5,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            },
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let host = EmbeddedRuntimeHost::new(runtime_host_config_with_inline_controller(Arc::new(
        recorder.clone(),
    )));
    let runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        host,
        store.clone(),
    )
    .await;
    let manager = runtime.runtime_session_services().expect("session manager");
    let direct = manager.direct_completion_client(
        RuntimeEffectControllerHandle::shared(Arc::new(recorder.clone())),
        Some("turn-direct".to_string()),
    );
    let completion = direct
        .direct_completion(
            crate::DirectRequest::text("mock-model", "summarize"),
            "direct-test",
        )
        .await
        .expect("direct completion");

    assert_eq!(completion.text, "direct answer");
    assert!(recorder.records().iter().any(|record| {
        record.kind == RuntimeEffectKind::Direct && record.turn_id.as_deref() == Some("turn-direct")
    }));

    // A direct effect must record usage into the shared in-memory ledger only;
    // that ledger is drained and persisted exactly once by the owning turn's
    // final commit. The direct path must NOT issue its own out-of-band
    // `commit_runtime_state` mid-turn: doing so races the owning turn's
    // head-revision CAS.
    assert_eq!(
        *store.runtime_commit_count.lock_recover(),
        0,
        "in-turn direct completion must not commit runtime state out-of-band"
    );
    let ledger = runtime.shared_token_ledger.lock_recover();
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].usage.input_tokens, 7);
}

#[tokio::test]
async fn direct_clients_from_one_turn_share_sequential_replay_ordinals() {
    let recorder = RecordingEffectController::default();
    let response = || MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            full_text: "direct answer".to_string(),
            ..LlmResponse::default()
        }),
    };
    let host = EmbeddedRuntimeHost::new(runtime_host_config_with_inline_controller(Arc::new(
        recorder.clone(),
    )));
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(vec![response(), response()]),
        host,
    )
    .await;
    let manager = runtime.runtime_session_services().expect("session manager");
    let first = manager.direct_completion_client(
        RuntimeEffectControllerHandle::shared(Arc::new(recorder.clone())),
        Some("turn-direct".to_string()),
    );
    let second = manager.direct_completion_client(
        RuntimeEffectControllerHandle::shared(Arc::new(recorder.clone())),
        Some("turn-direct".to_string()),
    );

    first
        .direct_completion(
            crate::DirectRequest::text("mock-model", "first"),
            "direct-test",
        )
        .await
        .expect("first direct completion");
    second
        .direct_completion(
            crate::DirectRequest::text("mock-model", "second"),
            "direct-test",
        )
        .await
        .expect("second direct completion");

    let replay_keys = recorder
        .records()
        .into_iter()
        .filter(|record| record.kind == RuntimeEffectKind::Direct)
        .map(|record| record.replay_key)
        .collect::<Vec<_>>();
    assert_eq!(replay_keys.len(), 2);
    assert!(replay_keys[0].starts_with("direct:v2:sha256:"));
    assert!(replay_keys[1].starts_with("direct:v2:sha256:"));
    assert_ne!(replay_keys[0], replay_keys[1]);
}

#[tokio::test]
async fn direct_concurrency_requires_keys_and_releases_unkeyed_guard() {
    let gate = Arc::new((
        tokio::sync::Notify::new(),
        tokio::sync::Notify::new(),
        std::sync::atomic::AtomicBool::new(true),
    ));
    let recorder = RecordingEffectController::default().with_direct_gate(Arc::clone(&gate));
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(runtime_host_config_with_inline_controller(Arc::new(
            recorder.clone(),
        ))),
    )
    .await;
    let manager = runtime.runtime_session_services().expect("session manager");
    let client = manager.direct_completion_client(
        RuntimeEffectControllerHandle::shared(Arc::new(recorder)),
        Some("turn-direct".to_string()),
    );

    let first_client = client.clone();
    let mut first = crate::task::spawn(async move {
        first_client
            .direct_completion(
                crate::DirectRequest::text("mock-model", "first"),
                "direct-test",
            )
            .await
    });
    gate.0.notified().await;
    client
        .direct_completion(
            crate::DirectRequest::text("mock-model", "other hook"),
            "other-plugin-hook",
        )
        .await
        .expect("a distinct usage source owns an independent ordinal lane");
    let overlap = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.direct_completion(
            crate::DirectRequest::text("mock-model", "overlap"),
            "direct-test",
        ),
    )
    .await
    .expect("overlap rejected promptly")
    .expect_err("overlapping unkeyed call must fail");
    assert!(overlap.to_string().contains("explicit replay keys"));
    gate.1.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), &mut first)
        .await
        .expect("first completion returned")
        .expect("first task")
        .expect("first completion");

    let keyed_a = client.direct_completion(
        crate::DirectRequest::text("mock-model", "a").with_replay_key("a"),
        "direct-test",
    );
    let keyed_b = client.direct_completion(
        crate::DirectRequest::text("mock-model", "b").with_replay_key("b"),
        "direct-test",
    );
    let (a, b) = tokio::join!(keyed_a, keyed_b);
    a.expect("first keyed call");
    b.expect("second keyed call");
    client
        .direct_completion(
            crate::DirectRequest::text("mock-model", "after"),
            "direct-test",
        )
        .await
        .expect("guard released after completion");
}

#[tokio::test]
async fn direct_effect_restores_required_streaming_for_provider_execution() {
    let saw_stream_events = Arc::new(AtomicBool::new(false));
    let captured = Arc::clone(&saw_stream_events);
    let transport = TestProvider::builder()
        .kind("stream-required")
        .requires_streaming(true)
        .complete(move |request| {
            let captured = Arc::clone(&captured);
            async move {
                captured.store(request.stream_events.is_some(), Ordering::SeqCst);
                Ok(LlmResponse {
                    full_text: "direct answer".to_string(),
                    parts: vec![LlmOutputPart::Text {
                        text: "direct answer".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        EmbeddedRuntimeHost::new(test_runtime_host_config()),
    )
    .await;

    let manager = runtime.runtime_session_services().expect("session manager");
    let direct = manager.direct_completion_client(
        RuntimeEffectControllerHandle::shared(Arc::new(InlineRuntimeEffectController::default())),
        None,
    );
    let completion = direct
        .direct_completion(
            crate::DirectRequest::text("mock-model", "summarize"),
            "direct-test",
        )
        .await
        .expect("direct completion");

    assert_eq!(completion.text, "direct answer");
    assert!(saw_stream_events.load(Ordering::SeqCst));
}

#[path = "effect_direct_llm.rs"]
mod direct_llm;

#[tokio::test]
async fn direct_llm_completion_envelope_stores_attachment_refs_not_bytes() {
    let recorder = RecordingEffectController::default();
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(test_runtime_host_config()),
    )
    .await;

    let image_bytes = vec![137, 80, 78, 71];
    let expected_attachment_id = crate::attachments::content_id(&image_bytes).to_string();
    let request = LlmRequest {
        model: "mock-model".to_string(),
        messages: vec![LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::Attachment { attachment_idx: 0 }],
        )],
        attachments: vec![AttachmentSource::inline(
            crate::MediaType::parse("image/png").unwrap(),
            image_bytes,
        )],
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::None,
        model_variant: Default::default(),
        model_capability: crate::ModelCapability::default(),
        scope: crate::LlmRequestScope::new(
            "direct-attachment-test",
            "direct-attachment-test:frame",
            "direct-attachment-test:request",
        ),
        output_spec: None,
        stream_events: None,
        generation: crate::GenerationOptions::default(),
        provider_trace: None,
    };

    let manager = runtime.runtime_session_services().expect("session manager");
    let direct = manager.direct_completion_client(
        RuntimeEffectControllerHandle::shared(Arc::new(recorder.clone())),
        None,
    );
    let completion = direct
        .direct_llm_completion(request, "direct-image-test")
        .await
        .expect("direct llm completion");

    assert_eq!(completion.response.full_text, "raw direct answer");
    let envelope = recorder
        .envelopes()
        .into_iter()
        .find(|envelope| envelope.contains("\"type\":\"direct\""))
        .expect("direct llm envelope");
    assert!(!envelope.contains("\"data\""));
    assert!(envelope.contains(&expected_attachment_id));
}

fn effect_module_sources(manifest_dir: &std::path::Path) -> Vec<PathBuf> {
    let dir = manifest_dir.join("src/runtime/effect");
    let mut paths = std::fs::read_dir(&dir)
        .expect("read effect module directory")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("rs"))
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

#[test]
fn lint_runtime_effect_executor_has_no_legacy_future_api() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source_files = effect_module_sources(&manifest_dir)
        .into_iter()
        .chain([
            manifest_dir.join("src/runtime/turn_driver.rs"),
            manifest_dir.join("src/runtime/session_manager/direct.rs"),
        ])
        .collect::<Vec<_>>();
    let legacy_future_type = ["Effect", "Future"].concat();
    let legacy_constructor = ["Runtime", "Effect", "Executor", "::new"].concat();
    for path in source_files {
        let source = std::fs::read_to_string(&path).expect("read runtime effect source");
        assert!(
            !source.contains(&legacy_future_type),
            "{} still mentions {legacy_future_type}",
            path.display()
        );
        assert!(
            !source.contains(&legacy_constructor),
            "{} still mentions {legacy_constructor}",
            path.display()
        );
    }
}

#[test]
fn lint_runtime_effect_controller_cutover_has_no_legacy_host_request_or_fallback_symbols() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source_files = effect_module_sources(&manifest_dir)
        .into_iter()
        .chain([
            manifest_dir.join("src/runtime/turn_driver.rs"),
            manifest_dir.join("src/runtime/session_manager/direct.rs"),
            manifest_dir.join("src/tool_dispatch.rs"),
            manifest_dir.join("src/runtime/assembly.rs"),
            manifest_dir.join("src/runtime/mod.rs"),
            manifest_dir.join("src/runtime/turn_loop.rs"),
            manifest_dir.join("src/runtime/process/model.rs"),
            manifest_dir.join("src/runtime/session_manager/process_runners/control.rs"),
        ])
        .collect::<Vec<_>>();
    let forbidden = [
        ["Runtime", "Effect", "Host"].concat(),
        ["Local", "Runtime", "Effect", "Host"].concat(),
        ["Runtime", "Effect", "Request"].concat(),
        ["Background", "Task", "Start", "Request"].concat(),
        ["missing", "_tool", "_result", "_completed", "_call"].concat(),
        ["fallback", "_assistant", "_output", "_from", "_state"].concat(),
        ["fallback", "_controller"].concat(),
        ["resolve", "_durable", "_turn", "_scope"].concat(),
        ["Process", "Op", "Scope", "::", "new"].concat(),
        ["b", "\"", "un", "serializable", "\""].concat(),
    ];
    for path in source_files {
        let source = std::fs::read_to_string(&path).expect("read runtime effect source");
        for symbol in &forbidden {
            assert!(
                !source.contains(symbol.as_str()),
                "{} still mentions {symbol}",
                path.display()
            );
        }
    }
}

fn unique_trace_path(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "lash-{prefix}-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}

struct EffectControllerTestProtocolFactory {
    install_code_executor: bool,
}

impl crate::PluginFactory for EffectControllerTestProtocolFactory {
    fn id(&self) -> &'static str {
        "test_protocol"
    }

    fn build(
        &self,
        _ctx: &crate::PluginSessionContext,
    ) -> Result<Arc<dyn crate::SessionPlugin>, crate::PluginError> {
        Ok(Arc::new(EffectControllerTestProtocolPlugin {
            install_code_executor: self.install_code_executor,
        }))
    }
}

struct EffectControllerTestProtocolPlugin {
    install_code_executor: bool,
}

impl crate::SessionPlugin for EffectControllerTestProtocolPlugin {
    fn id(&self) -> &'static str {
        "effect_controller_test_protocol"
    }

    fn register(&self, registrar: &mut crate::PluginRegistrar) -> Result<(), crate::PluginError> {
        registrar
            .protocol()
            .session(Arc::new(EffectControllerTestProtocolSession))?;
        if self.install_code_executor {
            registrar
                .execution()
                .code_executor(Arc::new(EffectControllerTestCodeExecutor))?;
        }
        registrar
            .protocol()
            .protocol_driver(Arc::new(EffectControllerTestProtocolDriver))?;
        Ok(())
    }
}

struct EffectControllerTestProtocolSession;

#[async_trait::async_trait]
impl ProtocolSessionPlugin for EffectControllerTestProtocolSession {}

struct EffectControllerTestCodeExecutor;

#[async_trait::async_trait]
impl crate::plugin::CodeExecutorPlugin for EffectControllerTestCodeExecutor {
    async fn execute_code(
        &self,
        _ctx: crate::RuntimeExecutionContext<'_>,
        _request: crate::ExecRequest,
    ) -> Result<crate::ExecResponse, crate::SessionError> {
        Ok(crate::ExecResponse {
            observations: vec!["exec output".to_string()],
            observation_truncation: Vec::new(),
            tool_calls: Vec::new(),
            executed_calls: Vec::new(),
            images: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            duration_ms: 1,
            terminal_finish: None,
        })
    }
}

struct EffectControllerTestProtocolDriver;

impl ProtocolDriverPlugin for EffectControllerTestProtocolDriver {
    fn build_preamble(&self, input: crate::ProtocolBuildInput) -> crate::TurnDriverPreamble {
        crate::TurnDriverPreamble {
            config: crate::TurnDriverConfig::chat(
                Arc::new(EffectControllerTestDriver),
                true,
                Arc::new(effect_controller_turn_limit_final_message),
            ),
            tool_specs: input.tool_catalog.model_tool_specs(),
            tool_names: input.tool_catalog.tool_names(),
            tool_names_fingerprint: input.tool_catalog.tool_names_fingerprint(),
            execution_prompt: Arc::from(""),
            prompt_contributions: input.extra_prompt_contributions,
        }
    }
}

fn effect_controller_turn_limit_final_message(
    message_id: String,
    max_turns: usize,
) -> crate::Message {
    crate::Message {
        id: message_id.clone(),
        role: crate::MessageRole::System,
        parts: crate::shared_parts(vec![crate::Part::error(
            format!("{message_id}.p0"),
            format!("Turn limit reached ({max_turns}) before a final test response."),
        )]),
        origin: None,
    }
}

struct EffectControllerTestDriver;

impl lash_sansio::ProtocolDriverHandle<crate::HostTurnProtocol> for EffectControllerTestDriver {
    fn prepare_protocol_iteration(
        &self,
        _ctx: crate::DriverContextView<'_>,
    ) -> Vec<crate::DriverAction> {
        vec![crate::DriverAction::StartExec {
            language: "code".to_string(),
            code: "print('effect controller')".to_string(),
            driver_state: crate::ProtocolDriverState::new(
                "effect_controller_test_protocol",
                serde_json::Value::Null,
            ),
        }]
    }

    fn handle_llm_success(
        &self,
        _ctx: crate::DriverContextView<'_>,
        _waiting: lash_sansio::WaitingLlmState<crate::HostTurnProtocol>,
        _llm_response: LlmResponse,
        _text_streamed: bool,
    ) -> Vec<crate::DriverAction> {
        Vec::new()
    }

    fn handle_tool_results(
        &self,
        _ctx: crate::DriverContextView<'_>,
        _completed: Vec<crate::sansio::CompletedToolCall>,
    ) -> Vec<crate::DriverAction> {
        Vec::new()
    }

    fn handle_exec_result(
        &self,
        _ctx: crate::DriverContextView<'_>,
        _waiting: lash_sansio::WaitingExecState<crate::HostTurnProtocol>,
        result: Result<crate::ExecResponse, String>,
    ) -> Vec<crate::DriverAction> {
        match result {
            Ok(response) => vec![crate::DriverAction::Finish(TurnOutcome::Finished(
                TurnFinish::FinalValue {
                    value: serde_json::json!(response.observations.join("\n")),
                },
            ))],
            Err(error) => vec![
                crate::DriverAction::Emit(crate::SessionStreamEvent::Error {
                    message: error,
                    envelope: None,
                }),
                crate::DriverAction::Finish(TurnOutcome::Stopped(TurnStop::RuntimeError)),
            ],
        }
    }
}
