//! Standard protocol stack: the model drives tools via the native
//! function-calling envelope of its LLM transport.
//!
//! This crate owns:
//!
//! - [`StandardDriver`] — the [`ProtocolDriverHandle`] that dispatches
//!   native tool calls and weaves reasoning parts into the assistant
//!   message timeline.
//! - The [`StandardProtocolPluginFactory`] plugin that claims the
//!   protocol-driver slot so the runtime can run standard-protocol
//!   sessions.
//! - The `batch` tool that composes parallel native tool calls (only
//!   exposed when this protocol stack is installed).

use std::sync::Arc;

use async_trait::async_trait;
use lash_core::llm::types::{ProviderReasoningReplay, ProviderReplayMeta, ResponseTextMeta};
use lash_core::plugin::{
    PluginError, PluginFactory, PluginRegistrar, PluginSessionContext, ProtocolDriverPlugin,
    ProtocolSessionContext, ProtocolSessionPlugin, SessionPlugin,
};
use lash_core::sansio::{
    CheckpointResumeAction, CompletedToolCall, PendingToolCall, ProtocolDriverHandle,
    WaitingExecState, WaitingLlmState,
};
use lash_core::session_model::message::PartAttachment;
use lash_core::session_model::{
    ConversationRecord, Message, MessageRole, Part, PartKind, SessionHistoryRecord,
    SessionStreamEvent, make_error_event, reassign_part_ids, shared_parts,
};

mod batch;
pub use batch::BatchResultRow;
pub mod scenario_contracts;
use batch::batch_tool_definition;
use lash_core::{
    CheckpointKind, DriverAction, DriverContextView, LlmOutputPart, LlmResponse,
    ProtocolBuildInput, SessionError, ToolOutcome, TurnDriverConfig, TurnDriverPreamble,
    facade_support::ToolInvocation, facade_support::TurnFinish, facade_support::TurnOutcome,
    facade_support::TurnStop, facade_support::normalized_response_parts,
    facade_support::reasoning_part,
};
use serde_json::Value;

#[cfg(test)]
use lash_core::{ToolCall, ToolContract, ToolManifest, ToolProvider};

const STANDARD_EXECUTION_SECTION: &str = r#"Use direct tool calls.

- Use `batch` (up to 25 calls) for two or more independent tool calls. Serialize calls when later arguments depend on earlier results.
- For direct conversational requests that need no tools, respond in prose only.

Example — two independent tool calls in one `batch` call:

```json
{
  "tool_calls": [
    { "tool": "<first_tool>", "parameters": { "arg": "value" } },
    { "tool": "<second_tool>", "parameters": { "arg": "value" } }
  ]
}
```"#;

const BATCH_MAX_TOOL_CALLS: usize = 25;
const STANDARD_PROTOCOL_PLUGIN_ID: &str = "standard_protocol";

/// Plugin factory that installs the standard-protocol driver,
/// session plugin, and native tool catalog.
#[derive(Default)]
pub struct StandardProtocolPluginFactory;

impl StandardProtocolPluginFactory {
    pub fn new() -> Self {
        Self
    }
}

impl PluginFactory for StandardProtocolPluginFactory {
    fn id(&self) -> &'static str {
        STANDARD_PROTOCOL_PLUGIN_ID
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(StandardProtocolPlugin))
    }
}

struct StandardProtocolPlugin;

impl SessionPlugin for StandardProtocolPlugin {
    fn id(&self) -> &'static str {
        STANDARD_PROTOCOL_PLUGIN_ID
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        reg.protocol().session(Arc::new(StandardProtocolSession))?;
        reg.protocol()
            .protocol_driver(Arc::new(StandardProtocolDriver))?;
        reg.tools()
            .orchestrating(standard_batch_orchestrating_tool())?;
        Ok(())
    }
}

struct StandardProtocolSession;

#[async_trait]
impl ProtocolSessionPlugin for StandardProtocolSession {
    async fn initialize_session(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<(), SessionError> {
        Ok(())
    }
}

struct StandardProtocolDriver;

impl ProtocolDriverPlugin for StandardProtocolDriver {
    fn build_preamble(&self, input: ProtocolBuildInput) -> TurnDriverPreamble {
        let tool_names = input.tool_catalog.tool_names();
        let tool_names_fingerprint = input.tool_catalog.tool_names_fingerprint();
        TurnDriverPreamble {
            config: TurnDriverConfig::chat(
                Arc::new(StandardDriver),
                true,
                Arc::new(turn_limit_exhausted_message),
            ),
            tool_specs: input.tool_catalog.model_tool_specs(),
            tool_names,
            tool_names_fingerprint,
            execution_prompt: Arc::from(STANDARD_EXECUTION_SECTION),
            prompt_contributions: input.extra_prompt_contributions,
        }
    }
}

fn turn_limit_exhausted_message(message_id: String, max_turns: usize) -> Message {
    Message {
        id: message_id.clone(),
        role: MessageRole::System,
        parts: shared_parts(vec![Part::error(
            format!("{message_id}.p0"),
            format!("Turn limit reached ({max_turns}) before a final assistant response."),
        )]),
        origin: None,
    }
}

/// First-party facade support for hosts whose protocol driver is not Standard
/// but which enable the native batch orchestrating operation in their builder
/// configuration.
///
/// Pass this definition to
/// [`lash_core::facade_support::PluginSpec::with_orchestrating_tool`] from the
/// plugin installed on the facade builder. The definition's capability-bearing
/// constructor remains sealed inside this crate.
pub fn standard_batch_orchestrating_tool() -> lash_core::facade_support::OrchestratingToolDef {
    let implementation: Arc<dyn lash_core::facade_support::OrchestratingToolImplementation> =
        Arc::new(StandardBatchOrchestratingTool);
    // SAFETY: this crate owns the Standard batch tool contract and body.
    unsafe { lash_core::facade_support::OrchestratingToolDef::from_first_party(implementation) }
}

struct StandardBatchOrchestratingTool;

#[async_trait]
impl lash_core::facade_support::OrchestratingToolImplementation for StandardBatchOrchestratingTool {
    fn manifest(&self) -> lash_core::ToolManifest {
        batch_tool_definition().manifest()
    }

    fn contract(&self) -> Arc<lash_core::ToolContract> {
        Arc::new(batch_tool_definition().contract())
    }

    async fn execute(
        &self,
        args: &Value,
        context: &lash_core::facade_support::OrchestrationContext<'_>,
    ) -> ToolOutcome {
        execute_orchestration(args, context).await
    }
}

#[derive(Debug)]
struct BatchCallSpec {
    index: usize,
    tool: String,
    parameters: Value,
}

async fn execute_orchestration(
    args: &Value,
    context: &lash_core::facade_support::OrchestrationContext<'_>,
) -> ToolOutcome {
    let specs = match parse_batch_specs(args) {
        Ok(specs) => specs,
        Err(err) => return err,
    };

    let mut immediate_outcomes = Vec::new();
    let mut parallel_specs = Vec::new();

    for spec in specs.into_iter().take(BATCH_MAX_TOOL_CALLS) {
        if spec.tool == "batch" {
            immediate_outcomes.push(BatchResultRow::failure(
                spec.index,
                spec.tool,
                0,
                serde_json::json!("Tool 'batch' is not allowed inside batch"),
            ));
            continue;
        }
        let Some(manifest) = context.callable_tool_manifest(&spec.tool) else {
            let error = format!("Tool '{}' is unavailable in this session", spec.tool);
            immediate_outcomes.push(BatchResultRow::failure(
                spec.index,
                spec.tool,
                0,
                error.into(),
            ));
            continue;
        };
        parallel_specs.push((
            spec.index,
            ToolInvocation::new(
                format!(
                    "{}:{:02}",
                    context.tool_call_id().unwrap_or("batch"),
                    spec.index
                ),
                manifest.id,
                spec.parameters,
            ),
        ));
    }

    let mut parallel_outcomes = context
        .call_tool_batch(
            parallel_specs
                .iter()
                .map(|(_, invocation)| invocation.clone())
                .collect(),
        )
        .await;
    for ((index, invocation), outcome) in
        parallel_specs.into_iter().zip(parallel_outcomes.drain(..))
    {
        let tool_label = invocation.tool_id.to_string();
        let tool_record = outcome.record.unwrap_or(lash_core::ToolCallRecord {
            call_id: Some(invocation.id),
            tool: tool_label,
            args: invocation.args,
            output: outcome.output,
            duration_ms: 0,
        });
        let value = tool_record.output.value_for_projection();
        // Batch results are replay data. Wall-clock child timing remains
        // available on traces, but cannot participate in a cross-tier
        // literal outcome.
        immediate_outcomes.push(if tool_record.output.is_success() {
            BatchResultRow::success(index, tool_record.tool, 0, value)
        } else {
            BatchResultRow::failure(index, tool_record.tool, 0, value)
        });
    }

    for overflow_index in BATCH_MAX_TOOL_CALLS
        ..args
            .get("tool_calls")
            .and_then(|value| value.as_array())
            .map(|value| value.len())
            .unwrap_or_default()
    {
        immediate_outcomes.push(BatchResultRow::failure(
            overflow_index,
            args.get("tool_calls")
                .and_then(|value| value.as_array())
                .and_then(|items| items.get(overflow_index))
                .and_then(|item| item.get("tool"))
                .and_then(|value| value.as_str())
                .unwrap_or("unknown"),
            0,
            serde_json::json!("Maximum of 25 tool calls allowed in batch"),
        ));
    }

    immediate_outcomes.sort_by_key(|outcome| outcome.index);
    ToolOutcome::ok(serde_json::json!({
        "results": immediate_outcomes,
    }))
}

fn parse_batch_specs(args: &Value) -> Result<Vec<BatchCallSpec>, ToolOutcome> {
    let Some(raw_calls) = args.get("tool_calls").and_then(|value| value.as_array()) else {
        return Err(ToolOutcome::err_fmt(
            "Missing required parameter: tool_calls",
        ));
    };
    if raw_calls.is_empty() {
        return Err(ToolOutcome::err_fmt(
            "Invalid tool_calls: expected at least one call",
        ));
    }

    let mut specs = Vec::with_capacity(raw_calls.len());
    for (index, item) in raw_calls.iter().enumerate() {
        let Some(object) = item.as_object() else {
            return Err(ToolOutcome::err_fmt(format_args!(
                "Invalid tool_calls[{index}]: expected object with tool and parameters"
            )));
        };
        let Some(tool) = object
            .get("tool")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|tool| !tool.is_empty())
        else {
            return Err(ToolOutcome::err_fmt(format_args!(
                "Invalid tool_calls[{index}].tool: expected non-empty string"
            )));
        };
        let parameters = object
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        specs.push(BatchCallSpec {
            index,
            tool: tool.to_string(),
            parameters,
        });
    }

    Ok(specs)
}

// ─────────────────────────────────────────────────────────────────────
// Standard protocol driver
// ─────────────────────────────────────────────────────────────────────

/// Protocol driver for the Standard protocol. Consumes native
/// tool-call envelopes from the LLM, dispatches them via
/// `DriverAction::StartTools`, and splices reasoning parts into the
/// assistant message so provider replay metadata preserves
/// chain-of-thought ordering.
pub struct StandardDriver;

#[derive(Clone, Debug)]
struct StandardToolCall {
    call_id: String,
    tool_name: String,
    input_json: String,
    replay: Option<ProviderReplayMeta>,
}

#[derive(Clone, Debug)]
enum StandardResponsePart {
    Text {
        text: String,
        response_meta: Option<ResponseTextMeta>,
    },
    Reasoning {
        text: String,
        replay: Option<ProviderReasoningReplay>,
    },
    ToolCall(StandardToolCall),
}

#[derive(Debug)]
struct StandardResponse {
    assistant_text: String,
    parts: Vec<StandardResponsePart>,
}

fn collect_standard_response(llm_response: &LlmResponse) -> StandardResponse {
    let mut assistant_text = String::new();
    let mut parts = Vec::new();

    for part in normalized_response_parts(llm_response) {
        match part {
            LlmOutputPart::Text {
                text,
                response_meta,
            } => {
                if text.trim().is_empty() {
                    continue;
                }
                let previous_len = assistant_text.len();
                lash_core::facade_support::append_assistant_text_part(&mut assistant_text, &text);
                parts.push(StandardResponsePart::Text {
                    text: assistant_text[previous_len..].to_string(),
                    response_meta,
                });
            }
            LlmOutputPart::Reasoning { text, replay } => {
                let text = text.trim().to_string();
                if text.is_empty() && replay.as_ref().is_none_or(|meta| meta.is_empty()) {
                    continue;
                }
                parts.push(StandardResponsePart::Reasoning { text, replay });
            }
            LlmOutputPart::ToolCall {
                call_id,
                tool_name,
                input_json,
                replay,
            } => parts.push(StandardResponsePart::ToolCall(StandardToolCall {
                call_id,
                tool_name,
                input_json,
                replay,
            })),
        }
    }

    StandardResponse {
        assistant_text,
        parts,
    }
}

fn reassemble_standard_response(
    assistant_id: &str,
    parts: Vec<StandardResponsePart>,
) -> (Vec<Part>, Vec<PendingToolCall>) {
    let mut message_parts = Vec::with_capacity(parts.len());
    let mut calls = Vec::new();

    for part in parts {
        match part {
            StandardResponsePart::Text {
                text,
                response_meta,
            } => {
                if text.trim().is_empty() {
                    continue;
                }
                message_parts.push(Part::prose(
                    format!("{assistant_id}.p{}", message_parts.len()),
                    text,
                    response_meta,
                ));
            }
            StandardResponsePart::Reasoning { text, replay } => {
                message_parts.push(reasoning_part(
                    assistant_id,
                    message_parts.len(),
                    text,
                    replay,
                ));
            }
            StandardResponsePart::ToolCall(tool_call) => {
                message_parts.push(Part::tool_call(
                    format!("{assistant_id}.p{}", message_parts.len()),
                    tool_call.input_json.clone(),
                    tool_call.call_id.clone(),
                    tool_call.tool_name.clone(),
                    tool_call.replay.clone(),
                ));
                let args = serde_json::from_str::<Value>(&tool_call.input_json)
                    .unwrap_or_else(|_| serde_json::json!({}));
                calls.push(PendingToolCall {
                    call_id: tool_call.call_id,
                    tool_name: tool_call.tool_name,
                    args,
                    replay: tool_call.replay,
                });
            }
        }
    }

    (message_parts, calls)
}

fn last_message_has_tool_result(ctx: &DriverContextView<'_>) -> bool {
    ctx.messages().last().is_some_and(|message| {
        matches!(message.role, MessageRole::User)
            && message
                .parts
                .iter()
                .any(|part| matches!(part.kind, PartKind::ToolResult))
    })
}

impl ProtocolDriverHandle<lash_core::HostTurnProtocol> for StandardDriver {
    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        vec![DriverAction::StartLlm {
            request: ctx.project_llm_request(true),
            driver_state: None,
        }]
    }

    fn handle_llm_success(
        &self,
        ctx: DriverContextView<'_>,
        _waiting: WaitingLlmState<lash_core::HostTurnProtocol>,
        llm_response: LlmResponse,
        text_streamed: bool,
    ) -> Vec<DriverAction> {
        let response = collect_standard_response(&llm_response);
        let mut actions = Vec::new();

        if !text_streamed {
            for part in &response.parts {
                if let StandardResponsePart::Text { text, .. } = part {
                    actions.push(DriverAction::Emit(SessionStreamEvent::TextDelta {
                        content: text.clone(),
                    }));
                }
            }
        }

        actions.push(DriverAction::Emit(SessionStreamEvent::LlmResponse {
            protocol_iteration: ctx.protocol_iteration(),
            content: response.assistant_text.clone(),
            duration_ms: 0,
        }));

        let has_tool_calls = response
            .parts
            .iter()
            .any(|part| matches!(part, StandardResponsePart::ToolCall(_)));
        let asst_id = standard_message_id(ctx.turn_id(), ctx.protocol_iteration(), "assistant");
        let (assistant_parts, calls) = reassemble_standard_response(&asst_id, response.parts);

        if !has_tool_calls {
            if assistant_parts.is_empty() {
                if last_message_has_tool_result(&ctx) {
                    // A model can intentionally complete a tool-only request
                    // with an empty final answer, e.g. when the user says
                    // "do nothing else" after the tool action.
                    actions.push(DriverAction::StartCheckpoint {
                        checkpoint: CheckpointKind::BeforeCompletion,
                        on_empty: CheckpointResumeAction::Finish(TurnOutcome::Finished(
                            TurnFinish::AssistantMessage {
                                text: String::new(),
                            },
                        )),
                    });
                    return actions;
                }
                actions.extend(empty_response_actions());
                return actions;
            }

            actions.push(DriverAction::AppendEvents(vec![conversation_event(
                Message {
                    id: asst_id,
                    role: MessageRole::Assistant,
                    parts: shared_parts(assistant_parts),
                    origin: None,
                },
            )]));
            actions.push(DriverAction::StartCheckpoint {
                checkpoint: CheckpointKind::BeforeCompletion,
                on_empty: CheckpointResumeAction::Finish(TurnOutcome::Finished(
                    TurnFinish::AssistantMessage {
                        text: response.assistant_text,
                    },
                )),
            });
            return actions;
        }

        if !assistant_parts.is_empty() {
            actions.push(DriverAction::AppendEvents(vec![conversation_event(
                Message {
                    id: asst_id,
                    role: MessageRole::Assistant,
                    parts: shared_parts(assistant_parts),
                    origin: None,
                },
            )]));
        }

        actions.push(DriverAction::StartTools { calls });
        actions
    }

    fn handle_tool_results(
        &self,
        ctx: DriverContextView<'_>,
        completed: Vec<CompletedToolCall>,
    ) -> Vec<DriverAction> {
        let mut actions = Vec::new();
        let mut result_parts = Vec::new();
        let mut terminal_outcome = None;

        for outcome in completed {
            if terminal_outcome.is_none() && outcome.output.is_success() {
                terminal_outcome = outcome.output.control.as_ref().and_then(|control| {
                    lash_core::turn_outcome_from_tool_control(&outcome.tool_name, control)
                });
            }

            append_model_return_parts(&mut result_parts, outcome.model_return);
        }

        if !result_parts.is_empty() {
            let user_id =
                standard_message_id(ctx.turn_id(), ctx.protocol_iteration(), "tool_results");
            reassign_part_ids(&user_id, &mut result_parts);
            actions.push(DriverAction::AppendEvents(vec![conversation_event(
                Message {
                    id: user_id,
                    role: MessageRole::User,
                    parts: shared_parts(result_parts),
                    origin: None,
                },
            )]));
        }

        if let Some(outcome) = terminal_outcome {
            actions.push(DriverAction::Finish(outcome));
            return actions;
        }

        actions.push(DriverAction::AdvanceProtocolIteration);
        let next_protocol_iteration = ctx.protocol_iteration() + 1;
        if let Some(max_turns) = ctx.turn_budget().max_turns()
            && next_protocol_iteration >= ctx.protocol_run_offset() + max_turns
        {
            let message_id =
                standard_message_id(ctx.turn_id(), next_protocol_iteration, "turn_limit");
            actions.push(DriverAction::AppendEvents(vec![conversation_event(
                turn_limit_exhausted_message(message_id, max_turns),
            )]));
            actions.push(DriverAction::Finish(TurnOutcome::Stopped(
                TurnStop::MaxTurns,
            )));
            return actions;
        }

        actions.push(DriverAction::StartCheckpoint {
            checkpoint: CheckpointKind::AfterWork,
            on_empty: CheckpointResumeAction::PrepareIteration,
        });
        actions
    }

    fn handle_exec_result(
        &self,
        _ctx: DriverContextView<'_>,
        _waiting: WaitingExecState<lash_core::HostTurnProtocol>,
        _result: Result<lash_core::ExecResponse, String>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }
}

fn standard_message_id(turn_id: &str, protocol_iteration: usize, purpose: &str) -> String {
    format!("m_standard_{turn_id}_{protocol_iteration}_{purpose}")
}

fn append_model_return_parts(
    parts: &mut Vec<Part>,
    model_return: lash_core::facade_support::ModelToolReturn,
) {
    for part in model_return.parts {
        match part {
            lash_core::facade_support::ModelToolReturnPart::Text { text } => {
                if text.is_empty() {
                    continue;
                }
                parts.push(Part::tool_result(
                    String::new(),
                    text,
                    model_return.call_id.clone(),
                    model_return.tool_name.clone(),
                ));
            }
            lash_core::facade_support::ModelToolReturnPart::Attachment(source) => {
                parts.push(Part::tool_result_attachment(
                    String::new(),
                    String::new(),
                    PartAttachment { source },
                    model_return.call_id.clone(),
                    model_return.tool_name.clone(),
                ));
            }
        }
    }
}

fn empty_response_actions() -> [DriverAction; 2] {
    [
        DriverAction::Emit(make_error_event(
            "llm_provider",
            Some("empty_response"),
            "Model returned no assistant text or tool calls.",
            None,
        )),
        DriverAction::Finish(TurnOutcome::Stopped(TurnStop::ProviderError)),
    ]
}

fn conversation_event(message: Message) -> SessionHistoryRecord {
    SessionHistoryRecord::Conversation(ConversationRecord::from_message(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::{
        AttachmentId, AttachmentSource, AttachmentTypeMetadata, MediaType, ToolCallOutput,
        ToolValue, facade_support::AttachmentMeta, facade_support::ModelToolReturn,
    };
    use lash_sansio::sync::MutexExt;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::Barrier;
    use tokio::time::{Duration, timeout};

    fn attachment_source(id: &str) -> AttachmentSource {
        AttachmentSource::stored(
            AttachmentMeta::new(
                AttachmentId::parse(id).expect("valid attachment id"),
                MediaType::parse("image/png").unwrap(),
                4,
                Some(AttachmentTypeMetadata::image(Some(1), Some(1))),
                Some("tiny".to_string()),
            )
            .as_ref(),
        )
    }

    #[test]
    fn standard_protocol_factory_id_is_stable_plugin_contract() {
        let factory = StandardProtocolPluginFactory::new();

        assert_eq!(factory.id(), STANDARD_PROTOCOL_PLUGIN_ID);
        assert_eq!(factory.id(), "standard_protocol");
    }

    #[test]
    fn standard_execution_section_uses_only_surviving_tool_examples() {
        for removed_tool in [
            "read_file",
            "\"edit\"",
            "\"write\"",
            "\"glob\"",
            "fetch_url",
            "search_web",
        ] {
            assert!(
                !STANDARD_EXECUTION_SECTION.contains(removed_tool),
                "standard prompt should not mention removed tool `{removed_tool}`"
            );
        }
        assert!(STANDARD_EXECUTION_SECTION.contains("<first_tool>"));
        assert!(STANDARD_EXECUTION_SECTION.contains("<second_tool>"));
    }

    #[test]
    fn protocol_message_ids_include_turn_identity() {
        let first = standard_message_id("turn-1", 0, "assistant");
        let replay = standard_message_id("turn-1", 0, "assistant");
        let next_turn = standard_message_id("turn-2", 0, "assistant");

        assert_eq!(first, replay);
        assert_ne!(first, next_turn);
    }

    fn sequence_part(kind: usize, position: usize) -> (LlmOutputPart, PartKind, String) {
        match kind {
            0 => {
                let marker = format!("text-{position}");
                (
                    LlmOutputPart::Text {
                        text: marker.clone(),
                        response_meta: None,
                    },
                    PartKind::Prose,
                    marker,
                )
            }
            1 => {
                let marker = format!("reasoning-{position}");
                (
                    LlmOutputPart::Reasoning {
                        text: marker.clone(),
                        replay: None,
                    },
                    PartKind::Reasoning,
                    marker,
                )
            }
            2 => {
                let marker = format!("tool-{position}");
                (
                    LlmOutputPart::ToolCall {
                        call_id: format!("call-{position}"),
                        tool_name: marker.clone(),
                        input_json: format!(r#"{{"position":{position}}}"#),
                        replay: None,
                    },
                    PartKind::ToolCall,
                    marker,
                )
            }
            _ => unreachable!("base-three sequence kind"),
        }
    }

    #[test]
    fn mixed_response_sequences_reassemble_in_arrival_order() {
        for len in 1_u32..=5 {
            for encoded in 0..3_usize.pow(len) {
                let mut cursor = encoded;
                let mut input = Vec::with_capacity(len as usize);
                let mut expected = Vec::with_capacity(len as usize);
                for position in 0..len as usize {
                    let (part, kind, marker) = sequence_part(cursor % 3, position);
                    cursor /= 3;
                    input.push(part);
                    expected.push((kind, marker));
                }

                let response = collect_standard_response(&LlmResponse {
                    parts: input,
                    ..LlmResponse::default()
                });
                let (actual, calls) = reassemble_standard_response("assistant", response.parts);

                assert_eq!(actual.len(), expected.len(), "sequence {encoded} len {len}");
                for (position, (actual, (expected_kind, marker))) in
                    actual.iter().zip(expected.iter()).enumerate()
                {
                    assert_eq!(
                        actual.kind, *expected_kind,
                        "kind at {position} in sequence {encoded} len {len}"
                    );
                    match actual.kind {
                        PartKind::ToolCall => assert_eq!(
                            actual.tool_name.as_deref(),
                            Some(marker.as_str()),
                            "tool marker at {position} in sequence {encoded} len {len}"
                        ),
                        _ => assert!(
                            actual.content.contains(marker),
                            "content marker at {position} in sequence {encoded} len {len}: {actual:?}"
                        ),
                    }
                }
                assert_eq!(
                    calls.len(),
                    expected
                        .iter()
                        .filter(|(kind, _)| *kind == PartKind::ToolCall)
                        .count(),
                    "tool dispatch count in sequence {encoded} len {len}"
                );
            }
        }
    }

    #[test]
    fn former_no_tool_and_tool_branch_scrambles_preserve_original_order() {
        let cases = [
            vec![sequence_part(1, 0).0, sequence_part(0, 1).0],
            vec![
                sequence_part(0, 0).0,
                sequence_part(1, 1).0,
                sequence_part(2, 2).0,
            ],
        ];

        let actual = cases
            .into_iter()
            .map(|parts| {
                let response = collect_standard_response(&LlmResponse {
                    parts,
                    ..LlmResponse::default()
                });
                reassemble_standard_response("assistant", response.parts)
                    .0
                    .into_iter()
                    .map(|part| part.kind)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        assert_eq!(actual[0], [PartKind::Reasoning, PartKind::Prose]);
        assert_eq!(
            actual[1],
            [PartKind::Prose, PartKind::Reasoning, PartKind::ToolCall]
        );
    }

    #[derive(Clone, Debug)]
    struct WhitespaceInterleavedProvider;

    #[async_trait::async_trait]
    impl lash_core::facade_support::Provider for WhitespaceInterleavedProvider {
        fn kind(&self) -> &'static str {
            "stub"
        }

        fn route_identity(&self, model: &str) -> lash_core::ProviderRouteIdentity {
            lash_core::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
        }

        fn options(&self) -> lash_core::facade_support::ProviderOptions {
            lash_core::facade_support::ProviderOptions::default()
        }

        fn set_options(&mut self, _options: lash_core::facade_support::ProviderOptions) {}

        fn serialize_config(&self) -> serde_json::Value {
            serde_json::json!({})
        }

        async fn complete(
            &mut self,
            _request: lash_core::LlmRequest,
        ) -> Result<lash_core::LlmResponse, lash_core::facade_support::LlmTransportError> {
            Ok(lash_core::LlmResponse {
                parts: vec![
                    lash_core::LlmOutputPart::Text {
                        text: "a".to_string(),
                        response_meta: None,
                    },
                    lash_core::LlmOutputPart::Text {
                        text: "   ".to_string(),
                        response_meta: None,
                    },
                    lash_core::LlmOutputPart::Reasoning {
                        text: "r".to_string(),
                        replay: None,
                    },
                    lash_core::LlmOutputPart::Text {
                        text: "b".to_string(),
                        response_meta: None,
                    },
                ],
                ..lash_core::LlmResponse::default()
            })
        }

        fn clone_boxed(&self) -> Box<dyn lash_core::facade_support::Provider> {
            Box::new(self.clone())
        }
    }

    #[derive(Clone, Debug)]
    struct BatchRuntimeProvider {
        calls: Arc<AtomicUsize>,
        saw_batch_result: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl lash_core::facade_support::Provider for BatchRuntimeProvider {
        fn kind(&self) -> &'static str {
            "stub"
        }

        fn route_identity(&self, model: &str) -> lash_core::ProviderRouteIdentity {
            lash_core::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
        }

        fn options(&self) -> lash_core::facade_support::ProviderOptions {
            lash_core::facade_support::ProviderOptions::default()
        }

        fn set_options(&mut self, _options: lash_core::facade_support::ProviderOptions) {}

        fn serialize_config(&self) -> serde_json::Value {
            serde_json::json!({})
        }

        async fn complete(
            &mut self,
            request: lash_core::LlmRequest,
        ) -> Result<lash_core::LlmResponse, lash_core::facade_support::LlmTransportError> {
            let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
            if call_index == 0 {
                return Ok(lash_core::LlmResponse {
                    parts: vec![lash_core::LlmOutputPart::ToolCall {
                        call_id: "batch-call".to_string(),
                        tool_name: "batch".to_string(),
                        input_json: serde_json::json!({
                            "tool_calls": [
                                {"tool": "alpha", "parameters": {}},
                                {"tool": "beta", "parameters": {"value": "fail"}},
                                {"tool": "internal_probe", "parameters": {}}
                            ]
                        })
                        .to_string(),
                        replay: None,
                    }],
                    response_metadata: Default::default(),
                    ..lash_core::LlmResponse::default()
                });
            }

            let projected_messages = format!("{:?}", request.messages);
            if projected_messages.contains("alpha") && projected_messages.contains("beta failed") {
                self.saw_batch_result.store(true, Ordering::SeqCst);
            }
            Ok(lash_core::LlmResponse {
                full_text: "done".to_string(),
                parts: vec![lash_core::LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..lash_core::LlmResponse::default()
            })
        }

        fn clone_boxed(&self) -> Box<dyn lash_core::facade_support::Provider> {
            Box::new(self.clone())
        }
    }

    #[derive(Debug)]
    struct BatchRuntimeTools {
        barrier: Arc<Barrier>,
        started: Arc<AtomicUsize>,
        internal_executed: Arc<AtomicUsize>,
    }

    fn runtime_test_tool(name: &str) -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            format!("tool:{name}"),
            name,
            "",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                },
                "additionalProperties": true
            }),
            serde_json::json!({ "type": "string" }),
        )
    }

    #[async_trait::async_trait]
    impl ToolProvider for BatchRuntimeTools {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            vec![
                runtime_test_tool("alpha").manifest(),
                runtime_test_tool("beta").manifest(),
                runtime_test_tool("internal_probe")
                    .with_activation(lash_core::ToolActivation::Internal)
                    .manifest(),
            ]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
            match name {
                "alpha" | "beta" => Some(Arc::new(runtime_test_tool(name).contract())),
                "internal_probe" => Some(Arc::new(
                    runtime_test_tool(name)
                        .with_activation(lash_core::ToolActivation::Internal)
                        .contract(),
                )),
                _ => None,
            }
        }

        async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
            if call.name == "internal_probe" {
                self.internal_executed.fetch_add(1, Ordering::SeqCst);
                return ToolOutcome::ok(serde_json::json!("internal body ran"));
            }
            self.started.fetch_add(1, Ordering::SeqCst);
            if timeout(Duration::from_millis(100), self.barrier.wait())
                .await
                .is_err()
            {
                return ToolOutcome::err_fmt("batch child tools did not run concurrently");
            }
            if call.name == "beta"
                && call.args.get("value").and_then(|value| value.as_str()) == Some("fail")
            {
                return ToolOutcome::err_fmt("beta failed");
            }
            ToolOutcome::ok(serde_json::json!(call.name))
        }
    }

    type RecordedEffectFrame = (lash_core::RuntimeEffectKind, Option<String>);

    #[derive(Clone, Default)]
    struct CountingEffectController {
        frames: Arc<std::sync::Mutex<Vec<RecordedEffectFrame>>>,
    }

    impl CountingEffectController {
        fn count(&self, kind: lash_core::RuntimeEffectKind) -> usize {
            self.frames
                .lock_recover()
                .iter()
                .filter(|(candidate, _)| *candidate == kind)
                .count()
        }

        fn tool_attempt_names(&self) -> Vec<String> {
            let mut names = self
                .frames
                .lock_recover()
                .iter()
                .filter_map(|(kind, name)| {
                    (*kind == lash_core::RuntimeEffectKind::ToolAttempt)
                        .then(|| name.clone())
                        .flatten()
                })
                .collect::<Vec<_>>();
            names.sort();
            names
        }
    }

    #[derive(Default)]
    struct DurableMemoryAttachmentStore {
        inner: lash_core::facade_support::InMemoryAttachmentStore,
    }

    #[async_trait::async_trait]
    impl lash_core::AttachmentStore for DurableMemoryAttachmentStore {
        fn persistence(&self) -> lash_core::AttachmentStorePersistence {
            lash_core::AttachmentStorePersistence::Durable
        }

        async fn put(
            &self,
            bytes: Vec<u8>,
            meta: lash_core::AttachmentCreateMeta,
        ) -> Result<lash_core::AttachmentRef, lash_core::AttachmentStoreError> {
            self.inner.put(bytes, meta).await
        }

        async fn get(
            &self,
            id: &lash_core::AttachmentId,
        ) -> Result<lash_core::StoredAttachment, lash_core::AttachmentStoreError> {
            self.inner.get(id).await
        }

        async fn delete(
            &self,
            id: &lash_core::AttachmentId,
        ) -> Result<(), lash_core::AttachmentStoreError> {
            self.inner.delete(id).await
        }

        async fn list(
            &self,
        ) -> Result<Vec<lash_core::StoredBlobRef>, lash_core::AttachmentStoreError> {
            self.inner.list().await
        }

        async fn head(
            &self,
            id: &lash_core::AttachmentId,
        ) -> Result<Option<lash_core::StoredBlobRef>, lash_core::AttachmentStoreError> {
            self.inner.head(id).await
        }
    }

    #[derive(Default)]
    struct DurableMemoryProcessEnvStore {
        inner: lash_core::facade_support::InMemoryProcessExecutionEnvStore,
    }

    #[async_trait::async_trait]
    impl lash_core::ProcessExecutionEnvStore for DurableMemoryProcessEnvStore {
        async fn put_process_execution_env(
            &self,
            env_ref: &lash_core::ProcessExecutionEnvRef,
            bytes: &[u8],
        ) -> Result<(), lash_core::PluginError> {
            self.inner.put_process_execution_env(env_ref, bytes).await
        }

        async fn get_process_execution_env(
            &self,
            env_ref: &lash_core::ProcessExecutionEnvRef,
        ) -> Result<Option<Vec<u8>>, lash_core::PluginError> {
            self.inner.get_process_execution_env(env_ref).await
        }
    }

    impl lash_core::AwaitEventResolver for CountingEffectController {
        fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
            lash_core::EffectReplayOwnership::Controller
        }
    }

    #[async_trait::async_trait]
    impl lash_core::RuntimeEffectController for CountingEffectController {
        async fn execute_effect(
            &self,
            envelope: lash_core::RuntimeEffectEnvelope,
            local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
        ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError>
        {
            let name = match &envelope.command {
                lash_core::RuntimeEffectCommand::ToolAttempt { call, .. } => {
                    Some(call.tool_name.clone())
                }
                _ => None,
            };
            self.frames
                .lock_recover()
                .push((envelope.command.kind(), name));
            if matches!(
                &envelope.command,
                lash_core::RuntimeEffectCommand::PeekAwaitEvent { .. }
            ) {
                return Ok(lash_core::RuntimeEffectOutcome::PeekAwaitEvent { resolution: None });
            }
            local_executor.execute(envelope).await
        }
    }

    #[tokio::test]
    async fn whitespace_only_text_does_not_split_terminal_history() {
        let provider_handle = lash_core::facade_support::ProviderHandle::new(
            lash_core::facade_support::ProviderComponents::new(Box::new(
                WhitespaceInterleavedProvider,
            )),
        );
        let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
        );
        host.providers.provider_resolver = Arc::new(
            lash_core::facade_support::SingleProviderResolver::new(provider_handle),
        );
        let policy = lash_core::SessionPolicy {
            provider_id: "stub".to_string(),
            model: lash_core::ModelSpec::builder("mock-model")
                .context_window_tokens(200_000)
                .build()
                .expect("valid model"),
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        };
        let scoped_controller = lash_core::ScopedEffectController::shared(
            Arc::new(CountingEffectController::default()),
            lash_core::ExecutionScope::turn("whitespace-response-session", "turn-1"),
        )
        .expect("scoped controller");
        let mut runtime = Box::pin(
            lash_core::facade_support::LashRuntime::builder(
                lash_core::CommitBudget::bounded(1024 * 1024, 512),
                lash_core::QueuedWorkBatchingConfig::new(1),
                lash_core::LeaseOwnerIdentity::opaque(
                    "protocol-standard-test-worker",
                    "protocol-standard-test-boot",
                ),
            )
            .with_session_id("whitespace-response-session")
            .with_policy(policy)
            .with_runtime_host(host)
            .with_plugin_factories(vec![Arc::new(StandardProtocolPluginFactory::new())])
            .build(),
        )
        .await
        .expect("runtime");

        let turn = runtime
            .stream_turn(
                lash_core::TurnInput::text("respond with mixed parts"),
                lash_core::facade_support::TurnOptions::new(
                    tokio_util::sync::CancellationToken::new(),
                    scoped_controller,
                ),
            )
            .await
            .expect("turn");

        let finish_text = match &turn.outcome {
            lash_core::facade_support::TurnOutcome::Finished(
                lash_core::facade_support::TurnFinish::AssistantMessage { text },
            ) => text,
            outcome => panic!("unexpected turn outcome: {outcome:?}"),
        };
        let read_view = turn.state.read_view();
        let assistant_messages = read_view
            .messages()
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .collect::<Vec<_>>();

        assert_eq!(
            assistant_messages.len(),
            1,
            "the terminal output must not materialize a duplicate assistant message"
        );
        let stored = assistant_messages[0];
        assert_eq!(
            stored
                .parts
                .iter()
                .map(|part| part.kind)
                .collect::<Vec<_>>(),
            [PartKind::Prose, PartKind::Reasoning, PartKind::Prose]
        );
        let rendered_text = stored
            .parts
            .iter()
            .filter(|part| {
                matches!(
                    part.kind,
                    PartKind::Prose | PartKind::Text | PartKind::Attachment | PartKind::ToolResult
                )
            })
            .map(|part| part.content.as_str())
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(finish_text, &rendered_text);
    }

    #[tokio::test]
    async fn standard_batch_is_runtime_owned_orchestration_without_an_enclosing_attempt() {
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let saw_batch_result = Arc::new(AtomicBool::new(false));
        let provider = BatchRuntimeProvider {
            calls: Arc::clone(&provider_calls),
            saw_batch_result: Arc::clone(&saw_batch_result),
        };
        let provider_handle = lash_core::facade_support::ProviderHandle::new(
            lash_core::facade_support::ProviderComponents::new(Box::new(provider)),
        );
        let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
        );
        host.providers.provider_resolver = Arc::new(
            lash_core::facade_support::SingleProviderResolver::new(provider_handle),
        );
        host.durability.attachment_store = Arc::new(
            lash_core::facade_support::SessionAttachmentStore::ephemeral(Arc::new(
                DurableMemoryAttachmentStore::default(),
            )),
        );
        host.durability.process_env_store = Arc::new(DurableMemoryProcessEnvStore::default());
        let started = Arc::new(AtomicUsize::new(0));
        let internal_executed = Arc::new(AtomicUsize::new(0));
        let factories: Vec<Arc<dyn lash_core::facade_support::PluginFactory>> = vec![
            Arc::new(StandardProtocolPluginFactory::new()),
            Arc::new(lash_core::plugin::StaticPluginFactory::new(
                "standard-batch-test-tools",
                lash_core::facade_support::PluginSpec::new().with_tool_provider(Arc::new(
                    BatchRuntimeTools {
                        barrier: Arc::new(Barrier::new(2)),
                        started: Arc::clone(&started),
                        internal_executed: Arc::clone(&internal_executed),
                    },
                )),
            )),
        ];
        let policy = lash_core::SessionPolicy {
            provider_id: "stub".to_string(),
            model: lash_core::ModelSpec::builder("mock-model")
                .context_window_tokens(200_000)
                .build()
                .expect("valid model"),
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        };
        let controller = CountingEffectController::default();
        let scoped_controller = lash_core::ScopedEffectController::shared(
            Arc::new(controller.clone()),
            lash_core::ExecutionScope::turn("standard-batch-session", "turn-1"),
        )
        .expect("scoped controller");
        let mut runtime = Box::pin(
            lash_core::facade_support::LashRuntime::builder(
                lash_core::CommitBudget::bounded(1024 * 1024, 512),
                lash_core::QueuedWorkBatchingConfig::new(1),
                lash_core::LeaseOwnerIdentity::opaque(
                    "protocol-standard-test-worker",
                    "protocol-standard-test-boot",
                ),
            )
            .with_session_id("standard-batch-session")
            .with_policy(policy)
            .with_runtime_host(host)
            .with_plugin_factories(factories)
            .build(),
        )
        .await
        .expect("runtime");

        let turn = runtime
            .stream_turn(
                lash_core::TurnInput::text("run the batch"),
                lash_core::facade_support::TurnOptions::new(
                    tokio_util::sync::CancellationToken::new(),
                    scoped_controller,
                ),
            )
            .await
            .expect("turn");

        assert!(matches!(
            turn.outcome,
            lash_core::facade_support::TurnOutcome::Finished(_)
        ));
        assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(
            internal_executed.load(Ordering::SeqCst),
            0,
            "a batch child must not cross normal admission into an Internal provider"
        );
        assert!(saw_batch_result.load(Ordering::SeqCst));
        assert_eq!(controller.count(lash_core::RuntimeEffectKind::ToolBatch), 2);
        assert_eq!(
            controller.count(lash_core::RuntimeEffectKind::ToolAttempt),
            2,
            "only alpha and beta are attempts; the batch body itself has no ToolAttempt frame"
        );
        assert_eq!(
            controller.tool_attempt_names(),
            vec!["alpha".to_string(), "beta".to_string()],
            "the runtime-owned batch orchestration body is never enclosed by ToolAttempt"
        );
    }

    #[test]
    fn tool_attachment_round_trips_to_generic_part() {
        let attachment = attachment_source("att-1");
        let output = ToolCallOutput::success_tool_value(ToolValue::Attachment(attachment.clone()));
        let model_return =
            ModelToolReturn::from_output("call-9".to_string(), "screenshot".to_string(), &output);

        let mut parts: Vec<Part> = Vec::new();
        append_model_return_parts(&mut parts, model_return);

        assert_eq!(parts.len(), 1, "single attachment yields single part");
        let part = &parts[0];
        assert!(matches!(part.kind, PartKind::Attachment));
        assert_eq!(part.content, "");
        assert_eq!(part.tool_call_id.as_deref(), Some("call-9"));
        assert_eq!(part.tool_name.as_deref(), Some("screenshot"));
        let part_attachment = part.attachment.as_ref().expect("attachment present");
        assert_eq!(part_attachment.source, attachment);
    }

    #[test]
    fn tool_text_and_attachment_round_trip_preserves_order() {
        let attachment = attachment_source("att-2");
        let output = ToolCallOutput::success_tool_value(ToolValue::Array(vec![
            ToolValue::String("before".into()),
            ToolValue::Attachment(attachment.clone()),
            ToolValue::String("after".into()),
        ]));
        let model_return =
            ModelToolReturn::from_output("call-10".to_string(), "snap".to_string(), &output);

        let mut parts: Vec<Part> = Vec::new();
        append_model_return_parts(&mut parts, model_return);

        // The array projection emits compact JSON text fragments around the
        // attachment, preserving in-order position.
        assert_eq!(
            parts.len(),
            3,
            "text + attachment + text yields three parts"
        );
        assert!(matches!(parts[0].kind, PartKind::ToolResult));
        assert!(parts[0].content.starts_with("[\"before\""));
        assert!(matches!(parts[1].kind, PartKind::Attachment));
        assert_eq!(
            parts[1].attachment.as_ref().expect("attachment").source,
            attachment
        );
        assert!(matches!(parts[2].kind, PartKind::ToolResult));
        assert!(parts[2].content.ends_with("\"after\"]"));
    }
}
