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
//! - The `batch` protocol sugar: the driver expands each `batch` call into
//!   the step's one tool group beside the response's native calls, and folds
//!   the members' results back into one batch result (ADR 0116 §2).

use lash_sansio::TurnId;
use std::sync::Arc;

use async_trait::async_trait;
use lash_core::llm::types::{ProviderReasoningReplay, ProviderReplayMeta, ResponseTextMeta};
use lash_core::plugin::{
    PluginError, PluginFactory, PluginRegistrar, PluginSessionContext, ProtocolDriverPlugin,
    ProtocolSessionContext, ProtocolSessionPlugin, SessionPlugin,
};
use lash_core::sansio::{
    CheckpointResumeAction, CompletedToolCall, PendingToolCall, PendingWork, ProtocolDriverHandle,
};
#[cfg(test)]
use lash_core::session_model::PartKind;
use lash_core::session_model::{
    ConversationRecord, Message, MessageRole, Part, SessionHistoryRecord, SessionStreamEvent,
    reassign_part_ids, shared_parts,
};

mod batch;
pub mod render;
pub use batch::BatchResultRow;
pub use render::{
    BuiltinToolOutputRenderer, StandardRenderConfig, ToolOutputRenderer, ToolOutputRendererSlot,
    ToolRenderParams,
};
pub mod scenario_contracts;
use batch::batch_tool_definition;
use lash_core::{
    CheckpointKind, DriverAction, DriverContextView, LlmOutputPart, LlmResponse,
    ProtocolBuildInput, SessionError, TurnDriverConfig, TurnDriverPreamble,
    facade_support::TurnFinish, facade_support::TurnOutcome, facade_support::TurnStop,
    facade_support::normalized_response_parts, facade_support::reasoning_part,
};
use serde_json::Value;

#[cfg(test)]
use lash_core::{ToolCall, ToolContract, ToolManifest, ToolOutcome, ToolProvider};

const STANDARD_EXECUTION_TITLE: &str = "Execution";
const STANDARD_PROTOCOL_PLUGIN_ID: &str = "standard_protocol";

/// The execution section of the prompt, naming `batch` and its maximum only
/// when the sugar is offered.
fn standard_execution_section(batch: BatchSugar) -> String {
    match batch {
        BatchSugar::Enabled { max_members } => format!(
            "Call tools directly with their declared JSON arguments. Use `batch` for two or more independent calls (at most {max_members} per batch); make dependent calls after their inputs return. Check each batch result’s success flag before using its value. Answer in prose only when no tool is needed."
        ),
        BatchSugar::Disabled => "Call tools directly with their declared JSON arguments. Make independent calls together; make dependent calls after their inputs return. Answer in prose only when no tool is needed.".to_string(),
    }
}

/// The hard ceiling on members per `batch` call. A configured maximum above
/// it is refused when the plugin builds.
pub const BATCH_MEMBER_CEILING: usize = 64;

/// Whether the driver offers `batch`, and with how many members per call.
///
/// `batch` is protocol sugar, not a tool: the driver expands each call into
/// the step's one tool group beside the response's native calls, so every
/// member starts before any finishes, and folds the members' results into one
/// batch result. It is not a Tool Catalog entry, so tool membership does not
/// apply to it and RLM cells and processes cannot call it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchSugar {
    /// `batch` is offered, with at most `max_members` members per call.
    Enabled { max_members: std::num::NonZeroUsize },
    /// `batch` is not offered. A call named `batch` is an ordinary unknown
    /// tool.
    Disabled,
}

impl Default for BatchSugar {
    fn default() -> Self {
        Self::Enabled {
            max_members: std::num::NonZeroUsize::MIN.saturating_add(BATCH_MEMBER_CEILING - 1),
        }
    }
}

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
    pub render: StandardRenderConfig,
    pub renderer: ToolOutputRendererSlot,
    pub batch: BatchSugar,
}

impl StandardProtocolConfig {
    /// Offer or withhold the `batch` sugar. A maximum above
    /// [`BATCH_MEMBER_CEILING`] is refused when the plugin builds.
    pub fn batch(mut self, sugar: BatchSugar) -> Self {
        self.batch = sugar;
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StandardTurnOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<StandardRenderConfig>,
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
        if let BatchSugar::Enabled { max_members } = self.config.batch
            && max_members.get() > BATCH_MEMBER_CEILING
        {
            return Err(PluginError::InvalidBatchMaximum {
                requested: max_members.get(),
                ceiling: BATCH_MEMBER_CEILING,
            });
        }
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
        let renderer = self.config.renderer.clone();
        reg.tool_results().presenter(Arc::new(move |input| {
            let renderer = renderer.clone();
            Box::pin(async move { render::present(input, &renderer).await })
        }))?;
        reg.protocol().session(Arc::new(StandardProtocolSession))?;
        reg.protocol()
            .protocol_driver(Arc::new(StandardProtocolDriver {
                config: self.config.clone(),
            }))?;
        let discovery = self.config.discovery.clone();
        let batch = self.config.batch;
        reg.tool_catalog().contribute(Arc::new(move |ctx| {
            validate_discovery(&ctx.tools, discovery.as_ref())?;
            validate_batch_name(&ctx.tools, batch)?;
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
        && !tools
            .iter()
            .any(|tool| tool.inline && tool.name == discovery.operation)
    {
        return Err(PluginError::InvalidToolDiscovery {
            operation: discovery.operation.clone(),
        });
    }
    Ok(())
}

/// While the sugar is offered, `batch` names it in every request, so a
/// catalogue tool of that name could never be called: it is refused.
fn validate_batch_name(
    tools: &[lash_core::ToolManifest],
    batch: BatchSugar,
) -> Result<(), PluginError> {
    if matches!(batch, BatchSugar::Enabled { .. })
        && let Some(tool) = tools
            .iter()
            .find(|tool| tool.name == batch::BATCH_TOOL_NAME)
    {
        return Err(PluginError::ResidentToolDuplicateName {
            name: tool.name.clone(),
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
    fn resolve_render(
        &self,
        options: &lash_core::ProtocolTurnOptions,
    ) -> Result<Option<lash_core::RecordedRender>, String> {
        let payload: serde_json::Value = options.decode().map_err(|error| error.to_string())?;
        let patch: StandardTurnOptions = serde_json::from_value(render::without_nulls(payload))
            .map_err(|error| error.to_string())?;
        let resolved = render::resolve(
            &StandardRenderConfig::builtin(),
            &self.config.render,
            &patch.render.unwrap_or_default(),
        )?;
        Ok(Some(lash_core::RecordedRender {
            renderer_id: self.config.renderer.0.id().to_string(),
            params: serde_json::to_value(resolved).map_err(|error| error.to_string())?,
        }))
    }

    fn build_preamble(&self, input: ProtocolBuildInput) -> TurnDriverPreamble {
        let tool_names = input.tool_catalog.tool_names();
        let tool_names_fingerprint = input.tool_catalog.tool_names_fingerprint();
        let catalog_specs = if self.config.discovery.is_some() {
            input.tool_catalog.inline_tools().model_tool_specs()
        } else {
            input.tool_catalog.model_tool_specs()
        };
        let tool_specs = match self.config.batch {
            BatchSugar::Enabled { max_members } => {
                let definition = batch_tool_definition(max_members);
                let model_tool = definition.contract().model_tool(&definition.manifest());
                let mut specs = catalog_specs.as_ref().clone();
                specs.push(lash_core::llm::types::LlmToolSpec {
                    name: model_tool.name,
                    description: model_tool.description,
                    input_schema: model_tool.input_schema,
                    output_schema: model_tool.output_schema,
                });
                Arc::new(specs)
            }
            BatchSugar::Disabled => catalog_specs,
        };
        TurnDriverPreamble {
            config: TurnDriverConfig::chat(
                Arc::new(StandardDriver {
                    discovery: self.config.discovery.is_some(),
                    batch: self.config.batch,
                }),
                true,
            ),
            tool_specs,
            tool_names,
            tool_names_fingerprint,
            execution_title: Arc::from(STANDARD_EXECUTION_TITLE),
            execution_prompt: Arc::from(standard_execution_section(self.config.batch)),
            prompt_contributions: input.extra_prompt_contributions,
            writer_formats: input.writer_formats,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Standard protocol driver
// ─────────────────────────────────────────────────────────────────────

/// Protocol driver for the Standard protocol. Consumes native
/// tool-call envelopes from the LLM, expands `batch` sugar into the step's
/// one tool group and dispatches it via `PendingWork::Tools`, and splices
/// reasoning parts into the assistant message so provider replay metadata
/// preserves chain-of-thought ordering.
#[derive(Default)]
pub struct StandardDriver {
    discovery: bool,
    batch: BatchSugar,
}

#[derive(Clone, Debug)]
struct StandardToolCall {
    call_id: lash_core::ToolCallId,
    provider_call_id: String,
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

fn collect_standard_response(
    llm_response: &LlmResponse,
    calls: &lash_core::sansio::ResponseToolCalls,
) -> StandardResponse {
    let mut assistant_text = String::new();
    let mut parts = Vec::new();
    let mut call_ids = calls.call_ids(llm_response).into_iter();

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
                call_id: provider_call_id,
                tool_name,
                input_json,
                replay,
            } => {
                let Some(call_id) = call_ids.next() else {
                    continue;
                };
                parts.push(StandardResponsePart::ToolCall(StandardToolCall {
                    call_id,
                    provider_call_id,
                    tool_name,
                    input_json,
                    replay,
                }));
            }
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
    call_id: lash_core::ToolCallId,
    provider_call_id: String,
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
                    tool_call.provider_call_id.clone(),
                    tool_call.tool_name.clone(),
                    tool_call.replay.clone(),
                ));
                let args = serde_json::from_str::<Value>(&tool_call.input_json)
                    .map_err(|error| error.to_string());
                calls.push(ReassembledToolCall {
                    call_id: tool_call.call_id,
                    provider_call_id: tool_call.provider_call_id,
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
    call_id: lash_core::ToolCallId,
    provider_call_id: Option<String>,
    tool_name: String,
    args: Value,
    output: lash_core::ToolCallOutput,
    replay: Option<ProviderReplayMeta>,
) -> CompletedToolCall {
    let model_return = lash_core::facade_support::ModelToolReturn {
        attachment_notices: Vec::new(),
        tool_name: tool_name.clone(),
        parts: vec![lash_core::facade_support::ModelToolReturnPart::Text {
            text: serde_json::to_string(&output).expect("typed refusal serializes"),
        }],
    };
    CompletedToolCall {
        call_id,
        provider_call_id,
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
        vec![DriverAction::Start(PendingWork::Llm {
            request: ctx.project_llm_request(true),
            driver_state: None,
        })]
    }

    fn handle_llm_success(
        &self,
        ctx: DriverContextView<'_>,
        request: Arc<lash_core::LlmRequest>,
        _driver_state: Option<lash_core::ProtocolDriverState>,
        llm_response: LlmResponse,
        calls: &lash_core::sansio::ResponseToolCalls,
        text_streamed: bool,
    ) -> Vec<DriverAction> {
        let response = collect_standard_response(&llm_response, calls);
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
            actions.push(DriverAction::Start(PendingWork::Checkpoint {
                checkpoint: CheckpointKind::BeforeCompletion,
                on_empty: CheckpointResumeAction::Finish(TurnOutcome::Finished(
                    TurnFinish::AssistantMessage {
                        text: response.assistant_text,
                    },
                )),
            }));
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
                        Some(call.provider_call_id),
                        call.tool_name,
                        Value::String(call.input_json),
                        output,
                        call.replay,
                    ));
                }
                Ok(args) => {
                    let call = PendingToolCall {
                        call_id: call.call_id,
                        provider_call_id: Some(call.provider_call_id),
                        tool_name: call.tool_name,
                        args,
                        replay: call.replay,
                    };
                    // A `batch` wrapper is listed whenever the sugar is on,
                    // and its members resolve against the session's callable
                    // catalog, not the request's listed tools.
                    if self.discovery
                        && !request.tools.iter().any(|tool| tool.name == call.tool_name)
                    {
                        let output = lash_core::ToolCallOutput::failure(
                            lash_core::ToolFailure::runtime(
                                lash_core::ToolFailureClass::Unavailable,
                                "unknown_tool",
                                match self.batch {
                                    BatchSugar::Enabled { .. } => format!(
                                        "Tool `{}` was not listed in this request; use a listed discovery operation or batch.",
                                        call.tool_name
                                    ),
                                    BatchSugar::Disabled => format!(
                                        "Tool `{}` was not listed in this request; use a listed discovery operation.",
                                        call.tool_name
                                    ),
                                },
                            ),
                        );
                        refused.push(refused_tool_call_completion(
                            call.call_id,
                            call.provider_call_id,
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
        let expansion = match self.batch {
            BatchSugar::Enabled { max_members } => batch::expand(calls, max_members),
            BatchSugar::Disabled => batch::Expansion {
                calls,
                ..batch::Expansion::default()
            },
        };
        let calls = expansion.calls;
        refused.extend(expansion.refused.into_iter().map(|(call, output)| {
            refused_tool_call_completion(
                call.call_id,
                call.provider_call_id,
                call.tool_name,
                call.args,
                output,
                call.replay,
            )
        }));
        if !refused.is_empty() {
            let completed = refused;
            actions.push(DriverAction::ReportToolCalls {
                completed: completed.clone(),
            });
            if calls.is_empty() && expansion.plan.is_empty() {
                actions.extend(self.handle_tool_results(ctx, completed));
                return actions;
            }
            let mut parts: Vec<Part> = completed
                .into_iter()
                .map(|outcome| tool_result_part(outcome.call_id, outcome.model_return))
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
        actions.push(DriverAction::Start(PendingWork::Tools {
            calls,
            expansion: expansion.plan,
        }));
        actions
    }

    fn fold_tool_results(
        &self,
        plan: &lash_core::sansio::ToolExpansionPlan,
        completed: Vec<CompletedToolCall>,
    ) -> Vec<CompletedToolCall> {
        batch::fold(plan, completed)
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

            result_parts.push(tool_result_part(outcome.call_id, outcome.model_return));
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

        actions.push(DriverAction::Start(PendingWork::Checkpoint {
            checkpoint: CheckpointKind::AfterWork,
            on_empty: CheckpointResumeAction::PrepareIteration,
        }));
        actions
    }

    // Equivalent mutant: cargo-mutants' `vec![]` replacement is the same value
    // as this body's `Vec::new()`, so no test can tell them apart.
    #[cfg_attr(test, mutants::skip)]
    fn handle_exec_result(
        &self,
        _ctx: DriverContextView<'_>,
        _driver_state: lash_core::ProtocolDriverState,
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
fn tool_result_part(
    call_id: lash_core::ToolCallId,
    model_return: lash_core::facade_support::ModelToolReturn,
) -> Part {
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
    Part::tool_result(String::new(), content, call_id, model_return.tool_name)
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
