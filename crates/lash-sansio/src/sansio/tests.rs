use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::TurnFinish;
use crate::llm::types::{LlmOutputPart, LlmRequest, LlmResponse, LlmTerminalReason};
use crate::session_model::message::PartAttachment;
use crate::session_model::{
    ConversationRecord, Message, MessageRole, MessageSequence, Part, SessionHistoryRecord,
};
use crate::{ModelToolReturnPart, ToolCancellation, ToolFailure, ToolFailureClass};

static NEXT_TEST_MESSAGE_ID: AtomicU64 = AtomicU64::new(1);

fn fresh_message_id() -> String {
    format!(
        "m_test_{}",
        NEXT_TEST_MESSAGE_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn test_config(protocol_driver: Arc<dyn ProtocolDriverHandle>) -> TurnMachineConfig {
    TurnMachineConfig {
        protocol_driver,
        projector: Arc::new(ChatContextProjector),
        sync_execution_environment: false,
        model: "test-model".to_string(),
        max_context_tokens: None,
        turn_budget: crate::TurnBudget::Unbounded,
        no_progress_budget: Default::default(),
        model_variant: crate::ReasoningSelection::ProviderDefault,
        model_capability: crate::llm::capability::ModelCapability::default(),
        generation: crate::llm::types::GenerationOptions::default(),
        autonomous: false,
        tool_specs: Vec::new().into(),
        system_prompt: Arc::from(""),
        session_id: "test".to_string(),
        turn_id: "test-turn".to_string(),
        emit_llm_trace: false,
        termination: (),
        turn_limit_final_message: Arc::new(test_turn_limit_final_message),
    }
}

fn test_turn_limit_final_message(message_id: String, max_turns: usize) -> Message {
    Message {
        id: message_id.clone(),
        role: MessageRole::System,
        parts: crate::shared_parts(vec![Part::error(
            format!("{message_id}.p0"),
            format!("Turn limit reached ({max_turns}) before a final test response."),
        )]),
        origin: None,
    }
}

fn assistant_done(text: impl Into<String>) -> TurnOutcome {
    TurnOutcome::Finished(TurnFinish::AssistantMessage { text: text.into() })
}

fn test_attachment_ref(byte_len: u64) -> crate::AttachmentRef {
    crate::AttachmentRef {
        id: crate::AttachmentId::parse("att-test").expect("valid attachment id"),
        media_type: crate::MediaType::parse("image/png").unwrap(),
        byte_len,
        type_metadata: None,
        label: None,
    }
}

fn user_message(content: &str) -> Message {
    Message {
        id: "m0".to_string(),
        role: MessageRole::User,
        parts: vec![Part::text("m0.p0".to_string(), content.to_string(), None)].into(),
        origin: None,
    }
}

fn conversation_event(message: Message) -> SessionHistoryRecord {
    SessionHistoryRecord::Conversation(ConversationRecord::from_message(message))
}

fn text_message(role: MessageRole, content: impl Into<String>) -> Message {
    let id = fresh_message_id();
    Message {
        id: id.clone(),
        role,
        parts: vec![Part::text(format!("{id}.p0"), content.into(), None)].into(),
        origin: None,
    }
}

fn drain_effects(machine: &mut TurnMachine) -> Vec<Effect> {
    let mut effects = Vec::new();
    while let Some(effect) = machine.poll_effect() {
        effects.push(effect);
    }
    effects
}

fn find_llm_call(effects: &[Effect]) -> Option<(&EffectId, &LlmRequest)> {
    effects.iter().find_map(|effect| match effect {
        Effect::LlmCall { id, request } => Some((id, request.as_ref())),
        _ => None,
    })
}

fn find_exec_call(effects: &[Effect]) -> Option<(&EffectId, &str)> {
    effects.iter().find_map(|effect| match effect {
        Effect::ExecCode { id, code, .. } => Some((id, code.as_str())),
        _ => None,
    })
}

fn find_checkpoint(effects: &[Effect]) -> Option<(EffectId, CheckpointKind)> {
    effects.iter().find_map(|effect| match effect {
        Effect::Checkpoint { id, checkpoint } => Some((*id, *checkpoint)),
        _ => None,
    })
}

fn find_progress(effects: &[Effect]) -> Option<(&MessageSequence, usize)> {
    effects.iter().find_map(|effect| match effect {
        Effect::Progress {
            messages,
            event_delta: _,
            protocol_iteration,
        } => Some((messages, *protocol_iteration)),
        _ => None,
    })
}

fn find_done(effects: &[Effect]) -> Option<(&MessageSequence, usize)> {
    effects.iter().find_map(|effect| match effect {
        Effect::Done {
            messages,
            event_delta: _,
            protocol_iteration,
        } => Some((messages, *protocol_iteration)),
        _ => None,
    })
}

fn progress_event_delta(effects: &[Effect]) -> Option<&[SessionHistoryRecord]> {
    effects.iter().find_map(|effect| match effect {
        Effect::Progress { event_delta, .. } => Some(event_delta.as_slice()),
        _ => None,
    })
}

fn done_event_delta(effects: &[Effect]) -> Option<&[SessionHistoryRecord]> {
    effects.iter().find_map(|effect| match effect {
        Effect::Done { event_delta, .. } => Some(event_delta.as_slice()),
        _ => None,
    })
}

fn find_execution_environment_sync(effects: &[Effect]) -> Option<(EffectId, bool)> {
    effects.iter().find_map(|effect| match effect {
        Effect::SyncExecutionEnvironment {
            id,
            update_machine_config,
        } => Some((*id, *update_machine_config)),
        _ => None,
    })
}

fn roundtrip_checkpoint(checkpoint: TurnCheckpoint) -> TurnCheckpoint {
    let encoded = serde_json::to_string(&checkpoint).expect("serialize checkpoint");
    serde_json::from_str(&encoded).expect("deserialize checkpoint")
}

fn empty_exec_response() -> crate::ExecResponse {
    crate::ExecResponse {
        observations: Vec::new(),
        observation_truncation: Vec::new(),
        tool_calls: Vec::new(),
        executed_calls: Vec::new(),
        printed_images: Vec::new(),
        error: None,
        duration_ms: 0,
        degraded_bindings: Vec::new(),
        terminal_finish: None,
    }
}

fn completed_tool(
    call_id: &str,
    tool_name: &str,
    args: serde_json::Value,
    output: ToolCallOutput,
) -> CompletedToolCall {
    CompletedToolCall {
        call_id: call_id.to_string(),
        tool_name: tool_name.to_string(),
        args,
        output,
        model_return: ModelToolReturn {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            parts: vec![ModelToolReturnPart::text(format!("{tool_name} result"))],
        },
        duration_ms: 1,
        intent_outcomes: Vec::new(),
        replay: None,
    }
}

struct ProseDriver;

impl ProtocolDriverHandle for ProseDriver {
    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        vec![DriverAction::StartLlm {
            request: ctx.project_llm_request(false),
            driver_state: None,
        }]
    }

    fn handle_llm_success(
        &self,
        _ctx: DriverContextView<'_>,
        _waiting: WaitingLlmState,
        _llm_response: LlmResponse,
        _text_streamed: bool,
    ) -> Vec<DriverAction> {
        vec![
            DriverAction::AppendEvents(vec![conversation_event(text_message(
                MessageRole::Assistant,
                "done",
            ))]),
            DriverAction::StartCheckpoint {
                checkpoint: CheckpointKind::BeforeCompletion,
                on_empty: CheckpointResumeAction::Finish(assistant_done("done")),
            },
        ]
    }

    fn handle_tool_results(
        &self,
        _ctx: DriverContextView<'_>,
        _completed: Vec<CompletedToolCall>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }

    fn handle_exec_result(
        &self,
        _ctx: DriverContextView<'_>,
        _waiting: WaitingExecState,
        _result: Result<crate::ExecResponse, String>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }
}

#[test]
fn chat_context_projector_projects_event_context_as_user_messages() {
    fn message_text(message: &crate::llm::types::LlmMessage) -> String {
        message
            .blocks
            .iter()
            .filter_map(|block| match block {
                crate::llm::types::LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    let cause = TurnCause {
        id: "wake:abc".to_string(),
        event_type: "process.wake".to_string(),
        origin: crate::MessageOrigin::Process {
            process_id: "process-1".to_string(),
            event_type: "process.wake".to_string(),
            sequence: 7,
            wake_id: Some("wake:abc".to_string()),
            caused_by: None,
        },
        text: "Background process wake\nProcess: process-1\nEvent: process.wake #7\nWake input:\nblue button pressed".to_string(),
    };
    let messages = MessageSequence::from(vec![cause.to_event_message()]);
    let config = test_config(Arc::new(ProseDriver));

    let active_request = ChatContextProjector.project(ProjectorContext {
        config: &config,
        messages: &messages,
        events: &[],
        turn_causes: std::slice::from_ref(&cause),
        protocol_iteration: 0,
        use_tools: false,
    });
    assert!(active_request.messages.iter().any(|message| {
        message.role == crate::llm::types::LlmRole::User
            && message_text(message).contains("=== TURN EVENTS ===")
            && message_text(message).contains("blue button pressed")
    }));
    assert!(active_request.messages.iter().all(|message| {
        message.role != crate::llm::types::LlmRole::System
            || !message_text(message).contains("blue button pressed")
    }));
    let active_mentions = active_request
        .messages
        .iter()
        .filter(|message| message_text(message).contains("blue button pressed"))
        .count();
    assert_eq!(
        active_mentions, 1,
        "active turn events must not duplicate history"
    );

    let history_request = ChatContextProjector.project(ProjectorContext {
        config: &config,
        messages: &messages,
        events: &[],
        turn_causes: &[],
        protocol_iteration: 1,
        use_tools: false,
    });
    assert!(history_request.messages.iter().any(|message| {
        message.role == crate::llm::types::LlmRole::User
            && message_text(message).contains("Runtime event:")
            && message_text(message).contains("blue button pressed")
    }));
    assert!(history_request.messages.iter().all(|message| {
        message.role != crate::llm::types::LlmRole::System
            || !message_text(message).contains("blue button pressed")
    }));
}

struct ExecDriver;

impl ProtocolDriverHandle for ExecDriver {
    fn prepare_protocol_iteration(&self, _ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        vec![DriverAction::StartExec {
            language: "code".to_string(),
            code: "print 1".to_string(),
            driver_state: serde_json::json!("exec-state"),
        }]
    }

    fn handle_llm_success(
        &self,
        _ctx: DriverContextView<'_>,
        _waiting: WaitingLlmState,
        _llm_response: LlmResponse,
        _text_streamed: bool,
    ) -> Vec<DriverAction> {
        Vec::new()
    }

    fn handle_tool_results(
        &self,
        _ctx: DriverContextView<'_>,
        _completed: Vec<CompletedToolCall>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }

    fn handle_exec_result(
        &self,
        _ctx: DriverContextView<'_>,
        waiting: WaitingExecState,
        _result: Result<crate::ExecResponse, String>,
    ) -> Vec<DriverAction> {
        let state = waiting
            .into_driver_state()
            .as_str()
            .expect("exec driver state")
            .to_string();
        vec![
            DriverAction::AppendEvents(vec![conversation_event(text_message(
                MessageRole::User,
                state,
            ))]),
            DriverAction::StartCheckpoint {
                checkpoint: CheckpointKind::BeforeCompletion,
                on_empty: CheckpointResumeAction::Finish(assistant_done("done")),
            },
        ]
    }
}

struct SyncThenAdvanceDriver;

impl ProtocolDriverHandle for SyncThenAdvanceDriver {
    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        vec![DriverAction::StartLlm {
            request: ctx.project_llm_request(true),
            driver_state: None,
        }]
    }

    fn handle_llm_success(
        &self,
        ctx: DriverContextView<'_>,
        _waiting: WaitingLlmState,
        _llm_response: LlmResponse,
        _text_streamed: bool,
    ) -> Vec<DriverAction> {
        if ctx.protocol_iteration() == ctx.protocol_run_offset() {
            vec![
                DriverAction::AdvanceProtocolIteration,
                DriverAction::StartCheckpoint {
                    checkpoint: CheckpointKind::BeforeCompletion,
                    on_empty: CheckpointResumeAction::PrepareIteration,
                },
            ]
        } else {
            vec![DriverAction::Finish(assistant_done("done"))]
        }
    }

    fn handle_tool_results(
        &self,
        _ctx: DriverContextView<'_>,
        _completed: Vec<CompletedToolCall>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }

    fn handle_exec_result(
        &self,
        _ctx: DriverContextView<'_>,
        _waiting: WaitingExecState,
        _result: Result<crate::ExecResponse, String>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }
}

struct ToolBatchDriver;

impl ProtocolDriverHandle for ToolBatchDriver {
    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        vec![DriverAction::StartLlm {
            request: ctx.project_llm_request(true),
            driver_state: None,
        }]
    }

    fn handle_llm_success(
        &self,
        _ctx: DriverContextView<'_>,
        _waiting: WaitingLlmState,
        _llm_response: LlmResponse,
        _text_streamed: bool,
    ) -> Vec<DriverAction> {
        vec![DriverAction::StartTools {
            calls: vec![
                PendingToolCall {
                    call_id: "call-read".to_string(),
                    tool_name: "read_file".to_string(),
                    args: serde_json::json!({"path": "a.txt"}),
                    replay: None,
                },
                PendingToolCall {
                    call_id: "call-search".to_string(),
                    tool_name: "search".to_string(),
                    args: serde_json::json!({"q": "needle"}),
                    replay: Some(ProviderReplayMeta {
                        item_id: Some("provider-item-2".to_string()),
                        opaque: Some("opaque-provider-state".to_string()),
                        ..ProviderReplayMeta::default()
                    }),
                },
            ],
        }]
    }

    fn handle_tool_results(
        &self,
        _ctx: DriverContextView<'_>,
        completed: Vec<CompletedToolCall>,
    ) -> Vec<DriverAction> {
        let summary = completed
            .iter()
            .map(|call| format!("{}:{:?}", call.tool_name, call.output.status()))
            .collect::<Vec<_>>()
            .join(",");
        vec![
            DriverAction::AppendEvents(vec![conversation_event(text_message(
                MessageRole::User,
                summary,
            ))]),
            DriverAction::StartCheckpoint {
                checkpoint: CheckpointKind::AfterWork,
                on_empty: CheckpointResumeAction::PrepareIteration,
            },
        ]
    }

    fn handle_exec_result(
        &self,
        _ctx: DriverContextView<'_>,
        _waiting: WaitingExecState,
        _result: Result<crate::ExecResponse, String>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }
}

#[test]
fn progress_emits_only_new_event_delta() {
    let initial = user_message("hello");
    let mut machine = TurnMachine::new(
        test_config(Arc::new(ProseDriver)),
        vec![initial.clone()],
        Arc::new(vec![conversation_event(initial)]),
        0,
    );

    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call").0;
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse::default()),
    });

    let effects = drain_effects(&mut machine);
    let delta = progress_event_delta(&effects).expect("progress delta");
    assert_eq!(delta.len(), 1);
    assert!(matches!(
        &delta[0],
        SessionHistoryRecord::Conversation(record)
            if record.role == MessageRole::Assistant
    ));
}

#[test]
fn progress_without_new_events_emits_empty_delta() {
    let mut machine = TurnMachine::new(
        test_config(Arc::new(SyncThenAdvanceDriver)),
        vec![user_message("hello")],
        Arc::new(Vec::new()),
        0,
    );

    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call").0;
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse::default()),
    });

    let effects = drain_effects(&mut machine);
    let delta = progress_event_delta(&effects).expect("progress delta");
    assert!(delta.is_empty());
}

#[test]
fn done_carries_unreported_final_delta() {
    let mut machine = TurnMachine::new(
        test_config(Arc::new(ProseDriver)),
        Vec::new(),
        Arc::new(Vec::new()),
        0,
    );
    machine.append_event(conversation_event(text_message(
        MessageRole::Assistant,
        "final",
    )));
    machine.finish_with_outcome(assistant_done("final"));

    let effects = drain_effects(&mut machine);
    let delta = done_event_delta(&effects).expect("done delta");
    assert_eq!(delta.len(), 1);
    assert!(matches!(
        &delta[0],
        SessionHistoryRecord::Conversation(record)
            if record.role == MessageRole::Assistant
    ));
}

#[test]
fn llm_request_includes_image_prompt_parts_for_attached_images() {
    let config = test_config(Arc::new(ProseDriver));
    let msgs = vec![Message {
        id: "m0".to_string(),
        role: MessageRole::User,
        parts: vec![
            Part::attachment_part(
                "m0.p0".to_string(),
                String::new(),
                Some(PartAttachment {
                    source: crate::llm::types::AttachmentSource::stored(test_attachment_ref(3)),
                }),
            ),
            Part::text("m0.p1".to_string(), "explain this".to_string(), None),
        ]
        .into(),
        origin: None,
    }];
    let mut machine = TurnMachine::new(config, msgs, Arc::new(Vec::new()), 0);

    let effects = drain_effects(&mut machine);
    let (_, request) = find_llm_call(&effects).expect("llm call");

    use crate::llm::types::LlmContentBlock;
    assert_eq!(request.attachments.len(), 1);
    assert!(request.messages.iter().any(|msg| {
        msg.blocks
            .iter()
            .any(|block| matches!(block, LlmContentBlock::Attachment { attachment_idx: 0 }))
    }));
    assert!(request.messages.iter().any(|msg| {
        msg.blocks.iter().any(|block| match block {
            LlmContentBlock::Text { text, .. } => text.contains("explain this"),
            _ => false,
        })
    }));
}

#[test]
fn driver_can_finish_via_checkpoint() {
    let config = test_config(Arc::new(ProseDriver));
    let msgs = vec![user_message("hello")];
    let mut machine = TurnMachine::new(config, msgs, Arc::new(Vec::new()), 0);

    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call").0;
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "Hello".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    let (checkpoint_id, checkpoint) = find_checkpoint(&effects).expect("checkpoint");
    assert_eq!(checkpoint, CheckpointKind::BeforeCompletion);
    machine.handle_response(Response::Checkpoint {
        id: checkpoint_id,
        delivery: CheckpointDelivery::default(),
    });

    let effects = drain_effects(&mut machine);
    assert!(find_done(&effects).is_some());
    assert!(machine.is_done());
}

#[test]
fn checkpoint_before_llm_completion_reissues_same_logical_llm_call() {
    let config = test_config(Arc::new(ProseDriver));
    let mut machine =
        TurnMachine::new(config, vec![user_message("hello")], Arc::new(Vec::new()), 0);

    let effects = drain_effects(&mut machine);
    let (llm_id, request) = find_llm_call(&effects).expect("llm call");
    let checkpoint = roundtrip_checkpoint(machine.checkpoint());
    let mut restored =
        TurnMachine::restore_from_checkpoint(test_config(Arc::new(ProseDriver)), checkpoint);

    let effects = drain_effects(&mut restored);
    let (restored_id, restored_request) = find_llm_call(&effects).expect("restored llm call");
    assert_eq!(*restored_id, *llm_id);
    assert_eq!(restored_request.model, request.model);
    assert_eq!(restored_request.messages, request.messages);
}

#[test]
fn checkpoint_after_llm_result_replays_checkpoint_without_second_llm() {
    let config = test_config(Arc::new(ProseDriver));
    let mut machine =
        TurnMachine::new(config, vec![user_message("hello")], Arc::new(Vec::new()), 0);

    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call").0;
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "Hello".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    });

    let checkpoint = roundtrip_checkpoint(machine.checkpoint());
    let mut restored =
        TurnMachine::restore_from_checkpoint(test_config(Arc::new(ProseDriver)), checkpoint);
    let effects = drain_effects(&mut restored);

    assert!(find_llm_call(&effects).is_none());
    let (checkpoint_id, checkpoint) = find_checkpoint(&effects).expect("completion checkpoint");
    assert_eq!(checkpoint, CheckpointKind::BeforeCompletion);
    restored.handle_response(Response::Checkpoint {
        id: checkpoint_id,
        delivery: CheckpointDelivery::default(),
    });
    let effects = drain_effects(&mut restored);
    assert!(find_done(&effects).is_some());
}

#[test]
fn output_limit_stops_as_incomplete_without_assistant_message() {
    let config = test_config(Arc::new(ProseDriver));
    let msgs = vec![user_message("hello")];
    let mut machine = TurnMachine::new(config, msgs, Arc::new(Vec::new()), 0);

    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call").0;
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "partial".to_string(),
                response_meta: None,
            }],
            terminal_reason: LlmTerminalReason::OutputLimit,
            terminal_diagnostic: Some("hit output limit".to_string()),
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    let (messages, _) = find_done(&effects).expect("done");
    assert!(machine.is_done());
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .count(),
        0
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(SessionStreamEvent::TextDelta { content }) if content == "partial"
    )));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(SessionStreamEvent::TurnOutcome {
            outcome: TurnOutcome::Stopped(TurnStop::Incomplete)
        })
    )));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(SessionStreamEvent::Error {
            envelope: Some(envelope),
            ..
        }) if envelope.terminal_reason == Some(LlmTerminalReason::OutputLimit)
    )));
}

#[test]
fn zero_output_limit_with_a_full_prompt_refines_to_context_overflow() {
    let mut config = test_config(Arc::new(ProseDriver));
    config.max_context_tokens = Some(100);
    let msgs = vec![user_message("hello")];
    let mut machine = TurnMachine::new(config, msgs, Arc::new(Vec::new()), 0);

    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call").0;
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            terminal_reason: LlmTerminalReason::OutputLimit,
            // The refinement reads the whole prompt-side subtotal, not just
            // `input_tokens`: 60 + 30 + 8 = 98 of a 100-token window.
            usage: crate::llm::types::LlmUsage {
                input_tokens: 60,
                output_tokens: 0,
                cache_read_input_tokens: 30,
                cache_write_input_tokens: 8,
                reasoning_output_tokens: 0,
            },
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(SessionStreamEvent::Error {
            envelope: Some(envelope),
            ..
        }) if envelope.terminal_reason == Some(LlmTerminalReason::ContextOverflow)
    )));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(SessionStreamEvent::TurnOutcome {
            outcome: TurnOutcome::Stopped(TurnStop::ProviderError)
        })
    )));
}

/// Raw provider counters are admitted once, before the context-window
/// refinement or the cumulative merge aggregates them. The prompt-side subtotal
/// is checked separately from the canonical total because signed counters let a
/// negative `output_tokens` hold the canonical total in range.
#[test]
fn provider_prompt_subtotal_overflow_fails_the_turn_at_ingress() {
    let mut config = test_config(Arc::new(ProseDriver));
    config.max_context_tokens = Some(100);
    let msgs = vec![user_message("hello")];
    let mut machine = TurnMachine::new(config, msgs, Arc::new(Vec::new()), 0);

    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call").0;
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            terminal_reason: LlmTerminalReason::OutputLimit,
            usage: crate::llm::types::LlmUsage {
                input_tokens: i64::MAX,
                output_tokens: i64::MIN,
                cache_read_input_tokens: i64::MAX,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            },
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    assert!(machine.is_done());
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(SessionStreamEvent::Error {
            envelope: Some(envelope),
            ..
        }) if envelope.kind == "token_usage_accounting"
            && envelope.code.as_deref() == Some("token_usage_overflow")
            && envelope.user_message.contains("input_total_tokens")
    )));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(SessionStreamEvent::TurnOutcome {
            outcome: TurnOutcome::Stopped(TurnStop::RuntimeError)
        })
    )));
    // Nothing about the rejected response reaches a usage consumer.
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Emit(SessionStreamEvent::TokenUsage { .. })))
    );
}

#[test]
fn context_overflow_response_stops_as_provider_error() {
    let config = test_config(Arc::new(ProseDriver));
    let msgs = vec![user_message("hello")];
    let mut machine = TurnMachine::new(config, msgs, Arc::new(Vec::new()), 0);

    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call").0;
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            terminal_reason: LlmTerminalReason::ContextOverflow,
            terminal_diagnostic: Some("context window exceeded".to_string()),
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    assert!(find_done(&effects).is_some());
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(SessionStreamEvent::TurnOutcome {
            outcome: TurnOutcome::Stopped(TurnStop::ProviderError)
        })
    )));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Emit(SessionStreamEvent::Error {
            envelope: Some(envelope),
            ..
        }) if envelope.terminal_reason == Some(LlmTerminalReason::ContextOverflow)
    )));
}

#[test]
fn checkpoint_messages_resume_prepare_protocol_iteration() {
    let config = test_config(Arc::new(ProseDriver));
    let msgs = vec![user_message("hello")];
    let mut machine = TurnMachine::new(config, msgs, Arc::new(Vec::new()), 0);

    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call").0;
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "Hello".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    let (checkpoint_id, checkpoint) = find_checkpoint(&effects).expect("checkpoint");
    assert_eq!(checkpoint, CheckpointKind::BeforeCompletion);
    machine.handle_response(Response::Checkpoint {
        id: checkpoint_id,
        delivery: CheckpointDelivery {
            messages: vec![PluginMessage::text(MessageRole::User, "one more thing")],
            ..CheckpointDelivery::default()
        },
    });

    let effects = drain_effects(&mut machine);
    let (progress_messages, progress_iteration) =
        find_progress(&effects).expect("checkpoint progress");
    assert_eq!(progress_iteration, 1);
    assert!(progress_messages.iter().any(|message| {
        message.role == MessageRole::User
            && message
                .parts
                .iter()
                .any(|part| part.content == "one more thing")
    }));
    let progress_idx = effects
        .iter()
        .position(|effect| matches!(effect, Effect::Progress { .. }))
        .expect("progress index");
    let llm_idx = effects
        .iter()
        .position(|effect| matches!(effect, Effect::LlmCall { .. }))
        .expect("llm index");
    assert!(progress_idx < llm_idx);
    assert!(find_llm_call(&effects).is_some());
    assert!(machine.messages().iter().any(|message| {
        message.role == MessageRole::User
            && message
                .parts
                .iter()
                .any(|part| part.content == "one more thing")
    }));
}

#[test]
fn checkpoint_preserves_parallel_tool_batch_before_any_result() {
    let config = test_config(Arc::new(ToolBatchDriver));
    let mut machine = TurnMachine::new(
        config,
        vec![user_message("use tools")],
        Arc::new(Vec::new()),
        0,
    );

    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call").0;
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse::default()),
    });
    let effects = drain_effects(&mut machine);
    let (tool_id, calls) = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::ToolCalls { id, calls } => Some((*id, calls.clone())),
            _ => None,
        })
        .expect("tool batch");
    assert_eq!(calls.len(), 2);

    let checkpoint = roundtrip_checkpoint(machine.checkpoint());
    let mut restored =
        TurnMachine::restore_from_checkpoint(test_config(Arc::new(ToolBatchDriver)), checkpoint);
    let effects = drain_effects(&mut restored);
    let (restored_tool_id, restored_calls) = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::ToolCalls { id, calls } => Some((*id, calls)),
            _ => None,
        })
        .expect("restored tool batch");
    assert_eq!(restored_tool_id, tool_id);
    assert_eq!(restored_calls.len(), 2);
    assert_eq!(restored_calls[0].call_id, "call-read");
    assert_eq!(restored_calls[1].tool_name, "search");
    assert_eq!(
        restored_calls[1]
            .replay
            .as_ref()
            .and_then(|replay| replay.item_id.as_deref()),
        Some("provider-item-2")
    );
}

#[test]
fn checkpoint_after_mixed_tool_batch_results_replays_model_feedback_once() {
    let config = test_config(Arc::new(ToolBatchDriver));
    let mut machine = TurnMachine::new(
        config,
        vec![user_message("use tools")],
        Arc::new(Vec::new()),
        0,
    );

    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call").0;
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse::default()),
    });
    let effects = drain_effects(&mut machine);
    let tool_id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::ToolCalls { id, .. } => Some(*id),
            _ => None,
        })
        .expect("tool batch");

    machine.handle_response(Response::ToolResults {
        id: tool_id,
        results: vec![
            completed_tool(
                "call-read",
                "read_file",
                serde_json::json!({"path":"a.txt"}),
                ToolCallOutput::success(serde_json::json!("contents")),
            ),
            completed_tool(
                "call-search",
                "search",
                serde_json::json!({"q":"needle"}),
                ToolCallOutput::failure(ToolFailure::tool(
                    ToolFailureClass::Execution,
                    "search_failed",
                    "index unavailable",
                )),
            ),
            completed_tool(
                "call-slow",
                "slow_tool",
                serde_json::json!({}),
                ToolCallOutput::cancelled(ToolCancellation::runtime("cancelled by parent")),
            ),
        ],
    });

    let checkpoint = roundtrip_checkpoint(machine.checkpoint());
    let mut restored =
        TurnMachine::restore_from_checkpoint(test_config(Arc::new(ToolBatchDriver)), checkpoint);
    let effects = drain_effects(&mut restored);
    assert!(find_llm_call(&effects).is_none());
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Progress { messages, .. }
            if messages.iter().any(|message| message.parts.iter().any(|part|
                part.content.contains("read_file:Success")
                    && part.content.contains("search:Failure")
                    && part.content.contains("slow_tool:Cancelled")
            ))
    )));
    let (checkpoint_id, checkpoint) = find_checkpoint(&effects).expect("after-work checkpoint");
    assert_eq!(checkpoint, CheckpointKind::AfterWork);
    restored.handle_response(Response::Checkpoint {
        id: checkpoint_id,
        delivery: CheckpointDelivery::default(),
    });
    let effects = drain_effects(&mut restored);
    assert!(find_llm_call(&effects).is_some());
}

#[test]
fn exec_driver_state_round_trip() {
    let config = test_config(Arc::new(ExecDriver));
    let msgs = vec![user_message("run code")];
    let mut machine = TurnMachine::new(config, msgs, Arc::new(Vec::new()), 0);

    let effects = drain_effects(&mut machine);
    let (exec_id, code) = find_exec_call(&effects).expect("exec call");
    assert_eq!(code, "print 1");
    machine.handle_response(Response::ExecResult {
        id: *exec_id,
        result: Ok(crate::ExecResponse {
            ..empty_exec_response()
        }),
    });

    let effects = drain_effects(&mut machine);
    let (checkpoint_id, checkpoint) = find_checkpoint(&effects).expect("checkpoint");
    assert_eq!(checkpoint, CheckpointKind::BeforeCompletion);
    machine.handle_response(Response::Checkpoint {
        id: checkpoint_id,
        delivery: CheckpointDelivery::default(),
    });

    let effects = drain_effects(&mut machine);
    let (messages, _) = find_done(&effects).expect("done");
    assert!(messages.iter().any(|message| {
        message
            .parts
            .iter()
            .any(|part| part.content == "exec-state")
    }));
}

#[test]
fn checkpoint_round_trips_waiting_exec_driver_state() {
    let config = test_config(Arc::new(ExecDriver));
    let mut machine =
        TurnMachine::new(config, vec![user_message("hello")], Arc::new(Vec::new()), 0);
    let effects = drain_effects(&mut machine);
    let (exec_id, _) = find_exec_call(&effects).expect("exec call");

    let decoded = roundtrip_checkpoint(machine.checkpoint());

    let mut restored =
        TurnMachine::restore_from_checkpoint(test_config(Arc::new(ExecDriver)), decoded);
    restored.handle_response(Response::ExecResult {
        id: *exec_id,
        result: Ok(crate::ExecResponse {
            ..empty_exec_response()
        }),
    });
    let effects = drain_effects(&mut restored);
    let (checkpoint_id, checkpoint) = find_checkpoint(&effects).expect("checkpoint");
    assert_eq!(checkpoint, CheckpointKind::BeforeCompletion);
    restored.handle_response(Response::Checkpoint {
        id: checkpoint_id,
        delivery: CheckpointDelivery::default(),
    });
    let effects = drain_effects(&mut restored);
    let (messages, _) = find_done(&effects).expect("done");
    assert!(messages.iter().any(|message| {
        message
            .parts
            .iter()
            .any(|part| part.content.contains("exec-state"))
    }));
}

#[test]
fn checkpoint_redelivers_waiting_llm_from_state_only() {
    let config = test_config(Arc::new(ProseDriver));
    let mut machine =
        TurnMachine::new(config, vec![user_message("hello")], Arc::new(Vec::new()), 0);
    let effects = drain_effects(&mut machine);
    assert!(find_llm_call(&effects).is_some());

    let encoded = serde_json::to_value(machine.checkpoint()).expect("checkpoint json");
    assert_eq!(
        encoded["pending_effects"]
            .as_array()
            .expect("pending_effects array")
            .len(),
        0
    );
    assert!(encoded["state"]["WaitingLlm"].get("delivery").is_none());

    let checkpoint: TurnCheckpoint = serde_json::from_value(encoded).expect("checkpoint");
    let mut restored =
        TurnMachine::restore_from_checkpoint(test_config(Arc::new(ProseDriver)), checkpoint);
    let effects = drain_effects(&mut restored);
    assert!(find_llm_call(&effects).is_some());
}

#[test]
fn stale_response_does_not_cancel_checkpoint_redelivery() {
    let config = test_config(Arc::new(ProseDriver));
    let mut machine =
        TurnMachine::new(config, vec![user_message("hello")], Arc::new(Vec::new()), 0);
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call").0;

    let checkpoint = roundtrip_checkpoint(machine.checkpoint());
    let mut restored =
        TurnMachine::restore_from_checkpoint(test_config(Arc::new(ProseDriver)), checkpoint);
    restored.handle_response(Response::LlmComplete {
        id: EffectId(llm_id.0 + 1),
        text_streamed: false,
        result: Ok(LlmResponse::default()),
    });

    let effects = drain_effects(&mut restored);
    let redelivered_id = find_llm_call(&effects)
        .expect("waiting LLM call should remain scheduled after a stale response")
        .0;
    assert_eq!(*redelivered_id, llm_id);
}

#[test]
fn checkpoint_redelivers_waiting_tool_batch_from_state_only() {
    let config = test_config(Arc::new(ToolBatchDriver));
    let mut machine = TurnMachine::new(
        config,
        vec![user_message("use tools")],
        Arc::new(Vec::new()),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call").0;
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse::default()),
    });
    let effects = drain_effects(&mut machine);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::ToolCalls { .. }))
    );

    let encoded = serde_json::to_value(machine.checkpoint()).expect("checkpoint json");
    assert_eq!(
        encoded["pending_effects"]
            .as_array()
            .expect("pending_effects array")
            .len(),
        0
    );

    let checkpoint: TurnCheckpoint = serde_json::from_value(encoded).expect("checkpoint");
    let mut restored =
        TurnMachine::restore_from_checkpoint(test_config(Arc::new(ToolBatchDriver)), checkpoint);
    let effects = drain_effects(&mut restored);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::ToolCalls { .. }))
    );
}

#[test]
fn checkpoint_redelivers_waiting_exec_from_state_only() {
    let config = test_config(Arc::new(ExecDriver));
    let mut machine =
        TurnMachine::new(config, vec![user_message("hello")], Arc::new(Vec::new()), 0);
    let effects = drain_effects(&mut machine);
    assert!(find_exec_call(&effects).is_some());

    let encoded = serde_json::to_value(machine.checkpoint()).expect("checkpoint json");
    assert_eq!(
        encoded["pending_effects"]
            .as_array()
            .expect("pending_effects array")
            .len(),
        0
    );

    let checkpoint: TurnCheckpoint = serde_json::from_value(encoded).expect("checkpoint");
    let mut restored =
        TurnMachine::restore_from_checkpoint(test_config(Arc::new(ExecDriver)), checkpoint);
    let effects = drain_effects(&mut restored);
    assert!(find_exec_call(&effects).is_some());
}

#[test]
fn initial_execution_environment_sync_is_host_only() {
    let mut config = test_config(Arc::new(ProseDriver));
    config.sync_execution_environment = true;
    let mut machine =
        TurnMachine::new(config, vec![user_message("hello")], Arc::new(Vec::new()), 0);

    let effects = drain_effects(&mut machine);
    let (sync_id, update_machine_config) =
        find_execution_environment_sync(&effects).expect("execution environment sync");
    assert!(!update_machine_config);

    machine.handle_response(Response::ExecutionEnvironmentSynced {
        id: sync_id,
        result: Ok(None),
    });

    let effects = drain_effects(&mut machine);
    assert!(find_llm_call(&effects).is_some());
}

#[test]
fn iteration_execution_environment_sync_can_refresh_prompt_and_tools() {
    let mut config = test_config(Arc::new(SyncThenAdvanceDriver));
    config.sync_execution_environment = true;
    config.system_prompt = Arc::from("initial prompt");
    let mut machine =
        TurnMachine::new(config, vec![user_message("hello")], Arc::new(Vec::new()), 0);

    let effects = drain_effects(&mut machine);
    let (initial_sync_id, update_machine_config) =
        find_execution_environment_sync(&effects).expect("initial execution environment sync");
    assert!(!update_machine_config);
    machine.handle_response(Response::ExecutionEnvironmentSynced {
        id: initial_sync_id,
        result: Ok(None),
    });

    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("llm call").0;
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "advance".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    });

    let effects = drain_effects(&mut machine);
    let (checkpoint_id, _) = find_checkpoint(&effects).expect("checkpoint");
    machine.handle_response(Response::Checkpoint {
        id: checkpoint_id,
        delivery: CheckpointDelivery::default(),
    });

    let effects = drain_effects(&mut machine);
    let (sync_id, update_machine_config) = find_execution_environment_sync(&effects)
        .expect("protocol_iteration execution environment sync");
    assert!(update_machine_config);

    machine.handle_response(Response::ExecutionEnvironmentSynced {
        id: sync_id,
        result: Ok(Some(ExecutionEnvironmentSync {
            system_prompt: Arc::from("updated prompt"),
            tool_specs: Arc::new(vec![crate::llm::types::LlmToolSpec {
                name: "new_tool".to_string(),
                description: "desc".to_string(),
                input_schema: serde_json::json!({ "type": "object" }).into(),
                output_schema: serde_json::json!({ "type": "object" }).into(),
            }]),
        })),
    });

    let effects = drain_effects(&mut machine);
    let (_, request) = find_llm_call(&effects).expect("second llm call");
    assert_eq!(request.tools.len(), 1);
    assert_eq!(request.tools[0].name, "new_tool");
    assert!(request.messages.iter().any(|message| {
        message.role == crate::llm::types::LlmRole::System
            && message.blocks.iter().any(|block| {
                matches!(
                    block,
                    crate::llm::types::LlmContentBlock::Text { text, .. }
                        if text.as_ref() == "updated prompt"
                )
            })
    }));
}
