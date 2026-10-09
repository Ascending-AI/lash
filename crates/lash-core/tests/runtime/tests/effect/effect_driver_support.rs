use super::*;
use lash_sansio::ReportedFailure;

pub(super) struct EffectControllerTestProtocolFactory {
    pub(super) code_executor: Option<Arc<dyn lash_core::plugin::CodeExecutorPlugin>>,
}

impl lash_core::facade_support::PluginFactory for EffectControllerTestProtocolFactory {
    fn id(&self) -> &'static str {
        "test_protocol"
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::facade_support::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(EffectControllerTestProtocolPlugin {
            code_executor: self.code_executor.clone(),
        }))
    }
}

impl lash_core::plugin::PluginDefinition for EffectControllerTestProtocolFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("test_protocol")
    }
}

struct EffectControllerTestProtocolPlugin {
    code_executor: Option<Arc<dyn lash_core::plugin::CodeExecutorPlugin>>,
}

impl lash_core::facade_support::SessionPlugin for EffectControllerTestProtocolPlugin {
    fn id(&self) -> &'static str {
        "test_protocol"
    }

    fn register(
        &self,
        registrar: &mut lash_core::facade_support::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        registrar
            .protocol()
            .session(Arc::new(EffectControllerTestProtocolSession))?;
        if let Some(code_executor) = self.code_executor.clone() {
            registrar.execution().code_executor(code_executor)?;
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

/// A protocol whose prompt section refuses to render: every
/// execution-environment sync of its turns fails the same way.
pub(super) struct PromptRefusingProtocolFactory;

pub(super) const PROMPT_REFUSAL: &str = "the prompt template names no dialect";

impl lash_core::facade_support::PluginFactory for PromptRefusingProtocolFactory {
    fn id(&self) -> &'static str {
        "test_protocol"
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::facade_support::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(PromptRefusingProtocolPlugin))
    }
}

impl lash_core::plugin::PluginDefinition for PromptRefusingProtocolFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("test_protocol")
    }
}

struct PromptRefusingProtocolPlugin;

impl lash_core::facade_support::SessionPlugin for PromptRefusingProtocolPlugin {
    fn id(&self) -> &'static str {
        "test_protocol"
    }

    fn register(
        &self,
        registrar: &mut lash_core::facade_support::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        registrar
            .protocol()
            .session(Arc::new(EffectControllerTestProtocolSession))?;
        registrar.prompt().section(
            lash_core::plugin::prompt::PromptSectionSpec::new(
                lash_core::prompt_sections::PromptSectionKey::new("intro")
                    .expect("valid section key"),
                lash_core::prompt_sections::PromptPlacement::InitialInstructions,
            ),
            Arc::new(|_: &lash_core::plugin::prompt::PromptInput<'_>| {
                Err(lash_core::plugin::prompt::PromptRenderError::new(
                    PROMPT_REFUSAL,
                ))
            }),
        )?;
        registrar
            .protocol()
            .protocol_driver(Arc::new(EffectControllerTestProtocolDriver))?;
        Ok(())
    }
}

pub(super) struct EffectControllerTestCodeExecutor;

#[async_trait::async_trait]
impl lash_core::plugin::CodeExecutorPlugin for EffectControllerTestCodeExecutor {
    async fn frame_switch_carries(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
        _successor: &lash_core::FrameNodeId,
        _initial_nodes: &[lash_core::SessionAppendNode],
    ) -> Result<Vec<lash_core::ArtifactName>, lash_core::SessionError> {
        Ok(Vec::new())
    }

    async fn execute_code(
        &self,
        _ctx: lash_core::RuntimeExecutionContext<'_>,
        _request: lash_core::ExecRequest,
    ) -> Result<lash_core::ExecResponse, lash_core::SessionError> {
        Ok(lash_core::ExecResponse {
            prints_retained: None,
            prints: vec![lash_core::CellPrint {
                text: "exec output".to_string(),
                value: serde_json::json!("exec output"),
                projection: Default::default(),
            }],
            calls: Vec::new(),
            tool_calls: Vec::new(),
            printed_images: Vec::new(),
            result: lash_core::CellOutcome::Completed,
            retained_finish_value: None,
            degraded_bindings: Vec::new(),
            suspended: false,
        })
    }
}

struct EffectControllerTestProtocolDriver;

impl ProtocolDriverPlugin for EffectControllerTestProtocolDriver {
    fn build_preamble(
        &self,
        input: lash_core::ProtocolBuildInput,
    ) -> lash_core::TurnDriverPreamble {
        lash_core::TurnDriverPreamble {
            config: lash_core::TurnDriverConfig::chat(Arc::new(EffectControllerTestDriver)),
            tool_specs: input.tool_catalog.model_tool_specs(),
            tool_names: input.tool_catalog.tool_names(),
            writer_formats: input.writer_formats,
        }
    }
}

struct EffectControllerTestDriver;

impl lash_sansio::ProtocolDriverHandle<lash_core::HostTurnProtocol> for EffectControllerTestDriver {
    fn prepare_protocol_iteration(
        &self,
        _ctx: lash_core::DriverContextView<'_>,
    ) -> Vec<lash_core::DriverAction> {
        vec![lash_core::DriverAction::Start(
            lash_core::sansio::PendingWork::Exec {
                language: "code".to_string(),
                code: "print('effect controller')".to_string(),
                driver_state: lash_core::ProtocolDriverState::new(
                    "test_protocol",
                    serde_json::Value::Null,
                ),
            },
        )]
    }

    fn handle_llm_success(
        &self,
        _ctx: lash_core::DriverContextView<'_>,
        _request: Arc<lash_core::LlmRequest>,
        _driver_state: Option<lash_core::ProtocolDriverState>,
        _llm_response: LlmResponse,
        _calls: &lash_core::sansio::ResponseToolCalls,
        _text_streamed: bool,
    ) -> Vec<lash_core::DriverAction> {
        Vec::new()
    }

    fn handle_tool_results(
        &self,
        _ctx: lash_core::DriverContextView<'_>,
        _completed: Vec<lash_core::sansio::CompletedToolCall>,
    ) -> Vec<lash_core::DriverAction> {
        Vec::new()
    }

    fn handle_exec_result(
        &self,
        ctx: lash_core::DriverContextView<'_>,
        _driver_state: lash_core::ProtocolDriverState,
        result: Result<lash_core::ExecResponse, lash_core::ExecCodeFailure>,
    ) -> Vec<lash_core::DriverAction> {
        if let Some(evidence) = ctx.observed_cancellation() {
            return vec![lash_core::DriverAction::FinishCancelled {
                evidence: evidence.clone(),
            }];
        }
        match result {
            Ok(response) => vec![lash_core::DriverAction::Finish(TurnOutcome::Finished(
                TurnFinish::FinalValue {
                    value: serde_json::json!(
                        response
                            .prints
                            .iter()
                            .map(|observation| observation.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                },
            ))],
            Err(error) => vec![
                // The failure as the driver received it, so a test can read
                // every field the host handed over.
                lash_core::DriverAction::Emit(
                    lash_core::facade_support::SessionStreamEvent::Error(ReportedFailure {
                        message: serde_json::to_string(&error).expect("an exec failure serializes"),
                        envelope: None,
                    }),
                ),
                lash_core::DriverAction::Finish(TurnOutcome::Stopped(TurnStop::RuntimeError)),
            ],
        }
    }
}
