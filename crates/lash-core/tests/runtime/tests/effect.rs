// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use crate::runtime_support::effect_controller_doubles as controller_doubles;
pub(crate) use crate::runtime_support::effect_controller_doubles::*;
pub(crate) use crate::runtime_support::effect_recording_authority::*;
pub(in crate::runtime::tests) use controller_doubles::RejectingEffectController;
use controller_doubles::{StrictReplayJournal, WrongOutcomeEffectController};
use lash_core::facade_support::SessionGraphFacadeOps;
use lash_core::llm::types::{
    AttachmentSource, LlmContentBlock, LlmMessage, LlmRole, LlmToolChoice,
};
use lash_core::plugin::{ProtocolDriverPlugin, ProtocolSessionPlugin};
use lash_sansio::sync::MutexExt;
mod commit_pins;
mod fig1127;
mod fig1416;

mod fig2471;
use crate::runtime_support::effect_recording_authority as recording_authority;
mod response_settlement;
mod source_lint_support;
mod turn_cancel_modes;
pub(super) use recording_authority::{
    host_with_effect_recorder, layered_effect_host, runtime_host_config_with_effect_layer,
};
use source_lint_support::{effect_module_sources, turn_loop_module_sources, unique_trace_path};

#[tokio::test]
async fn standard_turn_llm_and_checkpoint_effects_cross_controller_once() {
    let backend = memory_backend().await;
    let recorder = RecordingEffectController::default();
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
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
        host_with_effect_recorder(&backend, recorder.clone()),
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
                turn_context: lash_core::TurnContext::default(),
            },
            CancellationToken::new(),
            scoped_test_turn(&backend, &recorder, &TurnId::from("standard-effects")),
        )
        .await
        .expect("turn");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    assert_eq!(recorder.count_kind(RuntimeEffectKind::LlmCall), 1);
    assert_eq!(recorder.count_kind(RuntimeEffectKind::Checkpoint), 1);
    // A durable backend binds turn cancellation to its journal, so the gate is
    // peeked at the start and again once the model has answered.
    let peeks = recorder
        .records()
        .into_iter()
        .filter(|record| record.kind == RuntimeEffectKind::PeekAwaitEvent)
        .map(|record| record.replay_key)
        .collect::<Vec<_>>();
    assert_eq!(peeks, ["turn_cancel.start_gate", "turn_cancel.after_llm.0"]);
    assert!(recorder.records().iter().all(|record| {
        record.turn_id.is_some()
            && (record.kind == RuntimeEffectKind::PeekAwaitEvent
                || record.replay_key.starts_with("root:"))
    }));
}

#[tokio::test]
async fn turn_effect_envelope_does_not_carry_checkpoint_payload() {
    let backend = memory_backend().await;
    let recorder = RecordingEffectController::default();
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
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
        host_with_effect_recorder(&backend, recorder.clone()),
    )
    .await;
    let large_marker = format!("large-turn-marker-{}", "x".repeat(16_384));

    let turn = runtime
        .run_turn_assembled(
            TurnInput::text(large_marker.clone()),
            CancellationToken::new(),
            scoped_test_turn(&backend, &recorder, &TurnId::from("checkpoint-envelope")),
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
    let backend = memory_backend().await;
    let controller = Arc::new(RejectingEffectController::default());
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(runtime_host_config_with_effect_layer(
            &backend,
            controller.clone(),
        )),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput::text("hello"),
            CancellationToken::new(),
            layered_scope(
                &backend,
                controller,
                AdmittedScope::turn("root", "rejecting-controller"),
            ),
        )
        .await
        .expect("turn");

    assert!(matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::RuntimeError)
    ));
    assert!(turn.errors.iter().any(|issue| {
        issue.kind == lash_core::TurnFailureKind::RuntimeEffectController
            && issue
                .code
                .as_ref()
                .is_some_and(|code| code.namespaced() == "foreign:test_controller_rejected")
    }));
}

#[tokio::test]
async fn wrong_controller_outcome_fails_turn_explicitly() {
    let backend = memory_backend().await;
    let controller = Arc::new(WrongOutcomeEffectController);
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(runtime_host_config_with_effect_layer(
            &backend,
            controller.clone(),
        )),
    )
    .await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput::text("hello"),
            CancellationToken::new(),
            layered_scope(
                &backend,
                controller,
                AdmittedScope::turn("root", "wrong-outcome-controller"),
            ),
        )
        .await
        .expect("turn");

    assert!(matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::RuntimeError)
    ));
    assert!(turn.errors.iter().any(|issue| {
        issue.kind == lash_core::TurnFailureKind::RuntimeEffectController
            && issue.code.as_ref().map(|code| code.namespaced())
                == Some("lash:runtime_effect_wrong_outcome".to_string())
    }));
}

#[tokio::test]
async fn scoped_borrowed_effect_controller_uses_required_stable_turn_id() {
    let backend = memory_backend().await;
    let recorder = RecordingEffectController::default();
    assert!(
        layered_effect_host(&backend, Arc::new(recorder.clone()))
            .scoped(AdmittedScope::turn("effect-test-session", ""))
            .is_err()
    );
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
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
        EmbeddedRuntimeHost::new(test_runtime_host_config(&backend)),
    )
    .await;

    let scoped_effect_controller =
        scoped_test_turn(&backend, &recorder, &TurnId::from("stable-scoped-turn"));
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
    let backend = memory_backend().await;
    struct DirectTool;

    fn direct_tool_definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
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
    impl lash_core::ToolProvider for DirectTool {
        fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
            vec![direct_tool_definition().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
            (name == "direct_tool").then(|| Arc::new(direct_tool_definition().contract()))
        }

        async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
            let completion = call
                .context
                .direct_completions()
                .complete(
                    lash_core::facade_support::DirectRequest::text("mock-model", "nested"),
                    "tool-direct",
                )
                .await
                .expect("tool direct completion");
            lash_core::ToolOutcome::ok(serde_json::json!({ "text": completion.text })).into()
        }
    }

    let default_recorder = RecordingEffectController::default();
    // The scoped double shares the host-side recorder's substrate: group opens
    // land there, where the host's tool-child resolver was registered.
    let scoped_recorder = RecordingEffectController::default();
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
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
        host_with_effect_recorder(&backend, default_recorder.clone()),
    )
    .await;

    let scoped_effect_controller = scoped_test_turn(
        &backend,
        &scoped_recorder,
        &TurnId::from("scoped-tool-direct"),
    );
    let turn = runtime
        .stream_turn(
            TurnInput::text("use direct tool"),
            TurnOptions::new(CancellationToken::new(), scoped_effect_controller)
                .with_events(&NoopEventSink),
        )
        .await
        .expect("turn");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    // A batch is a durable group (FIG-3397): its children execute as
    // `ToolInvocation` leaves on the native group substrate, never through the
    // turn-scoped double; a leaf's attempt crosses the host's controller.
    assert_eq!(
        scoped_recorder.count_kind(RuntimeEffectKind::ToolInvocation),
        0
    );
    assert_eq!(
        default_recorder.count_kind(RuntimeEffectKind::ToolAttempt),
        1
    );
    assert_eq!(scoped_recorder.count_kind(RuntimeEffectKind::Direct), 0);
    assert_eq!(default_recorder.count_kind(RuntimeEffectKind::Direct), 0);
    assert!(
        default_recorder
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
impl lash_core::testing::EffectLayer for CapturingRuntimeReplayController {
    async fn execute_effect(
        &self,
        _inner: &dyn RuntimeEffectController,
        envelope: RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
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
                    stream: Box::default(),
                })
            }
            RuntimeEffectCommand::Checkpoint { .. } => Ok(RuntimeEffectOutcome::Checkpoint {
                result: Ok(lash_core::CheckpointDelivery::default()),
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
            // The consumer journals its incorporated prefix and each child its
            // recorded presentation; this double runs both locally, as the
            // shared recording double does.
            command @ (RuntimeEffectCommand::IncorporateGroupSettlements { .. }
            | RuntimeEffectCommand::PresentToolResult { .. }) => {
                local_executor
                    .execute(RuntimeEffectEnvelope::new(envelope.invocation, command))
                    .await
            }
            other => Err(RuntimeEffectControllerError::foreign(
                "unexpected_effect",
                lash_core::TurnFailureCause::Outcome,
                format!("unexpected effect {}", other.kind().as_str()),
            )),
        }
    }

    async fn await_next_settlement(
        &self,
        inner: &dyn RuntimeEffectController,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        // A group child is a `ToolInvocation` the backend substrate runs
        // itself, so its recorded outcome is captured where the consumer
        // reads it: the settlement's `triggers` is where a child's drained
        // emissions are journaled (ADR 0099 §6).
        let settlement = inner.await_next_settlement(handle, cancel).await?;
        if let Ok(outcome @ RuntimeEffectOutcome::ToolInvocation { .. }) = &settlement.outcome {
            self.tool_outcomes
                .lock_recover()
                .push(serde_json::to_value(outcome).expect("serialize tool outcome"));
        }
        Ok(settlement)
    }
}

struct TriggerEventTool;

fn trigger_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
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
/// orchestration lane and emits synchronously, twice, standing in for the first
/// emission and its redrive.
#[async_trait::async_trait]
impl lash_core::facade_support::OrchestratingToolImplementation for TriggerEventTool {
    fn manifest(&self) -> lash_core::ToolManifest {
        trigger_tool_definition().manifest()
    }

    fn contract(&self) -> Arc<lash_core::ToolContract> {
        Arc::new(trigger_tool_definition().contract())
    }

    async fn execute(
        &self,
        _args: &serde_json::Value,
        context: &lash_core::facade_support::OrchestrationContext<'_>,
    ) -> lash_core::ToolOutcome {
        let source_type = lash_core::triggers::trigger_event_type("ui.button", "pressed");
        let source_key = lash_core::facade_support::empty_trigger_source_key(&source_type)
            .expect("empty trigger source key");
        let idempotency_key = "test-trigger:button-pressed".to_string();
        let request = || {
            lash_core::TriggerOccurrenceRequest::new(
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
        lash_core::ToolOutcome::ok(serde_json::json!({ "emitted": true }))
    }
}

#[expect(
    unsafe_code,
    reason = "OrchestratingToolDef::from_first_party is lash-core's unsafe capability boundary, and this crate owns the tool contract it registers"
)]
fn trigger_orchestrating_tool() -> lash_core::facade_support::OrchestratingToolDef {
    let implementation: Arc<dyn lash_core::facade_support::OrchestratingToolImplementation> =
        Arc::new(TriggerEventTool);
    // SAFETY: lash-core owns this test-only trigger contract and its body.
    unsafe { lash_core::facade_support::OrchestratingToolDef::from_first_party(implementation) }
}

/// Calls the trigger-emitting orchestrating tool through `call_tool_batch`, so
/// the occurrence has to survive the inner batch effect boundary before the
/// outer one records it.
struct NestedTriggerBatchTool;

fn nested_trigger_batch_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
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
impl lash_core::facade_support::OrchestratingToolImplementation for NestedTriggerBatchTool {
    fn manifest(&self) -> lash_core::ToolManifest {
        nested_trigger_batch_tool_definition().manifest()
    }

    fn contract(&self) -> Arc<lash_core::ToolContract> {
        Arc::new(nested_trigger_batch_tool_definition().contract())
    }

    async fn execute(
        &self,
        _args: &serde_json::Value,
        context: &lash_core::facade_support::OrchestrationContext<'_>,
    ) -> lash_core::ToolOutcome {
        let replies = context
            .call_tool_batch(vec![lash_core::facade_support::ToolInvocation::new(
                "trigger_tool",
                lash_core::ToolId::from("tool:trigger_tool"),
                serde_json::json!({}),
            )])
            .await;
        assert_eq!(replies.len(), 1);
        lash_core::ToolOutcome::ok(serde_json::json!({ "nested": replies[0].output.clone() }))
    }
}

#[expect(
    unsafe_code,
    reason = "OrchestratingToolDef::from_first_party is lash-core's unsafe capability boundary, and this crate owns the tool contract it registers"
)]
fn nested_trigger_batch_orchestrating_tool() -> lash_core::facade_support::OrchestratingToolDef {
    let implementation: Arc<dyn lash_core::facade_support::OrchestratingToolImplementation> =
        Arc::new(NestedTriggerBatchTool);
    // SAFETY: lash-core owns this test-only trigger contract and its body.
    unsafe { lash_core::facade_support::OrchestratingToolDef::from_first_party(implementation) }
}

/// A trigger emitted inside a nested tool batch must reach the enclosing
/// effect's recorded outcome: the inner group's consumer restores each child's
/// journaled trigger receipts into the enclosing buffer (ADR 0099 §6), and the
/// enclosing leaf's settlement carries them. Without the restore the
/// occurrence is dropped before the outer boundary sees it, and the turn's
/// recorded effects lose an emission that really happened.
#[tokio::test]
async fn tool_batch_child_trigger_reaches_the_enclosing_group_settlement() {
    let backend = memory_backend().await;
    let controller = CapturingRuntimeReplayController::calling("trigger_batch_tool");
    let mut config = runtime_host_config_with_effect_layer(&backend, Arc::new(controller.clone()));
    config.providers.provider_resolver =
        Arc::new(lash_core::facade_support::SingleProviderResolver::new(
            mock_provider(Vec::new()).into_handle(),
        ));
    let trigger_store = backend.trigger_store();
    let source_key = lash_core::facade_support::empty_trigger_source_key("ui.button.pressed")
        .expect("empty trigger source key");
    lash_core::TriggerStore::execute_command(
        trigger_store.as_ref(),
        "fig1487-nested-batch-register",
        lash_core::TriggerCommand::Register {
            owner_scope: lash_core::TriggerOwnerScope::session("root"),
            actor: lash_core::ProcessOriginator::session(lash_core::SessionScope::new("root")),
            draft: lash_core::TriggerSubscriptionDraft::for_process(
                "fig1487/nested-batch",
                lash_core::ProcessExecutionEnvRef::new("process-env:fig1487-nested-batch"),
                "ui.button.pressed",
                source_key,
                lash_core::ProcessInput::Engine {
                    kind: "fig1487-nested-batch-engine".to_string(),
                    payload: serde_json::json!({}),
                },
                lash_core::ProcessIdentity::new("fig1487-nested-batch-engine"),
            )
            .with_payload_schema(lash_core::LashSchema::any()),
        },
    )
    .await
    .expect("register tool trigger")
    .expect("tool trigger mutation");
    let trigger = lash_core::facade_support::TriggerEvent::new(
        "Button",
        "ui.button",
        "pressed",
        lash_core::LashSchema::any(),
    );
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        vec![Arc::new(StaticPluginFactory::new(
            "button-triggers",
            lash_core::facade_support::PluginSpec::new()
                .with_trigger_event(trigger)
                .with_orchestrating_tool(trigger_orchestrating_tool())
                .with_orchestrating_tool(nested_trigger_batch_orchestrating_tool()),
        ))],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(config),
    )
    .await;

    let turn = runtime
        .stream_turn(
            TurnInput::text("emit trigger through a nested batch"),
            TurnOptions::new(
                CancellationToken::new(),
                layered_scope(
                    &backend,
                    Arc::new(controller.clone()),
                    AdmittedScope::turn("root", "trigger-batch-tool"),
                ),
            )
            .with_events(&NoopEventSink),
        )
        .await
        .expect("turn");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    let tool_outcomes = controller.tool_outcomes();
    let enclosing = tool_outcomes
        .iter()
        .find(|outcome| {
            outcome["type"] == "tool_invocation"
                && outcome["outcome"]["record"]["tool"] == "trigger_batch_tool"
        })
        .expect("the enclosing orchestrating leaf's recorded outcome");
    let enclosing_triggers = enclosing["settlement"]["triggers"]
        .as_array()
        .expect("enclosing settlement trigger outcomes");
    assert_eq!(
        enclosing_triggers.len(),
        2,
        "the nested emissions must reach the enclosing effect's recorded settlement"
    );
    assert_eq!(
        enclosing_triggers[0]["source_type"],
        serde_json::json!("ui.button.pressed")
    );
    assert_eq!(
        controller.process_starts(),
        2,
        "restoring the drained outcomes must not re-emit the occurrence"
    );
    assert_eq!(
        lash_core::TriggerStore::list_deliveries(trigger_store.as_ref())
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
    let backend = memory_backend().await;
    let controller = CapturingRuntimeReplayController::default();
    let mut config = runtime_host_config_with_effect_layer(&backend, Arc::new(controller.clone()));
    config.providers.provider_resolver =
        Arc::new(lash_core::facade_support::SingleProviderResolver::new(
            mock_provider(Vec::new()).into_handle(),
        ));
    let trigger_store = backend.trigger_store();
    let source_key = lash_core::facade_support::empty_trigger_source_key("ui.button.pressed")
        .expect("empty trigger source key");
    let registration = lash_core::TriggerStore::execute_command(
        trigger_store.as_ref(),
        "fig806-tool-register",
        lash_core::TriggerCommand::Register {
            owner_scope: lash_core::TriggerOwnerScope::session("root"),
            actor: lash_core::ProcessOriginator::session(lash_core::SessionScope::new("root")),
            draft: lash_core::TriggerSubscriptionDraft::for_process(
                "fig806/tool",
                lash_core::ProcessExecutionEnvRef::new("process-env:fig806-tool"),
                "ui.button.pressed",
                source_key,
                lash_core::ProcessInput::Engine {
                    kind: "fig806-tool-engine".to_string(),
                    payload: serde_json::json!({}),
                },
                lash_core::ProcessIdentity::new("fig806-tool-engine"),
            )
            .with_payload_schema(lash_core::LashSchema::any()),
        },
    )
    .await
    .expect("register tool trigger")
    .expect("tool trigger mutation");
    assert!(matches!(
        registration,
        lash_core::TriggerCommandOutcome::Mutation { .. }
    ));
    let trigger = lash_core::facade_support::TriggerEvent::new(
        "Button",
        "ui.button",
        "pressed",
        lash_core::LashSchema::any(),
    );
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        vec![Arc::new(StaticPluginFactory::new(
            "button-triggers",
            lash_core::facade_support::PluginSpec::new()
                .with_trigger_event(trigger)
                .with_orchestrating_tool(trigger_orchestrating_tool()),
        ))],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(config),
    )
    .await;

    let turn = runtime
        .stream_turn(
            TurnInput::text("emit trigger from tool"),
            TurnOptions::new(
                CancellationToken::new(),
                layered_scope(
                    &backend,
                    Arc::new(controller.clone()),
                    AdmittedScope::turn("root", "trigger-tool"),
                ),
            )
            .with_events(&NoopEventSink),
        )
        .await
        .expect("turn");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    let tool_outcomes = controller.tool_outcomes();
    assert_eq!(tool_outcomes.len(), 1);
    // The leaf is a `ToolInvocation` group child now (FIG-3397): its journaled
    // trigger receipts ride in the recorded settlement (ADR 0099 §6).
    assert_eq!(tool_outcomes[0]["type"], "tool_invocation");
    let triggers = tool_outcomes[0]["settlement"]["triggers"]
        .as_array()
        .expect("tool trigger outcomes in the settlement record");
    assert_eq!(
        triggers.len(),
        2,
        "the tool attempt must retain both the first emission and its redrive"
    );
    assert_eq!(
        triggers[0]["source_type"],
        serde_json::json!("ui.button.pressed")
    );
    assert_eq!(
        triggers[0]["payload"],
        serde_json::json!({ "pressed": true })
    );
    assert!(
        triggers[0]["occurrence_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert_eq!(
        controller.process_starts(),
        2,
        "the already-reserved tool redrive must emit the same process start"
    );
    assert_eq!(
        lash_core::TriggerStore::list_deliveries(trigger_store.as_ref())
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
            lash_core::SessionNodePayload::Plugin { plugin_type, body }
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
    let backend = memory_backend().await;
    struct RetryOnceTool {
        attempts: Arc<std::sync::atomic::AtomicUsize>,
    }

    fn retry_once_tool_definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
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
        .with_retry_policy(lash_core::ToolRetryPolicy::safe(2, 1, 1))
    }

    #[async_trait::async_trait]
    impl lash_core::ToolProvider for RetryOnceTool {
        fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
            vec![retry_once_tool_definition().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
            (name == "retry_once").then(|| Arc::new(retry_once_tool_definition().contract()))
        }

        async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
            let attempt = self
                .attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if attempt == 0 {
                return lash_core::ToolOutcome::retryable_failure(
                    lash_core::ToolFailureClass::External,
                    "transient",
                    "transient failure",
                    Some(1),
                )
                .into();
            }
            lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })).into()
        }
    }

    let recorder = RecordingEffectController::default();
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
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
        // The host shares the scoped recorder's group substrate: the turn's
        // tool group opens there, where the host's tool-child resolver is
        // registered (FIG-3397).
        host_with_effect_recorder(&backend, recorder.clone()),
    )
    .await;

    let scoped_effect_controller =
        scoped_test_turn(&backend, &recorder, &TurnId::from("scoped-retry-sleep"));
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
    let backend = memory_backend().await;
    let recorder = RecordingEffectController::default();
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
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
        host_with_effect_recorder(&backend, recorder.clone()),
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
                turn_context: lash_core::TurnContext::default(),
            },
            CancellationToken::new(),
            scoped_test_turn(&backend, &recorder, &TurnId::from("tool-replay-effects")),
        )
        .await
        .expect("turn");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    // The batch is a durable group of `ToolInvocation` children now
    // (FIG-3397). The children run on the native group substrate, not through
    // this wrapping double; each leaf's attempt crosses it as before.
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
    // No single envelope names both calls now: each leaf is its own
    // `ToolInvocation` group child (FIG-3397).
    assert!(
        recorder
            .envelopes()
            .iter()
            .any(|envelope| envelope.contains("call-1"))
    );
    assert!(
        recorder
            .envelopes()
            .iter()
            .any(|envelope| envelope.contains("call-2"))
    );
    assert!(
        turn.tool_calls
            .iter()
            .any(|record| record.tool == "echo_tool" && record.output.is_success())
    );
}

#[tokio::test]
async fn exec_and_execution_environment_effects_cross_controller_once() {
    let backend = memory_backend().await;
    let recorder = RecordingEffectController::default();
    let policy = SessionPolicy {
        provider_id: "mock".to_string(),
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("valid model spec"),
        ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let plugin_session =
        lash_core::testing::test_plugin_host(vec![Arc::new(EffectControllerTestProtocolFactory {
            code_executor: Some(Arc::new(EffectControllerTestCodeExecutor)),
        })])
        .build_session("root")
        .expect("plugins");
    let runtime_host = host_with_effect_recorder(&backend, recorder.clone());
    let runtime_services = RuntimeServices::new(
        plugin_session,
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let mut runtime = LashRuntime::from_embedded_state(
        policy,
        runtime_host,
        runtime_services,
        RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        )),
        lash_core::testing::runtime_lease_owner(),
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
                turn_context: lash_core::TurnContext::default(),
            },
            CancellationToken::new(),
            scoped_test_turn(&backend, &recorder, &TurnId::from("exec-surface-effects")),
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
    let backend = memory_backend().await;
    let policy = SessionPolicy {
        provider_id: "mock".to_string(),
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("valid model spec"),
        ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let plugin_session =
        lash_core::testing::test_plugin_host(vec![Arc::new(EffectControllerTestProtocolFactory {
            code_executor: None,
        })])
        .build_session("root")
        .expect("plugins");
    let runtime_host = EmbeddedRuntimeHost::new(test_runtime_host_config_with_provider(
        &backend,
        mock_provider(Vec::new()).into_handle(),
    ));
    let runtime_services = RuntimeServices::new(
        plugin_session,
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let mut runtime = LashRuntime::from_embedded_state(
        policy,
        runtime_host,
        runtime_services,
        RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        )),
        lash_core::testing::runtime_lease_owner(),
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
                turn_context: lash_core::TurnContext::default(),
            },
            CancellationToken::new(),
            backend_turn_scope(
                &backend,
                &SessionId::from("root"),
                &TurnId::from("exec-without-executor"),
            ),
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
    let backend = memory_backend().await;
    let recorder = RecordingEffectController::default();
    let trace_path = unique_trace_path("direct-completion");
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
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
        let mut config =
            runtime_host_config_with_effect_layer(&backend, Arc::new(recorder.clone()));
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
        RuntimeEffectControllerHandle::shared(layered_operation_controller(
            &backend,
            Arc::new(recorder.clone()),
        )),
        None,
    );
    let mut request = lash_core::facade_support::DirectRequest::text("mock-model", "summarize");
    let caused_by = CausalRef::ToolCall {
        session_id: SessionId::from("root"),
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
    let discriminator = lash_core::testing::runtime_internals::causal::direct_request_discriminator(
        None,
        Some(&caused_by),
        1,
    );
    let expected_replay_key =
        lash_core::testing::runtime_internals::causal::direct_effect_invocation(
            &ExecutionScope::turn("root", "turn-1"),
            &SessionId::from("root"),
            "direct-test",
            discriminator,
            None,
            Some(caused_by),
        )
        .replay_key()
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
    let backend = memory_backend().await;
    let recorder = RecordingEffectController::default();
    let store = unbound_recording_store(&backend).await;
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
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
    let host = EmbeddedRuntimeHost::new(runtime_host_config_with_effect_layer(
        &backend,
        Arc::new(recorder.clone()),
    ));
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
        RuntimeEffectControllerHandle::shared(layered_operation_controller(
            &backend,
            Arc::new(recorder.clone()),
        )),
        Some(TurnId::from("turn-direct".to_string())),
    );
    let completion = direct
        .direct_completion(
            lash_core::facade_support::DirectRequest::text("mock-model", "summarize"),
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
    let backend = memory_backend().await;
    let recorder = RecordingEffectController::default();
    let response = || MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "direct answer".to_string(),
                response_meta: None,
            }],
            ..LlmResponse::default()
        }),
    };
    let host = EmbeddedRuntimeHost::new(runtime_host_config_with_effect_layer(
        &backend,
        Arc::new(recorder.clone()),
    ));
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(vec![response(), response()]),
        host,
    )
    .await;
    let manager = runtime.runtime_session_services().expect("session manager");
    let first = manager.direct_completion_client(
        RuntimeEffectControllerHandle::shared(layered_operation_controller(
            &backend,
            Arc::new(recorder.clone()),
        )),
        Some(TurnId::from("turn-direct".to_string())),
    );
    let second = manager.direct_completion_client(
        RuntimeEffectControllerHandle::shared(layered_operation_controller(
            &backend,
            Arc::new(recorder.clone()),
        )),
        Some(TurnId::from("turn-direct".to_string())),
    );

    first
        .direct_completion(
            lash_core::facade_support::DirectRequest::text("mock-model", "first"),
            "direct-test",
        )
        .await
        .expect("first direct completion");
    second
        .direct_completion(
            lash_core::facade_support::DirectRequest::text("mock-model", "second"),
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
    assert!(replay_keys[0].starts_with("direct:v3:blake3:"));
    assert!(replay_keys[1].starts_with("direct:v3:blake3:"));
    assert_ne!(replay_keys[0], replay_keys[1]);
}

#[tokio::test]
async fn direct_concurrency_requires_keys_and_releases_unkeyed_guard() {
    let backend = memory_backend().await;
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
        EmbeddedRuntimeHost::new(runtime_host_config_with_effect_layer(
            &backend,
            Arc::new(recorder.clone()),
        )),
    )
    .await;
    let manager = runtime.runtime_session_services().expect("session manager");
    let client = manager.direct_completion_client(
        RuntimeEffectControllerHandle::shared(layered_operation_controller(
            &backend,
            Arc::new(recorder),
        )),
        Some(TurnId::from("turn-direct".to_string())),
    );

    let first_client = client.clone();
    let mut first = lash_core::task::spawn(async move {
        first_client
            .direct_completion(
                lash_core::facade_support::DirectRequest::text("mock-model", "first"),
                "direct-test",
            )
            .await
    });
    gate.0.notified().await;
    client
        .direct_completion(
            lash_core::facade_support::DirectRequest::text("mock-model", "other hook"),
            "other-plugin-hook",
        )
        .await
        .expect("a distinct usage source owns an independent ordinal lane");
    let overlap = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.direct_completion(
            lash_core::facade_support::DirectRequest::text("mock-model", "overlap"),
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
        lash_core::facade_support::DirectRequest::text("mock-model", "a").with_replay_key("a"),
        "direct-test",
    );
    let keyed_b = client.direct_completion(
        lash_core::facade_support::DirectRequest::text("mock-model", "b").with_replay_key("b"),
        "direct-test",
    );
    let (a, b) = tokio::join!(keyed_a, keyed_b);
    a.expect("first keyed call");
    b.expect("second keyed call");
    client
        .direct_completion(
            lash_core::facade_support::DirectRequest::text("mock-model", "after"),
            "direct-test",
        )
        .await
        .expect("guard released after completion");
}

#[tokio::test]
async fn direct_effect_restores_required_streaming_for_provider_execution() {
    let backend = memory_backend().await;
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
        EmbeddedRuntimeHost::new(test_runtime_host_config(&backend)),
    )
    .await;

    let manager = runtime.runtime_session_services().expect("session manager");
    let direct = manager.direct_completion_client(
        RuntimeEffectControllerHandle::shared(
            backend
                .effect_host()
                .scoped_static(lash_core::AdmittedScope::runtime_operation(
                    "test-runtime-effect-controller",
                ))
                .expect("admit the direct-completion scope")
                .expect("the backend host lends a static controller")
                .owned_controller()
                .expect("a static controller is shared"),
        ),
        None,
    );
    let completion = direct
        .direct_completion(
            lash_core::facade_support::DirectRequest::text("mock-model", "summarize"),
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
    let backend = memory_backend().await;
    let recorder = RecordingEffectController::default();
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        EmbeddedRuntimeHost::new(test_runtime_host_config(&backend)),
    )
    .await;

    let image_bytes = vec![137, 80, 78, 71];
    let expected_attachment_id = lash_core::attachments::content_id(&image_bytes).to_string();
    let request = LlmRequest {
        instructions: None,
        model: "mock-model".to_string(),
        messages: vec![LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::Attachment {
                source: Box::new(AttachmentSource::inline(
                    lash_core::MediaType::parse("image/png").unwrap(),
                    image_bytes,
                )),
            }],
        )],
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::None,
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        scope: lash_core::LlmRequestScope::new(
            "direct-attachment-test",
            "direct-attachment-test:frame",
            "direct-attachment-test:request",
        ),
        output_spec: None,
        stream_events: None,
        generation: lash_core::GenerationOptions::default(),
        provider_trace: None,
    };

    let manager = runtime.runtime_session_services().expect("session manager");
    let direct = manager.direct_completion_client(
        RuntimeEffectControllerHandle::shared(layered_operation_controller(
            &backend,
            Arc::new(recorder.clone()),
        )),
        None,
    );
    let completion = direct
        .direct_llm_completion(request, "direct-image-test")
        .await
        .expect("direct llm completion");

    assert_eq!(completion.response.full_text(), "raw direct answer");
    let envelope = recorder
        .envelopes()
        .into_iter()
        .find(|envelope| envelope.contains("\"type\":\"direct\""))
        .expect("direct llm envelope");
    assert!(!envelope.contains("\"data\""));
    assert!(envelope.contains(&expected_attachment_id));
}

#[test]
fn lint_runtime_effect_executor_has_no_legacy_future_api() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source_files = effect_module_sources(&manifest_dir)
        .into_iter()
        .chain([
            manifest_dir.join("src/runtime/turn_driver.rs"),
            manifest_dir.join("../lash-core-execution/src/direct.rs"),
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
        .chain(turn_loop_module_sources(&manifest_dir))
        .chain([
            manifest_dir.join("src/runtime/turn_driver.rs"),
            manifest_dir.join("../lash-core-execution/src/direct.rs"),
            manifest_dir.join("../lash-core-execution/src/tool_dispatch.rs"),
            manifest_dir.join("src/runtime/assembly.rs"),
            manifest_dir.join("src/runtime/mod.rs"),
            manifest_dir.join("src/runtime/turn_loop.rs"),
            manifest_dir.join("../lash-core-execution/src/runtime/process/model.rs"),
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

#[cfg(test)]
mod effect_driver_support;
use effect_driver_support::{
    EffectControllerTestCodeExecutor, EffectControllerTestProtocolFactory,
};
