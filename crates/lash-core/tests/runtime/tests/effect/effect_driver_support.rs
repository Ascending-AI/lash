use super::*;
use lash_core::plugin::PluginRecipe;
use lash_protocol_types::ReportedFailure;

pub(super) struct EffectControllerTestProtocolFactory {
    pub(super) code_executor: Option<Arc<dyn lash_core::plugin::CodeExecutorPlugin>>,
}

impl lash_core::facade_support::PluginFactory for EffectControllerTestProtocolFactory {
    fn id(&self) -> &'static str {
        "test_protocol"
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<
        lash_core::plugin::PluginRecipe,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let mut reg = lash_core::plugin::PluginRegistrar::<EffectControllerTestProtocolPlugin>::new(
            lash_core::plugin::PluginFactory::id(self.as_ref()),
        );
        EffectControllerTestProtocolPlugin::declare(&mut reg)?;
        if self.code_executor.is_some() {
            reg.execution().code_executor(move |this, _state| {
                this.code_executor.clone().ok_or_else(|| {
                    lash_execution_contracts::plugin_error::PluginError::Registration(
                        "declared executor is unavailable".into(),
                    )
                })
            })?;
        }

        let id = self.id();
        Ok(lash_core::plugin::PluginRecipe::new(
            id,
            reg,
            move |_ctx, _state| {
                Ok(Arc::new(EffectControllerTestProtocolPlugin {
                    code_executor: self.code_executor.clone(),
                }))
            },
        ))
    }
}

impl EffectControllerTestProtocolFactory {
    pub fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("test_protocol")
    }
}
impl lash_core::plugin::PluginMetadata for EffectControllerTestProtocolFactory {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        Self::declaration()
    }
}

struct EffectControllerTestProtocolPlugin {
    code_executor: Option<Arc<dyn lash_core::plugin::CodeExecutorPlugin>>,
}

impl lash_core::facade_support::SessionPlugin for EffectControllerTestProtocolPlugin {
    fn id(&self) -> &'static str {
        "test_protocol"
    }
}
impl EffectControllerTestProtocolPlugin {
    fn declare(
        reg: &mut lash_core::plugin::PluginRegistrar<Self>,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        reg.protocol()
            .session(move |_this, _state| Ok(Arc::new(EffectControllerTestProtocolSession)))?;

        reg.protocol().protocol_driver(move |_this, _state| {
            Ok(Arc::new(EffectControllerTestProtocolDriver))
        })?;
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

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<
        lash_core::plugin::PluginRecipe,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let mut reg = lash_core::plugin::PluginRegistrar::<PromptRefusingProtocolPlugin>::new(
            lash_core::plugin::PluginFactory::id(self.as_ref()),
        );
        PromptRefusingProtocolPlugin::declare(&mut reg)?;
        let id = self.id();
        Ok(lash_core::plugin::PluginRecipe::new(
            id,
            reg,
            move |_ctx, _state| Ok(Arc::new(PromptRefusingProtocolPlugin)),
        ))
    }
}

impl PromptRefusingProtocolFactory {
    pub fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("test_protocol")
    }
}
impl lash_core::plugin::PluginMetadata for PromptRefusingProtocolFactory {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        Self::declaration()
    }
}

struct PromptRefusingProtocolPlugin;

impl lash_core::facade_support::SessionPlugin for PromptRefusingProtocolPlugin {
    fn id(&self) -> &'static str {
        "test_protocol"
    }
}
impl PromptRefusingProtocolPlugin {
    fn declare(
        reg: &mut lash_core::plugin::PluginRegistrar<Self>,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        reg.protocol()
            .session(move |_this, _state| Ok(Arc::new(EffectControllerTestProtocolSession)))?;

        reg.prompt().section(
            lash_core::plugin::prompt::PromptSectionSpec::new(
                lash_core::prompt_sections::PromptSectionKey::new("intro")
                    .expect("valid section key"),
                lash_core::prompt_sections::PromptPlacement::InitialInstructions,
            ),
            move |_this, _state| {
                Ok(Arc::new(
                    |_: &lash_core::plugin::prompt::PromptInput<'_>| {
                        Err(lash_core::plugin::prompt::PromptRenderError::new(
                            PROMPT_REFUSAL,
                        ))
                    },
                ))
            },
        )?;

        reg.protocol().protocol_driver(move |_this, _state| {
            Ok(Arc::new(EffectControllerTestProtocolDriver))
        })?;
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
            consumption: Vec::new(),
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
            bindings: Default::default(),
            suspended: false,
        })
    }
}

struct EffectControllerTestProtocolDriver;

impl ProtocolDriverPlugin for EffectControllerTestProtocolDriver {
    fn attachment_omission_text(
        &self,
        notice: &lash_protocol_types::AttachmentMaterializationNotice,
    ) -> String {
        format!("({} omitted)", notice.media_type)
    }

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
                TurnFinish::Finished {
                    tool_name: "finish".to_string(),
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
                lash_core::DriverAction::Finish(TurnOutcome::Stopped(TurnStop::RuntimeError {
                    cause: lash_protocol_types::TurnErrorCause::new(
                        lash_protocol_types::TurnErrorCode::RuntimeRefused,
                    ),
                })),
            ],
        }
    }
}
