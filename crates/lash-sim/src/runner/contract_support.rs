use super::*;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use lash_sansio::sync::MutexExt;

/// One registered fixed contract execution: the semantic oracle id it proves,
/// the spec row it mirrors, and the executor that reproduces its result. Registration is a const slice per
/// suite, so replaying one execution is a lookup plus one call.
pub(super) struct FixedContractRow<E> {
    pub(super) semantic_oracle: &'static str,
    pub(super) source_path: &'static str,
    pub(super) source_scenario: &'static str,
    pub(super) execute: E,
}

pub(super) type TurnMachineContractExecutor = fn() -> Result<Value, FixedScriptRunnerError>;

pub(crate) fn replay_contract_execution(contract: &str) -> Result<Value, FixedScriptRunnerError> {
    if let Some(row) = STANDARD_CONTRACT_ROWS
        .iter()
        .chain(RLM_CONTRACT_ROWS)
        .find(|row| row.semantic_oracle == contract)
    {
        return contract_execution_payload(row, (row.execute)()?);
    }
    if let Ok(row) = agent_contract_row(contract) {
        return replay_agent_contract_execution(row);
    }
    Err(FixedScriptRunnerError::Assertion(format!(
        "contract execution replay is not registered for `{contract}`"
    )))
}

fn replay_agent_contract_execution(
    row: &'static AgentContractRow,
) -> Result<Value, FixedScriptRunnerError> {
    let contract = row.semantic_oracle;
    let runner = row.execute;
    let result = run_on_sim_harness_stack(
        format!("replay-{contract}-contract"),
        SIM_HARNESS_STACK_LIMIT_BYTES,
        move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(FixedScriptRunnerError::Io)?;
            runner(&runtime)
        },
    )?;
    contract_execution_payload(row, result)
}

pub(super) fn contract_execution_payload<E>(
    row: &FixedContractRow<E>,
    result: Value,
) -> Result<Value, FixedScriptRunnerError> {
    let result_body = serde_json::to_vec(&result)?;
    let result_sha256 = sha256_hex(&result_body);
    let source_material = format!(
        "{}:{}:{result_sha256}",
        row.source_path, row.source_scenario
    );
    let source_hash = sha256_hex(source_material.as_bytes());
    Ok(json!({
        "contract": row.semantic_oracle,
        "source": {
            "kind": "fixed_dst_api_execution",
            "path": row.source_path,
            "scenario": row.source_scenario,
            "source_hash": source_hash,
            "result_sha256": result_sha256,
        },
        "result": result,
    }))
}

pub(super) fn fixed_texts_provider(
    kind: &'static str,
    responses: Vec<&'static str>,
) -> ProviderHandle {
    let responses = Arc::new(tokio::sync::Mutex::new(
        responses
            .into_iter()
            .map(str::to_string)
            .collect::<VecDeque<_>>(),
    ));
    lash_core::testing::TestProvider::builder()
        .kind(kind)
        .complete(move |_request| {
            let responses = Arc::clone(&responses);
            async move {
                let Some(text) = responses.lock().await.pop_front() else {
                    // A fixed contract that asks for one more completion than it
                    // scripted is a defect in the contract, never a flaky
                    // transport. Classifying it retryable is what turned that
                    // defect into a turn-driver backoff against the simulated
                    // clock -- which no contract advances -- and so into an
                    // unbounded hang instead of a named failure.
                    return Err(LlmTransportError::new(format!(
                        "{kind} provider exhausted its fixed response"
                    ))
                    .with_retry_verdict(
                        lash::provider::TransportRetryVerdict::NotRetryable,
                    ));
                };
                let expected_text = text.clone();
                let response = text_llm_response(text);
                let response_part_text = response_text_part(&response);
                if response.full_text() != expected_text
                    || response_part_text != Some(expected_text.as_str())
                {
                    return Err(LlmTransportError::new(format!(
                        "{kind} fixed response shape changed: expected full_text and text part {:?}, got full_text {:?} parts {:?}",
                        expected_text, response.full_text(), response.parts
                    )));
                }
                Ok(response)
            }
        })
        .build()
        .into_handle()
}

pub(super) struct ContractAppTools;

#[async_trait::async_trait]
impl lash_core::ToolProvider for ContractAppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![contract_app_lookup_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(contract_app_lookup_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() == "app_lookup" {
            lash_core::ToolOutcome::ok(json!({ "ok": true })).into()
        } else {
            lash_core::ToolOutcome::err_fmt(format!("Unknown contract app tool: {}", call.name()))
                .into()
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
fn contract_app_lookup_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:app_lookup",
        "app_lookup",
        "Lookup deterministic app state.",
        json!({
            "type": "object",
            "additionalProperties": false
        }),
        json!({
            "type": "object",
            "properties": {
                "ok": { "type": "boolean" }
            },
            "required": ["ok"],
            "additionalProperties": false
        }),
    )
    .expect("valid declared tool schemas")
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(
        ["tools"],
        "app_lookup",
    ))
}

pub(super) struct ContractDurableInputTools {
    key_tx: Mutex<Option<tokio::sync::oneshot::Sender<Result<lash_core::AwaitEventKey, String>>>>,
    attempt_count: Mutex<usize>,
}

impl ContractDurableInputTools {
    pub(super) fn new(
        key_tx: tokio::sync::oneshot::Sender<Result<lash_core::AwaitEventKey, String>>,
    ) -> Self {
        Self {
            key_tx: Mutex::new(Some(key_tx)),
            attempt_count: Mutex::new(0),
        }
    }

    pub(super) fn attempt_count(&self) -> usize {
        *self.attempt_count.lock_recover()
    }

    fn increment_attempt_count(&self) {
        *self.attempt_count.lock_recover() += 1;
    }

    fn send_key_result(&self, result: Result<lash_core::AwaitEventKey, String>) {
        if let Some(tx) = self.key_tx.lock_recover().take() {
            let _ = tx.send(result);
        }
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ContractDurableInputTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![contract_durable_input_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "mock_input_request")
            .then(|| Arc::new(contract_durable_input_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() != "mock_input_request" {
            return lash_core::ToolOutcome::err_fmt(format!(
                "Unknown durable input tool: {}",
                call.name()
            ))
            .into();
        }
        let question = call
            .args
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("answer")
            .to_string();
        let key = match call.context.completion_key() {
            Ok(key) => key,
            Err(err) => {
                self.send_key_result(Err(err.to_string()));
                return lash_core::ToolOutcome::err_fmt(err).into();
            }
        };
        self.increment_attempt_count();
        // The attempt body cannot append process events. It declares the
        // announcement instead, and the runtime appends it when the call parks.
        let announcement = lash_core::PendingAnnouncement::new(
            "process.yield",
            json!({
                "type": "work.input_request.opened",
                "request_id": "request-1",
                "question": question,
                "await_key_id": key.key_id,
            }),
            "mock-input-request:request-1",
        );
        self.send_key_result(Ok(key));
        lash_core::ToolOutcome::pending(
            lash_core::PendingCompletion::new().announcing(announcement),
        )
        .into()
    }
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
fn contract_durable_input_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:mock_input_request",
        "mock_input_request",
        "Open a durable input request and wait for the answer.",
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string" }
            },
            "required": ["question"],
            "additionalProperties": false
        }),
        json!({
            "type": "object",
            "properties": {
                "request_id": { "type": "string" },
                "answer": {}
            },
            "required": ["request_id", "answer"],
            "additionalProperties": true
        }),
    )
    .expect("valid declared tool schemas")
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(
        ["tools"],
        "mock_input_request",
    ))
    .with_declaration(lash_core::ToolDeclaration::deferring())
}

pub(super) fn standard_contract_turn_machine_config() -> lash_core::TurnMachineConfig {
    let protocol_driver: Arc<
        dyn lash_core::sansio::ProtocolDriverHandle<lash_core::HostTurnProtocol>,
    > = Arc::new(lash_protocol_standard::StandardDriver::default());
    lash_core::TurnMachineConfig {
        protocol_driver,
        model_tool_calls: lash_core::sansio::ModelToolCalls::fixture(),
        projector: Arc::new(lash_core::sansio::ChatContextProjector),
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::new(
                    "standard-max-turn-contract".to_string(),
                    std::num::NonZeroUsize::MIN.saturating_add(127_999),
                )
                .with_capability(lash_core::LlmProfileCapability::default())
                .with_extra_body(Default::default())
                .with_request_defaults(Default::default()),
            ),
        )
        .with_reasoning(Default::default()),
        turn_budget: lash_core::TurnBudget::Unbounded,
        no_progress_budget: Default::default(),
        attachment_acceptance: Default::default(),
        generation: lash_core::GenerationOptions::default(),
        autonomous: false,
        session_id: SessionId::from("standard-max-turn-contract"),
        agent_frame_id: "standard-max-turn-frame".to_string(),
        turn_id: TurnId::from("standard-max-turn"),
        emit_llm_trace: false,
        writer_formats: lash_core::build_newest_writer_formats(),
        termination: lash_core::ProtocolTurnOptions::empty(),
    }
}

pub(super) fn contract_user_message(content: &str) -> lash_core::Message {
    lash_core::Message {
        id: "m0".to_string(),
        role: lash_core::MessageRole::User,
        parts: vec![lash_core::Part::text(
            "m0.p0".to_string(),
            content.to_string(),
            None,
        )]
        .into(),
        origin: None,
        reply_marker: None,
    }
}

pub(super) fn drain_contract_turn_machine_effects(
    machine: &mut lash_core::TurnMachine,
) -> Vec<lash_core::Effect> {
    let mut effects = Vec::new();
    while let Some(effect) = machine.poll_effect() {
        // The contract is about the driver; the environment it runs in is
        // empty.
        if let lash_core::Effect::SyncExecutionEnvironment { id } = effect {
            machine.handle_response(lash_core::sansio::Response::ExecutionEnvironmentSynced {
                id,
                result: Ok(lash_core::sansio::ExecutionEnvironmentSync::default()),
            });
            continue;
        }
        effects.push(effect);
    }
    effects
}

pub(super) fn find_contract_llm_call(
    effects: &[lash_core::Effect],
) -> Option<&lash_core::sansio::EffectId> {
    effects.iter().find_map(|effect| match effect {
        lash_core::Effect::LlmCall { id, .. } => Some(id),
        _ => None,
    })
}

pub(super) fn turn_outcome_contract_json(
    outcome: &lash_core::facade_support::TurnOutcome,
) -> Value {
    match outcome {
        lash_core::facade_support::TurnOutcome::Stopped(
            lash_core::facade_support::TurnStop::MaxTurns,
        ) => json!({
            "kind": "stopped",
            "stop_reason": "max_turns",
        }),
        lash_core::facade_support::TurnOutcome::Stopped(other) => json!({
            "kind": "stopped",
            "stop_reason": format!("{other:?}"),
        }),
        lash_core::facade_support::TurnOutcome::Finished(
            lash_core::facade_support::TurnFinish::FinalValue { value },
        ) => json!({
            "kind": "final_value",
            "value": value,
        }),
        lash_core::facade_support::TurnOutcome::Finished(other) => json!({
            "kind": "finished",
            "finish": format!("{other:?}"),
        }),
        lash_core::facade_support::TurnOutcome::AgentFrameSwitch {
            frame_key,
            initial_nodes,
            task,
        } => json!({
            "kind": "agent_frame_switch",
            "frame_key": frame_key,
            "initial_nodes": initial_nodes,
            "task": task,
        }),
        lash_core::facade_support::TurnOutcome::SegmentBoundary { reason } => json!({
            "kind": "segment_boundary",
            "reason": reason,
        }),
    }
}
