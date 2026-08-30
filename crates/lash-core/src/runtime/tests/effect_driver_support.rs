struct EffectControllerTestProtocolFactory {
    code_executor: Option<Arc<dyn crate::plugin::CodeExecutorPlugin>>,
}

impl crate::PluginFactory for EffectControllerTestProtocolFactory {
    fn id(&self) -> &'static str {
        "test_protocol"
    }

    fn build(
        &self,
        _ctx: &crate::PluginSessionContext,
    ) -> Result<Arc<dyn crate::SessionPlugin>, crate::PluginError> {
        Ok(Arc::new(EffectControllerTestProtocolPlugin {
            code_executor: self.code_executor.clone(),
        }))
    }
}

struct EffectControllerTestProtocolPlugin {
    code_executor: Option<Arc<dyn crate::plugin::CodeExecutorPlugin>>,
}

impl crate::SessionPlugin for EffectControllerTestProtocolPlugin {
    fn id(&self) -> &'static str {
        "effect_controller_test_protocol"
    }

    fn register(&self, registrar: &mut crate::PluginRegistrar) -> Result<(), crate::PluginError> {
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

struct EffectControllerTestCodeExecutor;

#[async_trait::async_trait]
impl crate::plugin::CodeExecutorPlugin for EffectControllerTestCodeExecutor {
    async fn execute_code(
        &self,
        _ctx: crate::RuntimeExecutionContext<'_>,
        _request: crate::ExecRequest,
    ) -> Result<crate::ExecResponse, crate::SessionError> {
        Ok(crate::ExecResponse {
            observations: vec![crate::Observation {
                text: "exec output".to_string(),
                projection: Default::default(),
            }],
            tool_calls: Vec::new(),
            executed_calls: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            duration_ms: 1,
            degraded_bindings: Vec::new(),
            terminal_finish: None,
        })
    }
}

struct EffectControllerTestProtocolDriver;

impl ProtocolDriverPlugin for EffectControllerTestProtocolDriver {
    fn build_preamble(&self, input: crate::ProtocolBuildInput) -> crate::TurnDriverPreamble {
        crate::TurnDriverPreamble {
            config: crate::TurnDriverConfig::chat(
                Arc::new(EffectControllerTestDriver),
                true,
                Arc::new(effect_controller_turn_limit_final_message),
            ),
            tool_specs: input.tool_catalog.model_tool_specs(),
            tool_names: input.tool_catalog.tool_names(),
            tool_names_fingerprint: input.tool_catalog.tool_names_fingerprint(),
            execution_prompt: Arc::from(""),
            prompt_contributions: input.extra_prompt_contributions,
        }
    }
}

fn effect_controller_turn_limit_final_message(
    message_id: String,
    max_turns: usize,
) -> crate::Message {
    crate::Message {
        id: message_id.clone(),
        role: crate::MessageRole::System,
        parts: crate::shared_parts(vec![crate::Part::error(
            format!("{message_id}.p0"),
            format!("Turn limit reached ({max_turns}) before a final test response."),
        )]),
        origin: None,
    }
}

struct EffectControllerTestDriver;

impl lash_sansio::ProtocolDriverHandle<crate::HostTurnProtocol> for EffectControllerTestDriver {
    fn prepare_protocol_iteration(
        &self,
        _ctx: crate::DriverContextView<'_>,
    ) -> Vec<crate::DriverAction> {
        vec![crate::DriverAction::StartExec {
            language: "code".to_string(),
            code: "print('effect controller')".to_string(),
            driver_state: crate::ProtocolDriverState::new(
                "effect_controller_test_protocol",
                serde_json::Value::Null,
            ),
        }]
    }

    fn handle_llm_success(
        &self,
        _ctx: crate::DriverContextView<'_>,
        _waiting: lash_sansio::WaitingLlmState<crate::HostTurnProtocol>,
        _llm_response: LlmResponse,
        _text_streamed: bool,
    ) -> Vec<crate::DriverAction> {
        Vec::new()
    }

    fn handle_tool_results(
        &self,
        _ctx: crate::DriverContextView<'_>,
        _completed: Vec<crate::sansio::CompletedToolCall>,
    ) -> Vec<crate::DriverAction> {
        Vec::new()
    }

    fn handle_exec_result(
        &self,
        ctx: crate::DriverContextView<'_>,
        _waiting: lash_sansio::WaitingExecState<crate::HostTurnProtocol>,
        result: Result<crate::ExecResponse, String>,
    ) -> Vec<crate::DriverAction> {
        if let Some(evidence) = ctx.observed_cancellation() {
            return vec![crate::DriverAction::FinishCancelled {
                evidence: evidence.clone(),
            }];
        }
        match result {
            Ok(response) => vec![crate::DriverAction::Finish(TurnOutcome::Finished(
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
                crate::DriverAction::Emit(crate::SessionStreamEvent::Error {
                    message: error,
                    envelope: None,
                }),
                crate::DriverAction::Finish(TurnOutcome::Stopped(TurnStop::RuntimeError)),
            ],
        }
    }
}
