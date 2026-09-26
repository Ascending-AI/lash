use super::*;

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

struct EffectControllerTestProtocolPlugin {
    code_executor: Option<Arc<dyn lash_core::plugin::CodeExecutorPlugin>>,
}

impl lash_core::facade_support::SessionPlugin for EffectControllerTestProtocolPlugin {
    fn id(&self) -> &'static str {
        "effect_controller_test_protocol"
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

pub(super) struct EffectControllerTestCodeExecutor;

#[async_trait::async_trait]
impl lash_core::plugin::CodeExecutorPlugin for EffectControllerTestCodeExecutor {
    async fn execute_code(
        &self,
        _ctx: lash_core::RuntimeExecutionContext<'_>,
        _request: lash_core::ExecRequest,
    ) -> Result<lash_core::ExecResponse, lash_core::SessionError> {
        Ok(lash_core::ExecResponse {
            observations: vec![lash_core::Observation {
                text: "exec output".to_string(),
                projection: Default::default(),
            }],
            calls: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            degraded_bindings: Vec::new(),
            terminal_finish: None,
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
            config: lash_core::TurnDriverConfig::chat(Arc::new(EffectControllerTestDriver), true),
            tool_specs: input.tool_catalog.model_tool_specs(),
            tool_names: input.tool_catalog.tool_names(),
            tool_names_fingerprint: input.tool_catalog.tool_names_fingerprint(),
            execution_prompt: Arc::from(""),
            prompt_contributions: input.extra_prompt_contributions,
        }
    }
}

struct EffectControllerTestDriver;

impl lash_sansio::ProtocolDriverHandle<lash_core::HostTurnProtocol> for EffectControllerTestDriver {
    fn prepare_protocol_iteration(
        &self,
        _ctx: lash_core::DriverContextView<'_>,
    ) -> Vec<lash_core::DriverAction> {
        vec![lash_core::DriverAction::StartExec {
            language: "code".to_string(),
            code: "print('effect controller')".to_string(),
            driver_state: lash_core::ProtocolDriverState::new(
                "effect_controller_test_protocol",
                serde_json::Value::Null,
            ),
        }]
    }

    fn handle_llm_success(
        &self,
        _ctx: lash_core::DriverContextView<'_>,
        _waiting: lash_sansio::WaitingLlmState<lash_core::HostTurnProtocol>,
        _llm_response: LlmResponse,
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
        _waiting: lash_sansio::WaitingExecState<lash_core::HostTurnProtocol>,
        result: Result<lash_core::ExecResponse, String>,
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
                            .observations
                            .iter()
                            .map(|observation| observation.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                },
            ))],
            Err(error) => vec![
                lash_core::DriverAction::Emit(
                    lash_core::facade_support::SessionStreamEvent::Error {
                        message: error,
                        envelope: None,
                    },
                ),
                lash_core::DriverAction::Finish(TurnOutcome::Stopped(TurnStop::RuntimeError)),
            ],
        }
    }
}
