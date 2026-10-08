//! Minimal protocol-plugin fakes shared by Lash tests.

use crate::llm::types::{StreamBlockEvent, StreamBlockKind};
use std::sync::Arc;

use async_trait::async_trait;

use super::*;
use crate::plugin::{
    CandidateFacts, ConfigOwner, ConfigRegistrar, ConfigRegistrationError, PluginFactory,
    PluginRegistrar, PluginSessionContext, ProtocolDriverPlugin, ProtocolSessionContext,
    ProtocolSessionPlugin, SessionPlugin,
};
use crate::sansio::{CompletedToolCall, PendingWork, ProtocolDriverHandle};
use crate::{
    DriverAction, DriverContextView, ExecResponse, ProtocolBuildInput, TurnDriverConfig,
    TurnDriverPreamble,
};
use lash_sansio::llm::types::LlmResponse;

pub fn test_standard_protocol_factories() -> Vec<Arc<dyn PluginFactory>> {
    vec![Arc::new(TestProtocolFactory {
        id: "test_protocol",
        decode_code_create_options: false,
        session_override: None,
        code_executor: None,
        driver: TestDriverKind::Standard,
    })]
}

/// The standard fake protocol, except that its driver answers the model's
/// response with no next step: the machine stalls, so the turn's stream ends
/// with neither an outcome nor `Done`, and its terminal is the host's
/// missing-`Done` fallback.
pub fn test_protocol_factories_ending_without_done() -> Vec<Arc<dyn PluginFactory>> {
    vec![Arc::new(TestProtocolFactory {
        id: "test_protocol",
        decode_code_create_options: false,
        session_override: None,
        code_executor: None,
        driver: TestDriverKind::EndsWithoutDone,
    })]
}

#[cfg(any(test, feature = "testing"))]
pub fn test_standard_protocol_factory_with_runtime_state(
    session: Arc<dyn ProtocolSessionPlugin>,
    code_executor: Option<Arc<dyn crate::plugin::CodeExecutorPlugin>>,
) -> Arc<dyn PluginFactory> {
    Arc::new(TestProtocolFactory {
        id: "test_protocol",
        decode_code_create_options: false,
        session_override: Some(session),
        code_executor,
        driver: TestDriverKind::Standard,
    })
}

/// Widening that builtin to the whole `testing` feature would push a second protocol session
/// onto every crate that turns the feature on, so the injection lives here, applying the same
/// id-override rule `PluginHost::new` uses.
pub fn test_plugin_host(factories: Vec<Arc<dyn PluginFactory>>) -> crate::PluginHost {
    let override_ids: std::collections::BTreeSet<&'static str> =
        factories.iter().map(|factory| factory.id()).collect();
    let mut all = test_standard_protocol_factories();
    all.retain(|factory| !override_ids.contains(factory.id()));
    all.extend(factories);
    crate::PluginHost::new(all)
}

pub fn test_code_protocol_factories() -> Vec<Arc<dyn PluginFactory>> {
    vec![Arc::new(TestProtocolFactory {
        id: "protocol_code",
        decode_code_create_options: true,
        session_override: None,
        code_executor: None,
        driver: TestDriverKind::Standard,
    })]
}

/// Which driver a fake protocol installs.
#[derive(Clone, Copy)]
enum TestDriverKind {
    /// [`TestDriver`].
    Standard,
    /// [`EndsWithoutDoneDriver`].
    EndsWithoutDone,
}

struct TestProtocolFactory {
    id: &'static str,
    decode_code_create_options: bool,
    session_override: Option<Arc<dyn ProtocolSessionPlugin>>,
    code_executor: Option<Arc<dyn crate::plugin::CodeExecutorPlugin>>,
    driver: TestDriverKind,
}

impl PluginFactory for TestProtocolFactory {
    fn id(&self) -> &'static str {
        self.id
    }

    /// The code protocol records the create extras a creator states; the
    /// standard fake registers no config.
    fn register_config(&self, reg: &mut ConfigRegistrar) -> Result<(), ConfigRegistrationError> {
        if self.decode_code_create_options {
            reg.owner(TestCodeConfigOwner)?;
        }
        Ok(())
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(TestProtocolPlugin {
            id: self.id,
            session_override: self.session_override.clone(),
            code_executor: self.code_executor.clone(),
            driver: self.driver,
        }))
    }
}

impl crate::plugin::PluginMetadata for TestProtocolFactory {
    fn plugin_declaration(&self) -> crate::plugin::PluginDeclaration {
        crate::plugin::PluginDeclaration::initial(self.id)
    }
}

struct TestProtocolPlugin {
    id: &'static str,
    session_override: Option<Arc<dyn ProtocolSessionPlugin>>,
    code_executor: Option<Arc<dyn crate::plugin::CodeExecutorPlugin>>,
    driver: TestDriverKind,
}

impl SessionPlugin for TestProtocolPlugin {
    fn id(&self) -> &'static str {
        self.id
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        reg.protocol().session(
            self.session_override
                .clone()
                .unwrap_or_else(|| Arc::new(TestProtocolSession)),
        )?;
        if let Some(code_executor) = self.code_executor.as_ref() {
            reg.execution().code_executor(code_executor.clone())?;
        }
        reg.protocol()
            .protocol_driver(Arc::new(TestProtocolDriver {
                driver: self.driver,
            }))?;
        Ok(())
    }
}

struct TestProtocolSession;

#[async_trait]
impl ProtocolSessionPlugin for TestProtocolSession {
    async fn initialize_session(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<(), crate::SessionError> {
        Ok(())
    }
}

/// The code protocol fake's config owner: it records the stated create
/// extras, and nothing when nothing is stated.
struct TestCodeConfigOwner;

/// The code protocol fake refuses nothing it can decode.
#[derive(Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
struct TestCodeConfigRefusal {
    message: String,
}

impl std::fmt::Display for TestCodeConfigRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl ConfigOwner for TestCodeConfigOwner {
    type Create = TestCodeCreateExtras;
    type Recorded = TestCodeCreateExtras;
    type Refusal = TestCodeConfigRefusal;
    type RunOptions = TestCodeCreateExtras;

    fn create(
        &self,
        input: Option<TestCodeCreateExtras>,
    ) -> Result<Option<TestCodeCreateExtras>, TestCodeConfigRefusal> {
        Ok(input)
    }

    fn validate(
        &self,
        _value: &TestCodeCreateExtras,
        _base: Option<&TestCodeCreateExtras>,
        _facts: &CandidateFacts<'_>,
    ) -> Result<(), TestCodeConfigRefusal> {
        Ok(())
    }

    /// A run replaces the extras whole.
    fn apply_run_options(
        &self,
        _recorded: &TestCodeCreateExtras,
        options: TestCodeCreateExtras,
    ) -> Result<TestCodeCreateExtras, TestCodeConfigRefusal> {
        Ok(options)
    }
}

#[derive(Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
struct TestCodeCreateExtras {
    #[schemars(with = "serde_json::Value")]
    termination: serde_json::Value,
}

impl Default for TestCodeCreateExtras {
    fn default() -> Self {
        Self {
            termination: default_test_code_termination(),
        }
    }
}

fn default_test_code_termination() -> serde_json::Value {
    serde_json::json!({
        "kind": "finish_required",
        "schema": null,
    })
}

struct TestProtocolDriver {
    driver: TestDriverKind,
}

impl ProtocolDriverPlugin for TestProtocolDriver {
    fn build_preamble(&self, input: ProtocolBuildInput) -> TurnDriverPreamble {
        let tool_names = input.tool_catalog.tool_names();
        let driver: Arc<dyn ProtocolDriverHandle<crate::HostTurnProtocol>> = match self.driver {
            TestDriverKind::Standard => Arc::new(TestDriver),
            TestDriverKind::EndsWithoutDone => Arc::new(EndsWithoutDoneDriver),
        };
        TurnDriverPreamble {
            config: TurnDriverConfig::chat(driver),
            tool_specs: input.tool_catalog.model_tool_specs(),
            tool_names,
            writer_formats: input.writer_formats,
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
        vec![DriverAction::Start(PendingWork::Llm {
            request: ctx
                .project_llm_request(true)
                .expect("fixture history projects"),
            driver_state: None,
        })]
    }

    fn handle_llm_success(
        &self,
        ctx: DriverContextView<'_>,
        _request: Arc<lash_sansio::llm::types::LlmRequest>,
        _driver_state: Option<crate::ProtocolDriverState>,
        llm_response: LlmResponse,
        calls: &crate::sansio::ResponseToolCalls,
        text_streamed: bool,
    ) -> Vec<DriverAction> {
        use crate::sansio::{CheckpointResumeAction, PendingToolCall};
        use crate::{CheckpointKind, Message, MessageRole, Part, SessionStreamEvent};
        use lash_sansio::llm::types::LlmOutputPart;
        use lash_sansio::session_model::make_error_event;

        let parts = crate::normalized_response_parts(&llm_response);
        let mut call_ids = calls.call_ids(&llm_response).into_iter();
        let mut assistant_text = String::new();
        let mut tool_calls: Vec<(
            crate::ToolCallId,
            String,
            String,
            String,
            Option<lash_sansio::llm::types::ProviderReplayMeta>,
        )> = Vec::new();
        let mut actions = Vec::new();
        let mut next_block_ordinal = 0u64;

        for (part_index, part) in parts.into_iter().enumerate() {
            match part {
                LlmOutputPart::Text {
                    text,
                    response_meta,
                } => {
                    if !text.is_empty() {
                        let previous_len = assistant_text.len();
                        crate::append_assistant_text_part(&mut assistant_text, &text);
                        let text = assistant_text[previous_len..].to_string();
                        if !text_streamed {
                            // Mirror StandardDriver: buffered completions emit
                            // the same Started/Delta/Completed lifecycle the
                            // streaming lane produces, keyed per text part.
                            let item_id = response_meta.as_ref().and_then(|meta| meta.id.clone());
                            let block = lash_sansio::llm::types::StreamBlockIdentity {
                                id: item_id
                                    .clone()
                                    .unwrap_or_else(|| format!("part:{part_index}")),
                                ordinal: next_block_ordinal,
                                item_id,
                            };
                            next_block_ordinal += 1;
                            actions.push(DriverAction::Emit(SessionStreamEvent::StreamBlock(
                                StreamBlockEvent::Started {
                                    kind: lash_sansio::llm::types::StreamBlockKind::AssistantText,
                                    block: block.clone(),
                                },
                            )));
                            actions.push(DriverAction::Emit(SessionStreamEvent::StreamBlock(
                                StreamBlockEvent::Delta {
                                    kind: StreamBlockKind::AssistantText,
                                    text: text.clone(),
                                    block: block.clone(),
                                },
                            )));
                            actions.push(DriverAction::Emit(SessionStreamEvent::StreamBlock(
                                StreamBlockEvent::Completed {
                                    kind: lash_sansio::llm::types::StreamBlockKind::AssistantText,
                                    block,
                                    text: text.clone(),
                                },
                            )));
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
                    let Some(id) = call_ids.next() else {
                        continue;
                    };
                    tool_calls.push((id, call_id, tool_name, input_json, replay));
                }
            }
        }

        actions.push(DriverAction::Emit(SessionStreamEvent::LlmResponse {
            protocol_iteration: ctx.protocol_iteration(),
            content: assistant_text.clone(),
        }));

        if tool_calls.is_empty() {
            if assistant_text.trim().is_empty() {
                actions.push(DriverAction::Emit(make_error_event(
                    lash_sansio::session_model::TurnFailureKind::LlmProvider,
                    Some(lash_sansio::session_model::TurnFailureCode::EmptyResponse.into()),
                    "Model returned no assistant text or tool calls.",
                    None,
                    lash_sansio::session_model::RuntimeOutputCuts::standard(),
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
                    reply_marker: None,
                })),
            ]));
            actions.push(DriverAction::Start(PendingWork::Checkpoint {
                checkpoint: CheckpointKind::BeforeCompletion,
                on_empty: CheckpointResumeAction::Finish(TurnOutcome::Finished(
                    TurnFinish::AssistantMessage { text: outcome_text },
                )),
            }));
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
        let mut pending_calls = Vec::new();
        for (call_id, provider_call_id, tool_name, input_json, replay) in tool_calls {
            assistant_parts.push(Part::tool_call(
                format!("{}.p{}", asst_id, assistant_parts.len()),
                input_json.clone(),
                call_id.clone(),
                provider_call_id.clone(),
                tool_name.clone(),
                replay.clone(),
            ));
            let args = serde_json::from_str::<serde_json::Value>(&input_json)
                .unwrap_or_else(|_| serde_json::json!({}));
            pending_calls.push(PendingToolCall {
                call_id,
                provider_call_id: Some(provider_call_id),
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
                    reply_marker: None,
                })),
            ]));
        }
        actions.push(DriverAction::Start(PendingWork::WaitingForToolResults {
            settled: None,
            calls: pending_calls,
            expansion: Default::default(),
        }));
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
            result_parts.push(Part::tool_result(
                String::new(),
                outcome
                    .model_return
                    .parts
                    .iter()
                    .filter(|block| {
                        !matches!(block, lash_sansio::ModelToolReturnPart::Text { text } if text.is_empty())
                    })
                    .cloned()
                    .collect(),
                outcome.call_id.clone(),
                outcome.tool_name.clone(),
            ));
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
                    reply_marker: None,
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
        actions.push(DriverAction::Start(PendingWork::Checkpoint {
            checkpoint: CheckpointKind::AfterWork,
            on_empty: CheckpointResumeAction::PrepareIteration,
        }));
        actions
    }

    fn handle_exec_result(
        &self,
        _ctx: DriverContextView<'_>,
        _driver_state: crate::ProtocolDriverState,
        _result: Result<ExecResponse, crate::ExecCodeFailure>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }
}

/// A driver that starts one model call and answers its response with only
/// the response's report: no next step and no finish. The machine then has
/// nothing to poll, so the turn's stream ends without an outcome or `Done`.
struct EndsWithoutDoneDriver;

impl ProtocolDriverHandle<crate::HostTurnProtocol> for EndsWithoutDoneDriver {
    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        vec![DriverAction::Start(PendingWork::Llm {
            request: ctx
                .project_llm_request(true)
                .expect("fixture history projects"),
            driver_state: None,
        })]
    }

    fn handle_llm_success(
        &self,
        ctx: DriverContextView<'_>,
        _request: Arc<lash_sansio::llm::types::LlmRequest>,
        _driver_state: Option<crate::ProtocolDriverState>,
        llm_response: LlmResponse,
        _calls: &crate::sansio::ResponseToolCalls,
        _text_streamed: bool,
    ) -> Vec<DriverAction> {
        vec![DriverAction::Emit(crate::SessionStreamEvent::LlmResponse {
            protocol_iteration: ctx.protocol_iteration(),
            content: llm_response.full_text(),
        })]
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
        _driver_state: crate::ProtocolDriverState,
        _result: Result<ExecResponse, crate::ExecCodeFailure>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }
}
