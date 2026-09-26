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

use lash_sansio::TurnId;
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
#[cfg(test)]
use lash_core::session_model::PartKind;
use lash_core::session_model::{
    ConversationRecord, Message, MessageRole, Part, SessionHistoryRecord, SessionStreamEvent,
    reassign_part_ids, shared_parts,
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

const STANDARD_EXECUTION_SECTION: &str = "Call tools directly with their declared JSON arguments. Use `batch` for two or more independent calls (up to 25); make dependent calls after their inputs return. Check each batch result’s success flag before using its value. Answer in prose only when no tool is needed.";

const BATCH_MAX_TOOL_CALLS: usize = 25;
const STANDARD_PROTOCOL_PLUGIN_ID: &str = "standard_protocol";

/// Plugin factory that installs the standard-protocol driver,
/// session plugin, and native tool catalog.
#[derive(Default)]
pub struct StandardProtocolPluginFactory {
    config: StandardProtocolConfig,
}

/// Host construction-time standard-mode presentation settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StandardProtocolConfig {
    pub discovery: Option<lash_core::ToolDiscovery>,
}

impl StandardProtocolPluginFactory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_config(config: StandardProtocolConfig) -> Self {
        Self { config }
    }
}

impl PluginFactory for StandardProtocolPluginFactory {
    fn id(&self) -> &'static str {
        STANDARD_PROTOCOL_PLUGIN_ID
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(StandardProtocolPlugin {
            config: self.config.clone(),
        }))
    }
}

struct StandardProtocolPlugin {
    config: StandardProtocolConfig,
}

impl SessionPlugin for StandardProtocolPlugin {
    fn id(&self) -> &'static str {
        STANDARD_PROTOCOL_PLUGIN_ID
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        reg.protocol().session(Arc::new(StandardProtocolSession))?;
        reg.protocol()
            .protocol_driver(Arc::new(StandardProtocolDriver {
                config: self.config.clone(),
            }))?;
        reg.tools()
            .orchestrating(standard_batch_orchestrating_tool())?;
        let discovery = self.config.discovery.clone();
        reg.tool_catalog().contribute(Arc::new(move |ctx| {
            validate_discovery(&ctx.tools, discovery.as_ref())?;
            Ok(Default::default())
        }));
        Ok(())
    }
}

fn validate_discovery(
    tools: &[lash_core::ToolManifest],
    discovery: Option<&lash_core::ToolDiscovery>,
) -> Result<(), PluginError> {
    if let Some(discovery) = discovery
        && !tools.iter().any(|tool| {
            tool.inline
                && tool.activation != lash_core::ToolActivation::Internal
                && tool.name == discovery.operation
        })
    {
        return Err(PluginError::InvalidToolDiscovery {
            operation: discovery.operation.clone(),
        });
    }
    Ok(())
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

struct StandardProtocolDriver {
    config: StandardProtocolConfig,
}

impl ProtocolDriverPlugin for StandardProtocolDriver {
    fn build_preamble(&self, input: ProtocolBuildInput) -> TurnDriverPreamble {
        let tool_names = input.tool_catalog.tool_names();
        let tool_names_fingerprint = input.tool_catalog.tool_names_fingerprint();
        TurnDriverPreamble {
            config: TurnDriverConfig::chat(
                Arc::new(StandardDriver {
                    discovery: self.config.discovery.is_some(),
                }),
                true,
            ),
            tool_specs: if self.config.discovery.is_some() {
                input.tool_catalog.inline_tools().model_tool_specs()
            } else {
                input.tool_catalog.model_tool_specs()
            },
            tool_names,
            tool_names_fingerprint,
            execution_prompt: Arc::from(STANDARD_EXECUTION_SECTION),
            prompt_contributions: input.extra_prompt_contributions,
            writer_formats: input.writer_formats,
        }
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
#[expect(
    unsafe_code,
    reason = "OrchestratingToolDef::from_first_party is lash-core's unsafe capability boundary, and this crate owns the tool contract it registers"
)]
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

    let mut specs = specs.into_iter();
    for spec in specs.by_ref().take(BATCH_MAX_TOOL_CALLS) {
        if spec.tool == "batch" {
            immediate_outcomes.push(BatchResultRow::failure(
                spec.index,
                spec.tool,
                serde_json::json!("Tool 'batch' is not allowed inside batch"),
            ));
            continue;
        }
        let Some(manifest) = context.callable_tool_manifest(&spec.tool) else {
            let error = format!("Tool '{}' is unavailable in this session", spec.tool);
            immediate_outcomes.push(BatchResultRow::failure(spec.index, spec.tool, error.into()));
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
        });
        let value = tool_record.output.value_for_projection();
        immediate_outcomes.push(if tool_record.output.is_success() {
            BatchResultRow::success(index, tool_record.tool, value)
        } else {
            BatchResultRow::failure(index, tool_record.tool, value)
        });
    }

    for spec in specs {
        immediate_outcomes.push(BatchResultRow::failure(
            spec.index,
            spec.tool,
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
#[derive(Default)]
pub struct StandardDriver {
    discovery: bool,
}

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

/// A tool call lifted out of the response, keeping the parse verdict on its
/// raw argument text so a malformed call can be refused before dispatch
/// without losing the original text.
struct ReassembledToolCall {
    call_id: String,
    tool_name: String,
    input_json: String,
    args: Result<Value, String>,
    replay: Option<ProviderReplayMeta>,
}

fn reassemble_standard_response(
    assistant_id: &str,
    parts: Vec<StandardResponsePart>,
) -> (Vec<Part>, Vec<ReassembledToolCall>) {
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
                    .map_err(|error| error.to_string());
                calls.push(ReassembledToolCall {
                    call_id: tool_call.call_id,
                    tool_name: tool_call.tool_name,
                    input_json: tool_call.input_json,
                    args,
                    replay: tool_call.replay,
                });
            }
        }
    }

    (message_parts, calls)
}

/// Build the `CompletedToolCall` for a call refused before dispatch. The
/// refusal is typed (`output`) and the model sees its serialized form
/// through `model_return`, the same shape execution results take.
#[expect(
    clippy::expect_used,
    reason = "the typed refusal is a crate-owned ToolCallOutput tree whose serde_json encoding cannot fail"
)]
fn refused_tool_call_completion(
    call_id: String,
    tool_name: String,
    args: Value,
    output: lash_core::ToolCallOutput,
    replay: Option<ProviderReplayMeta>,
) -> CompletedToolCall {
    let model_return = lash_core::facade_support::ModelToolReturn {
        attachment_notices: Vec::new(),
        call_id: call_id.clone(),
        tool_name: tool_name.clone(),
        parts: vec![lash_core::facade_support::ModelToolReturnPart::Text {
            text: serde_json::to_string(&output).expect("typed refusal serializes"),
        }],
    };
    CompletedToolCall {
        call_id,
        tool_name,
        args,
        output,
        model_return,
        intent_outcomes: Vec::new(),
        replay,
    }
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
        waiting: WaitingLlmState<lash_core::HostTurnProtocol>,
        llm_response: LlmResponse,
        text_streamed: bool,
    ) -> Vec<DriverAction> {
        let response = collect_standard_response(&llm_response);
        let mut actions = Vec::new();

        if !text_streamed {
            // Buffered completions publish the same Started/Delta/Completed
            // lifecycle the streaming lane emits, with the same identity
            // scheme (`item_id` where the provider named the item,
            // `part:{index}` otherwise), so replay and live sessions are
            // indistinguishable to hosts.
            let mut ordinal = 0u64;
            for (part_index, part) in response.parts.iter().enumerate() {
                if let StandardResponsePart::Text {
                    text,
                    response_meta,
                } = part
                    && !text.is_empty()
                {
                    let item_id = response_meta.as_ref().and_then(|meta| meta.id.clone());
                    let block = lash_sansio::llm::types::StreamBlockIdentity {
                        id: item_id
                            .clone()
                            .unwrap_or_else(|| format!("part:{part_index}")),
                        ordinal,
                        item_id,
                    };
                    ordinal += 1;
                    actions.push(DriverAction::Emit(SessionStreamEvent::StreamBlockStarted {
                        kind: lash_sansio::llm::types::StreamBlockKind::AssistantText,
                        block: block.clone(),
                    }));
                    actions.push(DriverAction::Emit(SessionStreamEvent::TextDelta {
                        content: text.clone(),
                        block: block.clone(),
                    }));
                    actions.push(DriverAction::Emit(
                        SessionStreamEvent::StreamBlockCompleted {
                            kind: lash_sansio::llm::types::StreamBlockKind::AssistantText,
                            block,
                            content: text.clone(),
                        },
                    ));
                }
            }
        }

        actions.push(DriverAction::Emit(SessionStreamEvent::LlmResponse {
            protocol_iteration: ctx.protocol_iteration(),
            content: response.assistant_text.clone(),
        }));

        let has_tool_calls = response
            .parts
            .iter()
            .any(|part| matches!(part, StandardResponsePart::ToolCall(_)));
        let asst_id = standard_message_id(ctx.turn_id(), ctx.protocol_iteration(), "assistant");
        let (assistant_parts, reassembled_calls) =
            reassemble_standard_response(&asst_id, response.parts);

        if !has_tool_calls {
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

        let mut calls: Vec<PendingToolCall> = Vec::new();
        let mut refused: Vec<CompletedToolCall> = Vec::new();
        for call in reassembled_calls {
            match call.args {
                Err(parse_error) => {
                    let output = lash_core::ToolCallOutput::failure(
                        lash_core::ToolFailure::runtime(
                            lash_core::ToolFailureClass::InvalidRequest,
                            "invalid_tool_call_json",
                            format!(
                                "Tool `{}` was not executed: its arguments were not valid JSON ({parse_error}).",
                                call.tool_name
                            ),
                        ),
                    );
                    refused.push(refused_tool_call_completion(
                        call.call_id,
                        call.tool_name,
                        Value::String(call.input_json),
                        output,
                        call.replay,
                    ));
                }
                Ok(args) => {
                    let call = PendingToolCall {
                        call_id: call.call_id,
                        tool_name: call.tool_name,
                        args,
                        replay: call.replay,
                    };
                    if self.discovery
                        && !waiting
                            .request
                            .tools
                            .iter()
                            .any(|tool| tool.name == call.tool_name)
                    {
                        let output = lash_core::ToolCallOutput::failure(
                            lash_core::ToolFailure::runtime(
                                lash_core::ToolFailureClass::Unavailable,
                                "unknown_tool",
                                format!(
                                    "Tool `{}` was not listed in this request; use a listed discovery operation or batch.",
                                    call.tool_name
                                ),
                            ),
                        );
                        refused.push(refused_tool_call_completion(
                            call.call_id,
                            call.tool_name,
                            call.args,
                            output,
                            call.replay,
                        ));
                    } else {
                        calls.push(call);
                    }
                }
            }
        }
        if !refused.is_empty() {
            let completed = refused;
            actions.push(DriverAction::ReportToolCalls {
                completed: completed.clone(),
            });
            if calls.is_empty() {
                actions.extend(self.handle_tool_results(ctx, completed));
                return actions;
            }
            let mut parts: Vec<Part> = completed
                .into_iter()
                .map(|outcome| tool_result_part(outcome.model_return))
                .collect();
            let message_id =
                standard_message_id(ctx.turn_id(), ctx.protocol_iteration(), "refused_tools");
            reassign_part_ids(&message_id, &mut parts);
            actions.push(DriverAction::AppendEvents(vec![conversation_event(
                Message {
                    id: message_id,
                    role: MessageRole::User,
                    parts: shared_parts(parts),
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

            result_parts.push(tool_result_part(outcome.model_return));
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

    // Equivalent mutant: cargo-mutants' `vec![]` replacement is the same value
    // as this body's `Vec::new()`, so no test can tell them apart.
    #[cfg_attr(test, mutants::skip)]
    fn handle_exec_result(
        &self,
        _ctx: DriverContextView<'_>,
        _waiting: WaitingExecState<lash_core::HostTurnProtocol>,
        _result: Result<lash_core::ExecResponse, String>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }
}

fn standard_message_id(turn_id: &TurnId, protocol_iteration: usize, purpose: &str) -> String {
    format!("m_standard_{turn_id}_{protocol_iteration}_{purpose}")
}

/// The one transcript part answering a tool call: the model return's text
/// and attachment blocks in the tool value's order, under the call's id.
/// Empty text blocks carry nothing and are dropped; a call whose return is
/// empty is still answered, so the transcript stays resume-safe.
fn tool_result_part(model_return: lash_core::facade_support::ModelToolReturn) -> Part {
    let content = model_return
        .parts
        .into_iter()
        .filter(|block| {
            !matches!(
                block,
                lash_core::facade_support::ModelToolReturnPart::Text { text } if text.is_empty()
            )
        })
        .collect();
    Part::tool_result(
        String::new(),
        content,
        model_return.call_id,
        model_return.tool_name,
    )
}

fn conversation_event(message: Message) -> SessionHistoryRecord {
    SessionHistoryRecord::Conversation(ConversationRecord::from_message(message))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod discovery_tests;

#[cfg(test)]
mod tool_result_tests;

#[cfg(test)]
mod driver_contract_tests;

#[cfg(test)]
mod provider_part_persistence_tests;
