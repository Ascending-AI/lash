//! A tool protocol that commits a protocol record of its own beside the
//! model's call, while that call is unanswered: the standard protocol's
//! round with one record appended mid-call (FIG-5588).

use std::sync::Arc;

use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_core::plugin::{ProtocolDriverPlugin, ProtocolSessionPlugin};
use lash_core::sansio::{CheckpointResumeAction, CompletedToolCall, PendingToolCall, PendingWork};
use lash_core::session_model::ConversationRecord;
use lash_core::{
    DriverAction, DriverContextView, Message, MessageRole, Part, SessionHistoryRecord,
};
use lash_sansio::{TurnFinish, TurnOutcome, shared_parts};

/// The protocol's plugin id, and the owner of the record it appends.
pub const NOTED_PROTOCOL: &str = "noted_protocol";

/// The record the protocol appends between the model's call and its result.
pub fn mid_call_note() -> lash_core::ProtocolEvent {
    lash_core::ProtocolEvent {
        plugin_id: NOTED_PROTOCOL.to_owned(),
        payload: serde_json::json!({ "note": "appended while the call is unanswered" }),
    }
}

pub struct NotedProtocolFactory;

impl lash_core::facade_support::PluginFactory for NotedProtocolFactory {
    fn id(&self) -> &'static str {
        NOTED_PROTOCOL
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::facade_support::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(NotedProtocolPlugin))
    }
}

impl lash_core::plugin::PluginDefinition for NotedProtocolFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(NOTED_PROTOCOL)
    }
}

struct NotedProtocolPlugin;

impl lash_core::facade_support::SessionPlugin for NotedProtocolPlugin {
    fn id(&self) -> &'static str {
        NOTED_PROTOCOL
    }

    fn register(
        &self,
        registrar: &mut lash_core::facade_support::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        registrar.protocol().session(Arc::new(NotedSession))?;
        registrar
            .protocol()
            .protocol_driver(Arc::new(NotedDriverPlugin))?;
        Ok(())
    }
}

struct NotedSession;

#[async_trait::async_trait]
impl ProtocolSessionPlugin for NotedSession {}

struct NotedDriverPlugin;

impl ProtocolDriverPlugin for NotedDriverPlugin {
    fn build_preamble(
        &self,
        input: lash_core::ProtocolBuildInput,
    ) -> lash_core::TurnDriverPreamble {
        lash_core::TurnDriverPreamble {
            config: lash_core::TurnDriverConfig::chat(Arc::new(NotedDriver)),
            tool_specs: input.tool_catalog.model_tool_specs(),
            tool_names: input.tool_catalog.tool_names(),
            writer_formats: input.writer_formats,
        }
    }
}

fn message(id: String, role: MessageRole, parts: Vec<Part>) -> SessionHistoryRecord {
    SessionHistoryRecord::Conversation(ConversationRecord::from_message(Message {
        id,
        role,
        parts: shared_parts(parts),
        origin: None,
        reply_marker: None,
    }))
}

/// An answer with calls commits them and the note, then runs their round;
/// the round's results join the transcript and the model is called again.
/// Any other answer finishes the turn with it.
struct NotedDriver;

impl lash_sansio::ProtocolDriverHandle<lash_core::HostTurnProtocol> for NotedDriver {
    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        match ctx.project_llm_request(false) {
            Ok(request) => vec![DriverAction::Start(PendingWork::Llm {
                request,
                driver_state: None,
            })],
            Err(error) => lash_sansio::sansio::stored_history_refusal_actions(error),
        }
    }

    fn handle_llm_success(
        &self,
        ctx: DriverContextView<'_>,
        _request: Arc<LlmRequest>,
        _driver_state: Option<lash_core::ProtocolDriverState>,
        llm_response: LlmResponse,
        calls: &lash_sansio::ResponseToolCalls,
        _text_streamed: bool,
    ) -> Vec<DriverAction> {
        let id = format!("noted-{}-assistant", ctx.protocol_iteration());
        let ids = calls.call_ids(&llm_response);
        if ids.is_empty() {
            let text: String = llm_response
                .parts
                .iter()
                .filter_map(|part| match part {
                    LlmOutputPart::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            return vec![
                DriverAction::AppendEvents(vec![message(
                    id.clone(),
                    MessageRole::Assistant,
                    vec![Part::text(format!("{id}.p0"), text.clone(), None)],
                )]),
                DriverAction::Finish(TurnOutcome::Finished(TurnFinish::AssistantMessage { text })),
            ];
        }
        let mut parts = Vec::new();
        let mut pending = Vec::new();
        let called = llm_response.parts.iter().filter_map(|part| match part {
            LlmOutputPart::ToolCall {
                call_id,
                tool_name,
                input_json,
                ..
            } => Some((call_id, tool_name, input_json)),
            _ => None,
        });
        for ((provider_call_id, tool_name, input_json), call_id) in called.zip(ids) {
            parts.push(Part::tool_call(
                format!("{id}.p{}", parts.len()),
                input_json.clone(),
                call_id.clone(),
                provider_call_id.clone(),
                tool_name.clone(),
                None,
            ));
            pending.push(PendingToolCall {
                call_id,
                provider_call_id: Some(provider_call_id.clone()),
                tool_name: tool_name.clone(),
                args: serde_json::from_str(input_json).unwrap_or_default(),
                replay: None,
            });
        }
        vec![
            DriverAction::AppendEvents(vec![
                message(id, MessageRole::Assistant, parts),
                SessionHistoryRecord::Protocol(mid_call_note()),
            ]),
            DriverAction::Start(PendingWork::tool_round(pending, Default::default())),
        ]
    }

    fn handle_tool_results(
        &self,
        ctx: DriverContextView<'_>,
        completed: Vec<CompletedToolCall>,
    ) -> Vec<DriverAction> {
        let id = format!("noted-{}-results", ctx.protocol_iteration());
        let parts = completed
            .into_iter()
            .enumerate()
            .map(|(index, call)| {
                Part::tool_result(
                    format!("{id}.p{index}"),
                    call.model_return.parts,
                    call.call_id,
                    call.tool_name,
                )
            })
            .collect();
        vec![
            DriverAction::AppendEvents(vec![message(id, MessageRole::User, parts)]),
            DriverAction::AdvanceProtocolIteration,
            DriverAction::Start(PendingWork::Checkpoint {
                checkpoint: lash_core::CheckpointKind::AfterWork,
                on_empty: CheckpointResumeAction::PrepareIteration,
            }),
        ]
    }

    fn handle_exec_result(
        &self,
        _ctx: DriverContextView<'_>,
        _driver_state: lash_core::ProtocolDriverState,
        _result: Result<lash_core::ExecResponse, lash_core::ExecCodeFailure>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }
}
