use super::*;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use lash_sansio::sync::MutexExt;

pub(super) async fn append_contract_execution_boundaries(
    events: &mut Vec<crate::scheduler::DeliveredBoundary>,
    store: &mut ModelStore,
    seed: u64,
    checkpoint_writes: CheckpointWriteCollector,
) -> Result<(), FixedScriptRunnerError> {
    let start_sequence = events.len();
    let mut scheduler = BoundaryScheduler::with_events(
        seed ^ 0x5e_3a_11_ce_c0_de,
        contract_execution_boundaries(events, &checkpoint_writes).await?,
    );
    while let Some(mut delivered) = scheduler.deliver_next_with(|event| store.apply_boundary(event))
    {
        delivered.sequence += start_sequence;
        events.push(delivered);
    }
    Ok(())
}

/// One registered fixed contract execution: the semantic oracle id it proves,
/// the spec row it mirrors, the generated-run boundary it anchors to, and the
/// executor that reproduces its result. Registration is a const slice per
/// suite, so replaying one execution is a lookup plus one call.
pub(super) struct FixedContractRow<E> {
    pub(super) semantic_oracle: &'static str,
    pub(super) source_path: &'static str,
    pub(super) source_scenario: &'static str,
    pub(super) anchor: FixedContractAnchor,
    pub(super) execute: E,
}

/// How a fixed contract execution anchors into the generated boundary stream.
#[derive(Clone, Copy)]
pub(super) enum FixedContractAnchor {
    /// `generated_anchor` records the first successful provider boundary.
    RecordedProvider,
    /// `generated_anchor` additionally records the real provider-parser
    /// mutation boundary carrying this mutation name.
    RecordedProviderMutation(&'static str),
    /// `generated_anchor` records the tool boundary plus the same-actor
    /// provider continuation that follows it.
    RecordedToolThenProvider,
    /// The actor is the first successful provider boundary's alias; no
    /// `generated_anchor` evidence is emitted.
    ProviderActor,
}

pub(super) type TurnMachineContractExecutor = fn() -> Result<Value, FixedScriptRunnerError>;

async fn contract_execution_boundaries(
    events: &[crate::scheduler::DeliveredBoundary],
    checkpoint_writes: &CheckpointWriteCollector,
) -> Result<Vec<BoundaryEvent>, FixedScriptRunnerError> {
    let mut next_at = events
        .iter()
        .map(|event| event.at)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let mut proof_events = Vec::new();
    for row in STANDARD_CONTRACT_ROWS {
        let execution = contract_execution_payload(row, (row.execute)()?)?;
        proof_events.push(fixed_contract_execution_boundary(
            events, next_at, row, execution,
        )?);
        next_at = next_at.saturating_add(1);
    }
    for row in RLM_CONTRACT_ROWS {
        let execution = contract_execution_payload(row, (row.execute)()?)?;
        proof_events.push(fixed_contract_execution_boundary(
            events, next_at, row, execution,
        )?);
        next_at = next_at.saturating_add(1);
    }
    for execution in agent_contract_executions().await? {
        let boundary =
            fixed_contract_execution_boundary(events, next_at, execution.row, execution.payload)?;
        for mut write in execution.checkpoint_writes {
            write.attribution = Some(crate::store::CheckpointAttribution {
                session_id: SessionId::from(boundary.actor_alias.clone()),
                cause_boundary_id: boundary.boundary_id.clone(),
            });
            // Contract proofs execute in isolated facade worlds whose opaque
            // execution-state identities are intentionally not seed-canonical.
            // Preserve the durable stored/ref disposition, but do not let those
            // unstable logical JSON lengths churn the deterministic transcript.
            for component in &mut write.components {
                if let CheckpointComponentWriteKind::Stored { logical_bytes } = &mut component.kind
                {
                    *logical_bytes = None;
                }
            }
            checkpoint_writes.push(write);
        }
        proof_events.push(boundary);
        next_at = next_at.saturating_add(1);
    }
    Ok(proof_events)
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn fixed_contract_execution_boundary<E>(
    events: &[crate::scheduler::DeliveredBoundary],
    at: u64,
    row: &FixedContractRow<E>,
    mut execution: Value,
) -> Result<BoundaryEvent, FixedScriptRunnerError> {
    let contract = row.semantic_oracle;
    let proof_id = contract.replace(['.', '_'], "-");
    let actor_alias = match row.anchor {
        FixedContractAnchor::RecordedProvider => {
            let provider = first_successful_provider(events).ok_or_else(|| {
                FixedScriptRunnerError::Assertion(format!(
                    "could not anchor {contract} execution to a successful generated provider boundary"
                ))
            })?;
            execution
                .as_object_mut()
                .expect("contract execution object")
                .insert(
                    "generated_anchor".to_string(),
                    json!({
                        "provider_boundary": provider.boundary_id,
                        "actor": provider.actor_alias,
                        "provider_sequence": provider.sequence,
                    }),
                );
            provider.actor_alias.clone()
        }
        FixedContractAnchor::RecordedProviderMutation(mutation) => {
            let provider = first_successful_provider(events).ok_or_else(|| {
                FixedScriptRunnerError::Assertion(format!(
                    "could not anchor {contract} execution to a successful generated provider boundary"
                ))
            })?;
            let parser = events
                .iter()
                .find(|event| {
                    event.kind == BoundaryKind::ProviderMutation
                        && event
                            .observed
                            .get("mutation")
                            .or_else(|| event.payload.get("mutation"))
                            .and_then(Value::as_str)
                            == Some(mutation)
                        && event
                            .observed
                            .pointer(
                                "/provider_parser_matrix/matrix/real_provider_parser_execution",
                            )
                            .and_then(Value::as_bool)
                            == Some(true)
                })
                .ok_or_else(|| {
                    FixedScriptRunnerError::Assertion(format!(
                        "could not anchor {contract} execution to real parser mutation `{mutation}`"
                    ))
                })?;
            execution
                .as_object_mut()
                .expect("contract execution object")
                .insert(
                    "generated_anchor".to_string(),
                    json!({
                        "provider_boundary": provider.boundary_id,
                        "provider_sequence": provider.sequence,
                        "provider_mutation_boundary": parser.boundary_id,
                        "mutation": mutation,
                        "real_provider_parser_execution": true,
                        "actor": provider.actor_alias,
                    }),
                );
            provider.actor_alias.clone()
        }
        FixedContractAnchor::RecordedToolThenProvider => {
            let Some((tool, provider)) = generated_tool_then_same_actor_provider(events) else {
                return Err(FixedScriptRunnerError::Assertion(format!(
                    "could not anchor {contract} execution to tool result and same-actor provider continuation"
                )));
            };
            execution
                .as_object_mut()
                .expect("contract execution object")
                .insert(
                    "generated_anchor".to_string(),
                    json!({
                        "tool_boundary": tool.boundary_id,
                        "continuation_provider_boundary": provider.boundary_id,
                        "actor": tool.actor_alias,
                        "tool_sequence": tool.sequence,
                        "continuation_provider_sequence": provider.sequence,
                        "same_actor_continuation": tool.actor_alias == provider.actor_alias
                            && provider.sequence > tool.sequence,
                    }),
                );
            tool.actor_alias.clone()
        }
        FixedContractAnchor::ProviderActor => first_successful_provider(events)
            .ok_or_else(|| {
                FixedScriptRunnerError::Assertion(format!(
                    "could not anchor {contract} execution to a successful generated provider boundary"
                ))
            })?
            .actor_alias
            .clone(),
    };
    Ok(contract_execution_boundary(
        &actor_alias,
        &proof_id,
        at,
        execution,
    ))
}

fn generated_tool_then_same_actor_provider(
    events: &[crate::scheduler::DeliveredBoundary],
) -> Option<(
    &crate::scheduler::DeliveredBoundary,
    &crate::scheduler::DeliveredBoundary,
)> {
    events
        .iter()
        .filter(|event| {
            event.kind == BoundaryKind::Tool
                && event.observed.get("runtime_tool_output").is_some()
                && event
                    .observed
                    .get("execution_count")
                    .and_then(Value::as_u64)
                    == Some(1)
        })
        .find_map(|tool| {
            events
                .iter()
                .filter(|provider| {
                    provider.kind == BoundaryKind::Provider
                        && provider.actor_alias == tool.actor_alias
                        && provider.sequence > tool.sequence
                        && provider.observed.get("success").and_then(Value::as_bool) == Some(true)
                })
                .min_by_key(|provider| provider.sequence)
                .map(|provider| (tool, provider))
        })
}

fn contract_execution_boundary(
    actor_alias: &str,
    proof_id: &str,
    at: u64,
    contract_execution: Value,
) -> BoundaryEvent {
    BoundaryEvent::new(
        format!("{actor_alias}:contract-execution:{proof_id}"),
        actor_alias.to_string(),
        BoundaryKind::Trigger,
        at,
        format!("contract-execution.{proof_id}"),
        json!({
            "session": actor_alias,
            "source_key": format!("contract-execution/{actor_alias}/{proof_id}"),
            "started_process": false,
            "contract_execution": contract_execution,
        }),
    )
}

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

    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id == contract_durable_input_definition().id()
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
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(
        ["tools"],
        "mock_input_request",
    ))
}

pub(super) fn standard_contract_turn_machine_config() -> lash_core::TurnMachineConfig {
    let protocol_driver: Arc<
        dyn lash_core::sansio::ProtocolDriverHandle<lash_core::HostTurnProtocol>,
    > = Arc::new(lash_protocol_standard::StandardDriver::default());
    lash_core::TurnMachineConfig {
        protocol_driver,
        projector: Arc::new(lash_core::sansio::ChatContextProjector),
        sync_execution_environment: false,
        model: "standard-max-turn-contract".to_string(),
        max_context_tokens: None,
        turn_budget: lash_core::TurnBudget::Unbounded,
        no_progress_budget: Default::default(),
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        autonomous: false,
        tool_specs: Vec::new().into(),
        system_prompt: std::sync::Arc::from(""),
        projector_turn_inputs: Default::default(),
        session_id: SessionId::from("standard-max-turn-contract"),
        agent_frame_id: "standard-max-turn-frame".to_string(),
        turn_id: TurnId::from("standard-max-turn"),
        emit_llm_trace: false,
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
    }
}

pub(super) fn drain_contract_turn_machine_effects(
    machine: &mut lash_core::TurnMachine,
) -> Vec<lash_core::Effect> {
    let mut effects = Vec::new();
    while let Some(effect) = machine.poll_effect() {
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
    }
}

fn first_successful_provider(
    events: &[crate::scheduler::DeliveredBoundary],
) -> Option<&crate::scheduler::DeliveredBoundary> {
    events
        .iter()
        .filter(|event| {
            event.kind == BoundaryKind::Provider
                && event.observed.get("success").and_then(Value::as_bool) == Some(true)
        })
        .min_by_key(|event| event.sequence)
}
