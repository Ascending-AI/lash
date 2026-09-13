use std::sync::Arc;

use async_trait::async_trait;

use super::*;
use crate::plugin::{
    PluginFactory, PluginRegistrar, PluginSessionContext, ProtocolDriverPlugin,
    ProtocolRuntimeContext, ProtocolSessionContext, ProtocolSessionPlugin, SessionPlugin,
};
use crate::sansio::{CompletedToolCall, ProtocolDriverHandle, WaitingExecState, WaitingLlmState};
use crate::{
    DriverAction, DriverContextView, ExecResponse, ProtocolBuildInput, TurnDriverConfig,
    TurnDriverPreamble,
};
use lash_sansio::llm::types::LlmResponse;

pub fn test_standard_protocol_factories() -> Vec<Arc<dyn PluginFactory>> {
    vec![Arc::new(TestProtocolFactory {
        id: "test_protocol",
        include_batch: true,
        decode_code_create_options: false,
        session_override: None,
        code_executor: None,
    })]
}

#[cfg(test)]
pub(crate) fn test_standard_protocol_factory_with_runtime_state(
    session: Arc<dyn ProtocolSessionPlugin>,
    code_executor: Option<Arc<dyn crate::plugin::CodeExecutorPlugin>>,
) -> Arc<dyn PluginFactory> {
    Arc::new(TestProtocolFactory {
        id: "test_protocol",
        include_batch: true,
        decode_code_create_options: false,
        session_override: Some(session),
        code_executor,
    })
}

pub fn test_code_protocol_factories() -> Vec<Arc<dyn PluginFactory>> {
    vec![Arc::new(TestProtocolFactory {
        id: "protocol_code",
        include_batch: false,
        decode_code_create_options: true,
        session_override: None,
        code_executor: None,
    })]
}

struct TestProtocolFactory {
    id: &'static str,
    include_batch: bool,
    decode_code_create_options: bool,
    session_override: Option<Arc<dyn ProtocolSessionPlugin>>,
    code_executor: Option<Arc<dyn crate::plugin::CodeExecutorPlugin>>,
}

impl PluginFactory for TestProtocolFactory {
    fn id(&self) -> &'static str {
        self.id
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(TestProtocolPlugin {
            id: self.id,
            include_batch: self.include_batch,
            decode_code_create_options: self.decode_code_create_options,
            session_override: self.session_override.clone(),
            code_executor: self.code_executor.clone(),
        }))
    }
}

struct TestProtocolPlugin {
    id: &'static str,
    include_batch: bool,
    decode_code_create_options: bool,
    session_override: Option<Arc<dyn ProtocolSessionPlugin>>,
    code_executor: Option<Arc<dyn crate::plugin::CodeExecutorPlugin>>,
}

impl SessionPlugin for TestProtocolPlugin {
    fn id(&self) -> &'static str {
        self.id
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        reg.protocol()
            .session(self.session_override.clone().unwrap_or_else(|| {
                Arc::new(TestProtocolSession {
                    decode_code_create_options: self.decode_code_create_options,
                })
            }))?;
        if let Some(code_executor) = self.code_executor.as_ref() {
            reg.execution().code_executor(code_executor.clone())?;
        }
        if self.include_batch {
            reg.tools().orchestrating(test_batch_orchestrating_tool())?;
        }
        reg.protocol()
            .protocol_driver(Arc::new(TestProtocolDriver))?;
        Ok(())
    }
}

struct TestProtocolSession {
    decode_code_create_options: bool,
}

#[async_trait]
impl ProtocolSessionPlugin for TestProtocolSession {
    async fn initialize_session(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<(), crate::SessionError> {
        Ok(())
    }

    fn configure_runtime_on_materialize(
        &self,
        mut ctx: ProtocolRuntimeContext<'_>,
        materialization: crate::plugin::ProtocolSessionMaterialization<'_>,
    ) -> Result<(), crate::SessionError> {
        if !self.decode_code_create_options {
            return Ok(());
        }
        if let Some(extras) = materialization
            .plugin_options
            .decode::<TestCodeCreateExtras>("code_protocol")
            .map_err(|err| {
                crate::SessionError::Protocol(format!("invalid test code create options: {err}"))
            })?
        {
            let options = crate::ProtocolTurnOptions::typed(extras)?;
            ctx.set_protocol_turn_options(options);
        }
        Ok(())
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
struct TestCodeCreateExtras {
    termination: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_answer_format: Option<serde_json::Value>,
}

impl Default for TestCodeCreateExtras {
    fn default() -> Self {
        Self {
            termination: default_test_code_termination(),
            final_answer_format: None,
        }
    }
}

fn default_test_code_termination() -> serde_json::Value {
    serde_json::json!({
        "kind": "finish_required",
        "schema": null,
    })
}

/// The test `batch` tool registers in the runtime-owned orchestration lane,
/// exactly like `lash-protocol-standard`'s: nesting tool dispatch is not
/// something a recorded leaf attempt can do.
#[expect(
    unsafe_code,
    reason = "OrchestratingToolDef::from_first_party is lash-core's unsafe capability boundary, and this crate owns the tool contract it registers"
)]
fn test_batch_orchestrating_tool() -> crate::tool_provider::orchestration::OrchestratingToolDef {
    let implementation: Arc<
        dyn crate::tool_provider::orchestration::OrchestratingToolImplementation,
    > = Arc::new(TestProtocolBatchTool);
    // SAFETY: lash-core owns this test-only batch contract and its body.
    unsafe {
        crate::tool_provider::orchestration::OrchestratingToolDef::from_first_party(implementation)
    }
}

struct TestProtocolBatchTool;

#[async_trait]
impl crate::tool_provider::orchestration::OrchestratingToolImplementation
    for TestProtocolBatchTool
{
    fn manifest(&self) -> crate::ToolManifest {
        test_batch_tool_definition().manifest()
    }

    fn contract(&self) -> Arc<crate::ToolContract> {
        Arc::new(test_batch_tool_definition().contract())
    }

    async fn execute(
        &self,
        args: &serde_json::Value,
        context: &crate::tool_provider::orchestration::OrchestrationContext<'_>,
    ) -> crate::ToolOutcome {
        execute_test_batch(context, args).await
    }
}

/// Minimal `batch` tool definition used by lash's own tests. Mirrors the
/// standard protocol plugin's batch schema, but lives here so lash's tests
/// don't need a dev-dep on that plugin crate.
fn test_batch_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:batch",
        "batch",
        "Execute up to 25 independent tool calls concurrently.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "tool_calls": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 25,
                    "items": {
                        "type": "object",
                        "properties": {
                            "tool": { "type": "string" },
                            "parameters": { "type": "object", "additionalProperties": true }
                        },
                        "required": ["tool", "parameters"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["tool_calls"],
            "additionalProperties": false,
        }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

/// Minimal batch executor used by lash's own tests (mirrors the
/// behavior of `lash-protocol-standard`'s `execute_batch_tool_call`).
async fn execute_test_batch(
    context: &crate::tool_provider::orchestration::OrchestrationContext<'_>,
    args: &serde_json::Value,
) -> crate::ToolOutcome {
    const MAX: usize = 25;
    let Some(raw_calls) = args.get("tool_calls").and_then(|v| v.as_array()) else {
        return crate::ToolOutcome::err_fmt("Missing required parameter: tool_calls");
    };
    if raw_calls.is_empty() {
        return crate::ToolOutcome::err_fmt("Invalid tool_calls: expected at least one call");
    }

    let mut results: Vec<lash_sansio::BatchResultRow> = Vec::new();
    let mut parallel_specs = Vec::new();
    for (index, item) in raw_calls.iter().enumerate().take(MAX) {
        let Some(obj) = item.as_object() else {
            return crate::ToolOutcome::err_fmt(format_args!(
                "Invalid tool_calls[{index}]: expected object with tool and parameters"
            ));
        };
        let Some(tool) = obj
            .get("tool")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|t| !t.is_empty())
        else {
            return crate::ToolOutcome::err_fmt(format_args!(
                "Invalid tool_calls[{index}].tool: expected non-empty string"
            ));
        };
        if tool == "batch" {
            results.push(lash_sansio::BatchResultRow::failure(
                index,
                tool,
                0,
                serde_json::json!("Tool 'batch' is not allowed inside batch"),
            ));
            continue;
        }
        let parameters = obj
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let Some(manifest) = context.callable_tool_manifest(tool) else {
            results.push(lash_sansio::BatchResultRow::failure(
                index,
                tool,
                0,
                serde_json::json!(format!("Tool '{tool}' is unavailable in this session")),
            ));
            continue;
        };
        parallel_specs.push((
            index,
            crate::ToolInvocation::new(format!("test-batch:{index}"), manifest.id, parameters),
        ));
    }

    let outcomes = context
        .call_tool_batch(
            parallel_specs
                .iter()
                .map(|(_, invocation)| invocation.clone())
                .collect(),
        )
        .await;
    for ((index, invocation), outcome) in parallel_specs.into_iter().zip(outcomes) {
        let tool_label = invocation.tool_id.to_string();
        let tool_record = outcome.record.unwrap_or(crate::ToolCallRecord {
            call_id: Some(invocation.id),
            tool: tool_label,
            args: invocation.args,
            output: outcome.output,
            duration_ms: 0,
        });
        let value = tool_record.output.value_for_projection();
        results.push(if tool_record.output.is_success() {
            lash_sansio::BatchResultRow::success(
                index,
                tool_record.tool,
                tool_record.duration_ms,
                value,
            )
        } else {
            lash_sansio::BatchResultRow::failure(
                index,
                tool_record.tool,
                tool_record.duration_ms,
                value,
            )
        });
    }

    for overflow_index in MAX..raw_calls.len() {
        results.push(lash_sansio::BatchResultRow::failure(
            overflow_index,
            raw_calls
                .get(overflow_index)
                .and_then(|item| item.get("tool"))
                .and_then(|value| value.as_str())
                .unwrap_or("unknown"),
            0,
            serde_json::json!("Maximum of 25 tool calls allowed in batch"),
        ));
    }

    results.sort_by_key(|row| row.index);
    crate::ToolOutcome::ok(serde_json::json!({ "results": results }))
}

#[test]
fn test_batch_result_row_decode_names_missing_required_field() {
    let error = serde_json::from_value::<lash_sansio::BatchResultRow>(serde_json::json!({
        "index": 0,
        "tool": "probe",
        "success": true,
        "result": "ok"
    }))
    .expect_err("row without duration_ms must fail");

    assert!(
        error.to_string().contains("missing field `duration_ms`"),
        "{error}"
    );
}

struct TestProtocolDriver;

impl ProtocolDriverPlugin for TestProtocolDriver {
    fn build_preamble(&self, input: ProtocolBuildInput) -> TurnDriverPreamble {
        let tool_names = input.tool_catalog.tool_names();
        let tool_names_fingerprint = input.tool_catalog.tool_names_fingerprint();
        TurnDriverPreamble {
            config: TurnDriverConfig::chat(Arc::new(TestDriver), false),
            tool_specs: input.tool_catalog.model_tool_specs(),
            tool_names,
            tool_names_fingerprint,
            execution_prompt: Arc::from(""),
            prompt_contributions: input.extra_prompt_contributions,
        }
    }
}

/// Minimal Standard-style driver used by lash's own test suite. Mirrors
/// the parts of the real `lash-protocol-standard::StandardDriver` that
/// production tests depend on: extract tool calls + assistant text from
/// the LLM response, append the assistant message, dispatch tools, and
/// finish-checkpoint when there are no tools. Reasoning parts are
/// surfaced but without the interleave ordering the real driver uses —
/// no test asserts that ordering.
struct TestDriver;

impl ProtocolDriverHandle<crate::HostTurnProtocol> for TestDriver {
    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        vec![DriverAction::StartLlm {
            request: ctx.project_llm_request(true),
            driver_state: None,
        }]
    }

    fn handle_llm_success(
        &self,
        ctx: DriverContextView<'_>,
        _waiting: WaitingLlmState<crate::HostTurnProtocol>,
        llm_response: LlmResponse,
        text_streamed: bool,
    ) -> Vec<DriverAction> {
        use crate::sansio::{CheckpointResumeAction, PendingToolCall};
        use crate::{CheckpointKind, Message, MessageRole, Part, SessionStreamEvent};
        use lash_sansio::llm::types::LlmOutputPart;
        use lash_sansio::session_model::make_error_event;

        let parts = crate::normalized_response_parts(&llm_response);
        let mut assistant_text = String::new();
        let mut tool_calls: Vec<(
            String,
            String,
            String,
            Option<lash_sansio::llm::types::ProviderReplayMeta>,
        )> = Vec::new();
        let mut actions = Vec::new();

        for part in parts {
            match part {
                LlmOutputPart::Text { text, .. } => {
                    if !text.is_empty() {
                        let previous_len = assistant_text.len();
                        crate::append_assistant_text_part(&mut assistant_text, &text);
                        if !text_streamed {
                            actions.push(DriverAction::Emit(SessionStreamEvent::TextDelta {
                                content: assistant_text[previous_len..].to_string(),
                            }));
                        }
                    }
                }
                LlmOutputPart::Reasoning { .. } => {}
                LlmOutputPart::ToolCall {
                    call_id,
                    tool_name,
                    input_json,
                    replay,
                } => {
                    tool_calls.push((call_id, tool_name, input_json, replay));
                }
            }
        }

        actions.push(DriverAction::Emit(SessionStreamEvent::LlmResponse {
            protocol_iteration: ctx.protocol_iteration(),
            content: assistant_text.clone(),
            duration_ms: 0,
        }));

        if tool_calls.is_empty() {
            if assistant_text.trim().is_empty() {
                actions.push(DriverAction::Emit(make_error_event(
                    "llm_provider",
                    Some("empty_response"),
                    "Model returned no assistant text or tool calls.",
                    None,
                )));
                actions.push(DriverAction::Finish(TurnOutcome::Stopped(
                    TurnStop::ProviderError,
                )));
                return actions;
            }
            let asst_id = format!(
                "m_standard_{}_{}_assistant",
                ctx.turn_id(),
                ctx.protocol_iteration()
            );
            let outcome_text = assistant_text.clone();
            let parts_out = vec![Part::prose(format!("{asst_id}.p0"), assistant_text, None)];
            actions.push(DriverAction::AppendEvents(vec![
                SessionHistoryRecord::Conversation(ConversationRecord::from_message(Message {
                    id: asst_id,
                    role: MessageRole::Assistant,
                    parts: lash_sansio::shared_parts(parts_out),
                    origin: None,
                })),
            ]));
            actions.push(DriverAction::StartCheckpoint {
                checkpoint: CheckpointKind::BeforeCompletion,
                on_empty: CheckpointResumeAction::Finish(TurnOutcome::Finished(
                    TurnFinish::AssistantMessage { text: outcome_text },
                )),
            });
            return actions;
        }

        let asst_id = format!(
            "m_standard_{}_{}_assistant",
            ctx.turn_id(),
            ctx.protocol_iteration()
        );
        let mut assistant_parts = Vec::new();
        if !assistant_text.trim().is_empty() {
            assistant_parts.push(Part::prose(
                format!("{}.p{}", asst_id, assistant_parts.len()),
                assistant_text,
                None,
            ));
        }
        let mut calls = Vec::new();
        for (call_id, tool_name, input_json, replay) in tool_calls {
            assistant_parts.push(Part::tool_call(
                format!("{}.p{}", asst_id, assistant_parts.len()),
                input_json.clone(),
                call_id.clone(),
                tool_name.clone(),
                replay.clone(),
            ));
            let args = serde_json::from_str::<serde_json::Value>(&input_json)
                .unwrap_or_else(|_| serde_json::json!({}));
            calls.push(PendingToolCall {
                call_id,
                tool_name,
                args,
                replay,
            });
        }
        if !assistant_parts.is_empty() {
            actions.push(DriverAction::AppendEvents(vec![
                SessionHistoryRecord::Conversation(ConversationRecord::from_message(Message {
                    id: asst_id,
                    role: MessageRole::Assistant,
                    parts: lash_sansio::shared_parts(assistant_parts),
                    origin: None,
                })),
            ]));
        }
        actions.push(DriverAction::StartTools { calls });
        actions
    }

    fn handle_tool_results(
        &self,
        ctx: DriverContextView<'_>,
        completed: Vec<CompletedToolCall>,
    ) -> Vec<DriverAction> {
        use crate::sansio::CheckpointResumeAction;
        use crate::{CheckpointKind, Message, MessageRole, Part, SessionStreamEvent};
        use lash_sansio::session_model::reassign_part_ids;
        let mut actions = Vec::new();
        let mut result_parts = Vec::new();
        let mut terminal_outcome = None;
        for outcome in completed {
            if terminal_outcome.is_none() && outcome.output.is_success() {
                terminal_outcome = match outcome.output.control.as_ref() {
                    Some(crate::ToolControl::SwitchAgentFrame {
                        frame_key,
                        initial_nodes,
                        task: Some(task),
                    }) if !task.trim().is_empty() => Some(TurnOutcome::AgentFrameSwitch {
                        frame_key: frame_key.clone(),
                        task: task.clone(),
                        initial_nodes: initial_nodes.clone(),
                    }),
                    Some(crate::ToolControl::Finish { value }) => {
                        Some(TurnOutcome::Finished(TurnFinish::ToolValue {
                            tool_name: outcome.tool_name.clone(),
                            value: crate::tool_value_for_projection(value),
                        }))
                    }
                    Some(crate::ToolControl::Fail { failure }) => {
                        Some(TurnOutcome::Stopped(TurnStop::ToolError {
                            tool_name: outcome.tool_name.clone(),
                            value: crate::tool_failure_for_projection(failure),
                        }))
                    }
                    _ => None,
                };
            }
            for part in &outcome.model_return.parts {
                match part {
                    lash_sansio::ModelToolReturnPart::Text { text } => {
                        if text.is_empty() {
                            continue;
                        }
                        result_parts.push(Part::tool_result(
                            String::new(),
                            text.clone(),
                            outcome.call_id.clone(),
                            outcome.tool_name.clone(),
                        ));
                    }
                    lash_sansio::ModelToolReturnPart::Attachment(source) => {
                        result_parts.push(Part::tool_result_attachment(
                            String::new(),
                            String::new(),
                            lash_sansio::PartAttachment {
                                source: source.clone(),
                            },
                            outcome.call_id.clone(),
                            outcome.tool_name.clone(),
                        ));
                    }
                }
            }
        }
        if !result_parts.is_empty() {
            let user_id = format!(
                "m_standard_{}_{}_tool_results",
                ctx.turn_id(),
                ctx.protocol_iteration()
            );
            reassign_part_ids(&user_id, &mut result_parts);
            actions.push(DriverAction::AppendEvents(vec![
                SessionHistoryRecord::Conversation(ConversationRecord::from_message(Message {
                    id: user_id,
                    role: MessageRole::User,
                    parts: lash_sansio::shared_parts(result_parts),
                    origin: None,
                })),
            ]));
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
            let _ = SessionStreamEvent::Done;
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
        _waiting: WaitingExecState<crate::HostTurnProtocol>,
        _result: Result<ExecResponse, String>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }
}
