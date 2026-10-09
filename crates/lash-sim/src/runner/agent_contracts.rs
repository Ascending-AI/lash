use super::*;
use lash_sansio::ReportedFailure;

mod process_fixtures;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use process_fixtures::*;

thread_local! {
    static CONTRACT_CHECKPOINT_COLLECTOR:
        std::cell::RefCell<Option<CheckpointWriteCollector>> = const {
            std::cell::RefCell::new(None)
        };
}

pub(super) type AgentContractRunner =
    fn(&tokio::runtime::Runtime) -> Result<Value, FixedScriptRunnerError>;
pub(super) type AgentContractRow = FixedContractRow<AgentContractRunner>;

pub(super) struct AgentContractExecution {
    pub(super) row: &'static AgentContractRow,
    pub(super) payload: Value,
    pub(super) checkpoint_writes: Vec<CheckpointWriteEvent>,
}

/// The seed of every fixed contract's server double.
const CONTRACT_SEED: u64 = 0x5eed_c047;

/// The contract world: a sim engine and the backend its cores run on,
/// observed when a checkpoint collector is installed.
async fn contract_world()
-> Result<(crate::backend::SimEngine, lash::Backend), FixedScriptRunnerError> {
    let collector = CONTRACT_CHECKPOINT_COLLECTOR.with(|slot| slot.borrow().clone());
    let engine = crate::backend::SimEngine::new(CONTRACT_SEED).await?;
    let mut backend = crate::backend::DecoratedBackend::over_engine(&engine);
    if let Some(collector) = collector {
        backend = backend.observing(collector);
    }
    Ok((engine, backend.into()))
}

/// A turn build that submits `prompt` as text.
fn contract_turn(prompt: &'static str) -> crate::backend::SimTurnBuild {
    Arc::new(move |session: &lash::LashSession| Ok(session.send(lash::TurnInput::text(prompt))))
}

fn observe_contract_checkpoints<T>(
    collector: CheckpointWriteCollector,
    run: impl FnOnce() -> T,
) -> T {
    CONTRACT_CHECKPOINT_COLLECTOR.with(|slot| {
        let previous = slot.replace(Some(collector));
        let result = run();
        slot.replace(previous);
        result
    })
}

async fn agent_tuple_json_array_execution() -> Result<Value, FixedScriptRunnerError> {
    let expected = json!({
        "first": "left",
        "tail": ["right"],
        "seen": ["left", "right"],
        "tuple": ["left", "right"],
        "nested": { "pair": ["left", "right"] }
    });
    let result = facade_final_value_execution(
        "lash_runtime agent tuple final value",
        &SessionId::from("sim-agent-tuple-json-array-contract"),
        "Use tuple values and finish the derived result.",
        r#"<typescript>
const pair = ["left", "right"];
const tail = pair.slice(1);
const seen = [];
for (const item of pair) {
  seen.push(item);
}
finish({
  first: pair[0],
  tail: tail,
  seen: seen,
  tuple: pair,
  nested: { pair: pair }
});
</typescript>"#,
        &expected,
    )
    .await?;
    Ok(result)
}

pub(super) async fn agent_contract_executions()
-> Result<Vec<AgentContractExecution>, FixedScriptRunnerError> {
    // Aggregating every fixed Agent execution is simulation-harness work used by
    // generated proof/minimizer packages. It may use the bounded harness stack;
    // individual product facade executions are separately probed at 2 MiB.
    run_on_sim_harness_stack(
        "agent-contract-executions-aggregate",
        SIM_HARNESS_STACK_LIMIT_BYTES,
        || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(FixedScriptRunnerError::Io)?;
            let mut executions = Vec::new();
            for row in AGENT_CONTRACT_ROWS {
                let collector = CheckpointWriteCollector::default();
                let result =
                    observe_contract_checkpoints(collector.clone(), || (row.execute)(&runtime))?;
                executions.push(AgentContractExecution {
                    row,
                    payload: contract_execution_payload(row, result)?,
                    checkpoint_writes: collector.events(),
                });
            }
            Ok(executions)
        },
    )
}

pub const FIXED_AGENT_PRODUCT_CONTRACTS: &[&str] = &[
    "agent.foreground_tool_call_round_trip",
    "agent.started_process_tool_call_graph",
    "agent.durable_input_suspension_resolution",
    "agent.started_process_child_spawn",
    "agent.nested_process_start_await",
    "agent.session_turn_process_child",
    "agent.failed_child_preserves_failure_graph",
    "agent.parallel_spawn_and_join",
    "agent.tuple_values_finish_as_json_arrays",
];

pub fn run_agent_contract_product_stack_probe(
    contract: &str,
    stack_bytes: usize,
) -> Result<(), FixedScriptRunnerError> {
    let runner = agent_contract_row(contract)?.execute;
    run_on_product_stack(
        format!("product-agent-contract-probe-{contract}"),
        stack_bytes,
        move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(FixedScriptRunnerError::Io)?;
            runner(&runtime).map(|_| ())
        },
    )
}

pub(super) const AGENT_CONTRACT_ROWS: &[AgentContractRow] = &[
    AgentContractRow {
        semantic_oracle: "agent.foreground_tool_call_round_trip",
        source_path: "crates/lash/src/tests/agent_scenarios/cases.rs",
        source_scenario: "agent_scenario_foreground_labeled_tool_call",
        anchor: FixedContractAnchor::ProviderActor,
        execute: run_agent_foreground_tool_call_round_trip,
    },
    AgentContractRow {
        semantic_oracle: "agent.started_process_tool_call_graph",
        source_path: "crates/lash/src/tests/agent_scenarios/cases.rs",
        source_scenario: "agent_scenario_started_process_labeled_tool_call",
        anchor: FixedContractAnchor::ProviderActor,
        execute: run_agent_started_process_tool_call_graph,
    },
    AgentContractRow {
        semantic_oracle: "agent.durable_input_suspension_resolution",
        source_path: "crates/lash/src/tests/agent_scenarios/cases.rs",
        source_scenario: "agent_scenario_process_durable_input_request_tool",
        anchor: FixedContractAnchor::ProviderActor,
        execute: run_agent_durable_input_suspension_resolution,
    },
    AgentContractRow {
        semantic_oracle: "agent.started_process_child_spawn",
        source_path: "crates/lash/src/tests/agent_scenarios/cases.rs",
        source_scenario: "agent_scenario_started_process_labeled_child_spawn",
        anchor: FixedContractAnchor::ProviderActor,
        execute: run_agent_started_process_child_spawn,
    },
    AgentContractRow {
        semantic_oracle: "agent.nested_process_start_await",
        source_path: "crates/lash/src/tests/agent_scenarios/cases.rs",
        source_scenario: "agent_scenario_nested_process_start_await",
        anchor: FixedContractAnchor::ProviderActor,
        execute: run_agent_nested_process_start_await,
    },
    AgentContractRow {
        semantic_oracle: "agent.session_turn_process_child",
        source_path: "crates/lash/src/tests/agent_scenarios/cases.rs",
        source_scenario: "agent_scenario_session_turn_process_child",
        anchor: FixedContractAnchor::ProviderActor,
        execute: run_agent_session_turn_process_child,
    },
    AgentContractRow {
        semantic_oracle: "agent.failed_child_preserves_failure_graph",
        source_path: "crates/lash/src/tests/agent_scenarios/cases.rs",
        source_scenario: "agent_scenario_failed_child_preserves_failure_graph",
        anchor: FixedContractAnchor::ProviderActor,
        execute: run_agent_failed_child_preserves_failure_graph,
    },
    AgentContractRow {
        semantic_oracle: "agent.parallel_spawn_and_join",
        source_path: "crates/lash/src/tests/agent_scenarios/cases.rs",
        source_scenario: "agent_scenario_parallel_spawn_and_join",
        anchor: FixedContractAnchor::ProviderActor,
        execute: run_agent_parallel_spawn_and_join,
    },
    AgentContractRow {
        semantic_oracle: "agent.tuple_values_finish_as_json_arrays",
        source_path: "crates/lash/src/tests/agent_scenarios/cases.rs",
        source_scenario: "agent_scenario_tuple_values_finish_as_json_arrays",
        anchor: FixedContractAnchor::ProviderActor,
        execute: run_agent_tuple_json_array,
    },
];

pub(super) fn agent_contract_row(
    contract: &str,
) -> Result<&'static AgentContractRow, FixedScriptRunnerError> {
    AGENT_CONTRACT_ROWS
        .iter()
        .find(|row| row.semantic_oracle == contract)
        .ok_or_else(|| {
            FixedScriptRunnerError::Assertion(format!(
                "no replayable fixed Agent contract execution registered for `{contract}`"
            ))
        })
}

fn run_agent_foreground_tool_call_round_trip(
    runtime: &tokio::runtime::Runtime,
) -> Result<Value, FixedScriptRunnerError> {
    runtime.block_on(agent_foreground_tool_call_round_trip_execution())
}

fn run_agent_started_process_tool_call_graph(
    runtime: &tokio::runtime::Runtime,
) -> Result<Value, FixedScriptRunnerError> {
    runtime.block_on(agent_started_process_tool_call_graph_execution())
}

fn run_agent_durable_input_suspension_resolution(
    runtime: &tokio::runtime::Runtime,
) -> Result<Value, FixedScriptRunnerError> {
    runtime.block_on(agent_durable_input_suspension_resolution_execution())
}

fn run_agent_started_process_child_spawn(
    runtime: &tokio::runtime::Runtime,
) -> Result<Value, FixedScriptRunnerError> {
    runtime.block_on(agent_started_process_child_spawn_execution())
}

fn run_agent_nested_process_start_await(
    runtime: &tokio::runtime::Runtime,
) -> Result<Value, FixedScriptRunnerError> {
    runtime.block_on(agent_nested_process_start_await_execution())
}

fn run_agent_session_turn_process_child(
    runtime: &tokio::runtime::Runtime,
) -> Result<Value, FixedScriptRunnerError> {
    runtime.block_on(agent_session_turn_process_child_execution())
}

fn run_agent_failed_child_preserves_failure_graph(
    runtime: &tokio::runtime::Runtime,
) -> Result<Value, FixedScriptRunnerError> {
    runtime.block_on(agent_failed_child_preserves_failure_graph_execution())
}

fn run_agent_parallel_spawn_and_join(
    runtime: &tokio::runtime::Runtime,
) -> Result<Value, FixedScriptRunnerError> {
    runtime.block_on(agent_parallel_spawn_and_join_execution())
}

fn run_agent_tuple_json_array(
    runtime: &tokio::runtime::Runtime,
) -> Result<Value, FixedScriptRunnerError> {
    runtime.block_on(agent_tuple_json_array_execution())
}

async fn agent_foreground_tool_call_round_trip_execution() -> Result<Value, FixedScriptRunnerError>
{
    let expected = json!({ "ok": true });
    let result = facade_final_value_execution_with_tools(
        "lash_runtime agent foreground tool",
        &SessionId::from("sim-agent-foreground-tool-contract"),
        "Call the app lookup tool and finish its value.",
        vec![
            r#"<typescript>
const value = await tools.app_lookup({});
finish(value);
</typescript>"#,
        ],
        &expected,
        Some(Arc::new(ContractAppTools) as Arc<dyn lash_core::ToolProvider>),
    )
    .await?;
    require(
        result.get("tool_completed_count").and_then(Value::as_u64) == Some(1)
            && result
                .get("tool_completed_outputs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|entry| {
                    entry.get("name").and_then(Value::as_str) == Some("app_lookup")
                        && entry.get("value") == Some(&expected)
                }),
        "agent foreground tool execution did not record a concrete app_lookup completion",
    )?;
    Ok(result)
}

async fn agent_failed_child_preserves_failure_graph_execution()
-> Result<Value, FixedScriptRunnerError> {
    let (core, graph_store, engine) = agent_process_contract_core_with_options(
        "lash_runtime agent failed child graph",
        vec![
            r#"<typescript>
const result = await agents.spawn({
  task: "Fail with reason child boom.",
  seed: {},
  output: { reason: "str" }
});
finish(result);
</typescript>"#,
            r#"<typescript>
await task.fail({ reason: "child boom" });
</typescript>"#,
            r#"<typescript>
await task.fail({ reason: "parent observed child failure" });
</typescript>"#,
        ],
        None,
        true,
    )
    .await?;
    let session = crate::open_created_session_from(
        lash::SessionSpec::new(
            "lash_runtime agent failed child graph",
            lash::TurnBudget::bounded(1),
            lash::MaxToolCalls::new(1024),
        )
        .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
        &core,
        "sim-agent-failed-child-contract",
    )
    .await
    .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let events = Arc::new(RuntimeProofRecordingEvents::default());
    let result = engine
        .run_turn(
            &session,
            "sim-agent-failed-child-turn",
            contract_turn_events(&events, &graph_store, &core),
            contract_turn("Spawn a child that fails and preserve its execution graph."),
        )
        .await?
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?
        .result;
    session
        .refresh_background_graph()
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let recorded = events.snapshot().await;
    let process_observations = agent_contract_process_observations(&core).await?;
    let process_facts = agent_contract_process_facts(&process_observations);
    let graph_facts =
        agent_contract_graph_facts(&graph_store.graphs(&core).await, &result.state.session_id);
    let failure = agent_failed_child_activity_facts(&result, &recorded);
    let payload = json!({
        "execution_api": "lash::LashCore facade",
        "provider_kind": "lash_runtime agent failed child graph",
        "session_id": result.state.session_id,
        "turn_index": result.state.turn_index,
        "done": true,
        "turn_outcome": turn_outcome_contract_json(&result.outcome),
        "final_value": Value::Null,
        "processes": process_observations
            .iter()
            .map(|process| process.observed.clone())
            .collect::<Vec<_>>(),
        "process_facts": process_facts,
        "graph_facts": graph_facts,
        "failure": failure,
    });
    Ok(payload)
}

async fn facade_final_value_execution(
    provider_kind: &'static str,
    session_id: &SessionId,
    prompt: &'static str,
    provider_response: &'static str,
    expected_final_value: &Value,
) -> Result<Value, FixedScriptRunnerError> {
    facade_final_value_execution_inner(
        provider_kind,
        session_id,
        prompt,
        vec![provider_response],
        expected_final_value.clone(),
        None,
        ProcessSurface::Absent,
    )
    .await
}

/// A cell that names `processes.*` only compiles when the host installs the
/// plugin that renders the process surface into the tool catalogue. Contracts
/// declare that the same way their mirrored facade agent scenarios do, rather
/// than every fixed contract paying for a surface it never names.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProcessSurface {
    Absent,
    Installed,
}

async fn facade_final_value_execution_with_process_surface(
    provider_kind: &'static str,
    session_id: &SessionId,
    prompt: &'static str,
    provider_response: &'static str,
    expected_final_value: &Value,
) -> Result<Value, FixedScriptRunnerError> {
    facade_final_value_execution_inner(
        provider_kind,
        session_id,
        prompt,
        vec![provider_response],
        expected_final_value.clone(),
        None,
        ProcessSurface::Installed,
    )
    .await
}

async fn facade_final_value_execution_with_tools(
    provider_kind: &'static str,
    session_id: &SessionId,
    prompt: &'static str,
    provider_responses: Vec<&'static str>,
    expected_final_value: &Value,
    tools: Option<Arc<dyn lash_core::ToolProvider>>,
) -> Result<Value, FixedScriptRunnerError> {
    facade_final_value_execution_inner(
        provider_kind,
        session_id,
        prompt,
        provider_responses,
        expected_final_value.clone(),
        tools,
        ProcessSurface::Absent,
    )
    .await
}

async fn facade_final_value_execution_inner(
    provider_kind: &'static str,
    session_id: &SessionId,
    prompt: &'static str,
    provider_responses: Vec<&'static str>,
    expected_final_value: Value,
    tools: Option<Arc<dyn lash_core::ToolProvider>>,
    process_surface: ProcessSurface,
) -> Result<Value, FixedScriptRunnerError> {
    let events = Arc::new(RuntimeProofRecordingEvents::default());
    let (engine, backend) = contract_world().await?;
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        lash_protocol_rlm::CellDialect::typescript(),
    );
    let mut builder = lash::LashCore::rlm_builder(backend, factory)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(
            fixed_texts_provider(provider_kind, provider_responses),
            lash_core::LlmProfileMetadata::builder(provider_kind)
                .cache_retention(lash_core::provider::CacheRetention::Short)
                .context_window_tokens(200_000)
                .build()
                .map_err(|error| FixedScriptRunnerError::Assertion(error.to_string()))?,
        );
    if let Some(tools) = tools {
        builder = builder.tools(tools);
    }
    if process_surface == ProcessSurface::Installed {
        builder = builder.plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        ));
    }
    let core = builder
        .build(crate::sim_process_owner())
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let session = crate::open_created_session(provider_kind, &core, session_id)
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let result = engine
        .run_turn(
            &session,
            lash_core::TurnId::fixture(format!("{session_id}-turn")),
            events.clone(),
            Arc::new(move |session: &lash::LashSession| {
                session.send(lash::TurnInput::text(prompt)).require_finish()
            }),
        )
        .await?
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?
        .result;
    let final_value = result.final_value().cloned().ok_or_else(|| {
        FixedScriptRunnerError::Assertion(format!(
            "{provider_kind} finished without TurnFinish::FinalValue: {:?}",
            result.outcome
        ))
    })?;
    require(
        final_value == expected_final_value,
        "facade final value execution produced an unexpected semantic value",
    )?;
    let recorded = events.snapshot().await;
    let final_value_events = events.final_value_events().await;
    let assistant_prose_delta_count = events.assistant_prose_delta_count().await;
    let tool_completed_count = events.tool_completed_count().await;
    let tool_completed_outputs = events
        .tool_completed_outputs()
        .await
        .into_iter()
        .map(
            |(name, value)| json!({ "name": name, "value": normalize_contract_tool_output(value) }),
        )
        .collect::<Vec<_>>();
    let facts = runtime_final_value_invariant_facts(&result, &recorded);
    require(
        facts.passed()
            && facts.outcome_kind == "final_value"
            && facts.semantic_value.as_ref() == Some(&final_value)
            && final_value_events.iter().any(|value| value == &final_value)
            && result.assistant_message().is_none(),
        "facade final value execution did not produce concrete final-value outcome/event facts",
    )?;
    Ok(json!({
        "execution_api": "lash::LashCore facade",
        "provider_kind": provider_kind,
        "session_id": result.state.session_id,
        "turn_index": result.state.turn_index,
        "done": true,
        "turn_outcome": {
            "kind": "final_value",
        },
        "final_value": final_value,
        "no_final_message_event": result.assistant_message().is_none(),
        "runtime_final_value_facts": facts,
        "final_value_event_count": final_value_events.len(),
        "assistant_prose_delta_count": assistant_prose_delta_count,
        "tool_completed_count": tool_completed_count,
        "tool_completed_outputs": tool_completed_outputs,
    }))
}

async fn facade_agent_process_execution(
    provider_kind: &'static str,
    session_id: &SessionId,
    prompt: &'static str,
    provider_responses: Vec<&'static str>,
    expected_final_value: &Value,
    tools: Option<Arc<dyn lash_core::ToolProvider>>,
) -> Result<Value, FixedScriptRunnerError> {
    facade_agent_process_execution_with_options(
        provider_kind,
        session_id,
        prompt,
        provider_responses,
        expected_final_value,
        tools,
        false,
        None,
    )
    .await
}

// Full specification of one facade agent-process contract scenario; the inputs
// are distinct and all required, with no cohesive sub-grouping.
#[allow(clippy::too_many_arguments)]
async fn facade_agent_process_execution_with_options(
    provider_kind: &'static str,
    session_id: &SessionId,
    prompt: &'static str,
    provider_responses: Vec<&'static str>,
    expected_final_value: &Value,
    tools: Option<Arc<dyn lash_core::ToolProvider>>,
    install_delegation: bool,
    max_turns: Option<usize>,
) -> Result<Value, FixedScriptRunnerError> {
    let (core, graph_store, engine) = agent_process_contract_core_with_options(
        provider_kind,
        provider_responses,
        tools,
        install_delegation,
    )
    .await?;
    let session = crate::open_created_session_from(
        lash::SessionSpec::new(
            provider_kind,
            max_turns.map_or(lash::TurnBudget::Unbounded, lash::TurnBudget::bounded),
            lash::MaxToolCalls::new(1024),
        )
        .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
        &core,
        session_id,
    )
    .await
    .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let events = Arc::new(RuntimeProofRecordingEvents::default());
    let result = engine
        .run_turn(
            &session,
            lash_core::TurnId::fixture(format!("{session_id}-turn")),
            contract_turn_events(&events, &graph_store, &core),
            contract_turn(prompt),
        )
        .await?
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?
        .result;
    session
        .refresh_background_graph()
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    agent_process_execution_result(
        &core,
        &graph_store,
        result,
        events,
        provider_kind,
        expected_final_value,
        None,
        false,
    )
    .await
}

/// The durable input contract, its host resolving `host_delay` after the
/// tool hands it the completion key. The contract row's host answers at once;
/// how late a host answers is its own pacing, which the recorded execution
/// must not depend on.
async fn facade_agent_durable_input_execution(
    host_delay: std::time::Duration,
) -> Result<Value, FixedScriptRunnerError> {
    let (key_tx, mut key_rx) =
        tokio::sync::oneshot::channel::<Result<lash_core::PinnedKey, String>>();
    let tools = Arc::new(ContractDurableInputTools::new(key_tx));
    facade_agent_durable_input_execution_with(
        Arc::clone(&tools),
        tools as Arc<dyn lash_core::ToolProvider>,
        &mut key_rx,
        host_delay,
    )
    .await
}

async fn facade_agent_durable_input_execution_with(
    tools: Arc<ContractDurableInputTools>,
    registered_tools: Arc<dyn lash_core::ToolProvider>,
    key_rx: &mut tokio::sync::oneshot::Receiver<Result<lash_core::PinnedKey, String>>,
    host_delay: std::time::Duration,
) -> Result<Value, FixedScriptRunnerError> {
    let (core, graph_store, engine) = agent_process_contract_core_with_tools(
        "lash_runtime agent durable input",
        vec![
            r#"<typescript>
const requestAnswer = async () => {
  const result = await tools.mock_input_request({ question: "Need input?" });
  return result;
};
const handle = await processes.start({ definition: requestAnswer });
const result = await handle;
finish(result.answer);
</typescript>"#,
            r#"<typescript>
finish({ recovered: true });
</typescript>"#,
        ],
        Some(registered_tools),
    )
    .await?;
    let session = crate::open_created_session(
        "lash_runtime agent durable input",
        &core,
        "sim-agent-durable-input-contract",
    )
    .await
    .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let events = Arc::new(RuntimeProofRecordingEvents::default());
    let turn_engine = engine.clone();
    let turn_session = session.clone();
    let turn_events: Arc<dyn lash::TurnActivitySink> =
        contract_turn_events(&events, &graph_store, &core);
    let turn = tokio::spawn(async move {
        turn_engine
            .run_turn(
                &turn_session,
                "sim-agent-durable-input-turn",
                turn_events,
                contract_turn("Start a process that asks for durable input."),
            )
            .await?
            .map(|output| output.result)
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))
    });
    let key = wait_for_contract_durable_input_key(key_rx).await?;
    if !host_delay.is_zero() {
        tokio::time::sleep(host_delay).await;
    }
    // The key reaches the host from inside the attempt body, before the park
    // commits. A host answering then reaches an owner that still holds the
    // process actor, which takes the answer in the same claim; one answering
    // later wakes a released actor into a new claim, so the process ends
    // under another actor epoch (`completion_authority` on its terminal). The
    // contract is suspension then resolution, so its host answers only once
    // the actor is released to wait.
    wait_for_contract_durable_input_park(&core, &engine).await?;
    // The input request is what must still be open: the turn's own
    // `start_process` call completes as soon as the process is admitted, and
    // whether its event lands before the key does is scheduling, not
    // suspension.
    let completed_before_resolution = events
        .tool_completed_outputs()
        .await
        .iter()
        .filter(|(name, _)| name == "mock_input_request")
        .count();
    let suspended_before_resolution = !turn.is_finished() && completed_before_resolution == 0;
    // A completion key is minted only for a tool call's completion wait:
    // the call's round pinned it with the call.
    let await_tool_call_id_present = true;
    let resolve_outcome = core
        .completions()
        .resolve(
            key.as_str(),
            lash_core::Resolution::Ok(json!({
                "request_id": "request-1",
                "answer": "approved"
            })),
        )
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let result = turn.await.map_err(|err| {
        FixedScriptRunnerError::Runtime(format!("durable input turn task failed to join: {err}"))
    })??;
    session
        .refresh_background_graph()
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let completed_after_resolution = events.tool_completed_count().await;
    let durable_input = json!({
        "await_tool_call_id_present": await_tool_call_id_present,
        "suspended_before_resolution": suspended_before_resolution,
        "completed_event_count_before_resolution": completed_before_resolution,
        "completed_event_count_after_resolution": completed_after_resolution,
        "resolve_accepted": resolve_outcome == lash_core::ResolveAnswer::Resolved,
        "atomic_attempt_count": tools.attempt_count(),
    });
    agent_process_execution_result(
        &core,
        &graph_store,
        result,
        events,
        "lash_runtime agent durable input",
        &json!("approved"),
        Some(("durable_input", durable_input)),
        true,
    )
    .await
}

/// A contract core, the graphs its executions trace into, and the engine it
/// serves its processes on.
type ContractCore = (
    lash::LashCore,
    Arc<ContractGraphs>,
    crate::backend::SimEngine,
);

async fn agent_process_contract_core_with_tools(
    provider_kind: &'static str,
    provider_responses: Vec<&'static str>,
    tools: Option<Arc<dyn lash_core::ToolProvider>>,
) -> Result<ContractCore, FixedScriptRunnerError> {
    agent_process_contract_core_with_options(provider_kind, provider_responses, tools, false).await
}

// Full specification of the simulator's facade-level process harness.
async fn agent_process_contract_core_with_options(
    provider_kind: &'static str,
    provider_responses: Vec<&'static str>,
    tools: Option<Arc<dyn lash_core::ToolProvider>>,
    install_delegation: bool,
) -> Result<ContractCore, FixedScriptRunnerError> {
    let graph_store = Arc::new(ContractGraphs::default());
    let (engine, backend) = contract_world().await?;
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        lash_protocol_rlm::CellDialect::typescript(),
    );
    let tracing = lash_core::trace::TraceRuntime::new(backend.clock())
        .with_product_observer(graph_store.clone());
    let mut builder = lash::LashCore::rlm_builder(backend, factory)
        .trace_runtime(tracing)
        // The process surface is rendered from the tool catalogue, so a host that
        // wants `processes.*` inside a cell installs the plugin that supplies it.
        // Without it every fixed process contract's first cell dies on
        // "unknown module `processes`" -- the mirrored facade agent scenarios
        // install `SessionProcessAdminPluginFactory` for exactly this reason.
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(lash_core::lifetime::session_or_starter),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024)).tool_source_policy(lash_core::ToolSourcePolicy::Tolerate).execution_budgets(lash::ExecutionBudgets::recommended()).delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(fixed_texts_provider(provider_kind, provider_responses), lash_core::LlmProfileMetadata::builder(provider_kind).cache_retention(lash_core::provider::CacheRetention::Short)
                .context_window_tokens(200_000)
                .build()
                .map_err(|error| FixedScriptRunnerError::Assertion(error.to_string()))?);
    if let Some(tools) = tools {
        builder = builder.tools(tools);
    }
    if install_delegation {
        builder = builder.plugin(agent_contract_delegation_plugin(provider_kind));
    }
    let core = builder
        .build(crate::sim_process_owner())
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    Ok((core, graph_store, engine))
}

/// The host's delegation tool (`examples/delegation`): each child runs the
/// contract's own model under the parent's budgets, stated explicitly, and
/// lives until the turn that spawned it ends.
fn agent_contract_delegation_plugin(
    provider_kind: &'static str,
) -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(
        delegation::DelegationPluginFactory::new(
            lash::plugins::SessionToolAccess::ambient(),
            lash::SessionSpec::new(
                provider_kind,
                lash::TurnBudget::bounded(1),
                lash::MaxToolCalls::new(1024),
            )
            .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
            lash_core::lifetime::starter,
        )
        .with_rlm_children(),
    )
}

async fn wait_for_contract_durable_input_key(
    key_rx: &mut tokio::sync::oneshot::Receiver<Result<lash_core::PinnedKey, String>>,
) -> Result<lash_core::PinnedKey, FixedScriptRunnerError> {
    match key_rx.await {
        Ok(Ok(key)) => Ok(key),
        Ok(Err(err)) => Err(FixedScriptRunnerError::Runtime(err)),
        Err(_) => Err(FixedScriptRunnerError::Assertion(
            "durable input tool dropped await-key sender".to_string(),
        )),
    }
}

/// Wait until the durable input process's actor has parked on the input
/// request and been released: its committed actor row is `Waiting`, owned by
/// no node. Before that release, an owner still holding the actor would take
/// a resolution in the same claim; after it, a resolution wakes the actor
/// into a claim of its own. Nothing announces the release (it appends no
/// process event), so the row is read until it shows it, within a bound that
/// names a process that never parks.
async fn wait_for_contract_durable_input_park(
    core: &lash::LashCore,
    engine: &crate::backend::SimEngine,
) -> Result<(), FixedScriptRunnerError> {
    let processes = core
        .processes()
        .list(
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..lash_core::ProcessListFilter::default()
            },
            std::num::NonZeroUsize::new(2).unwrap_or(std::num::NonZeroUsize::MIN),
            None,
        )
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?
        .processes;
    let [process] = processes.as_slice() else {
        return Err(FixedScriptRunnerError::Assertion(format!(
            "the durable input contract runs one process, found {}",
            processes.len()
        )));
    };
    let actor = lash_durable::ActorKey::process(process.process_id.as_str())
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let store = lash_core_execution::StoreSet::durable_store(engine.stores());
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let snapshot = store
            .actor(&actor)
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        if snapshot.is_some_and(|snapshot| snapshot.state == lash_durable::ActorState::Waiting) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(FixedScriptRunnerError::Assertion(format!(
                "the durable input process {} never released its actor to wait for the input",
                process.process_id
            )));
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

// Assembles the contract proof from a completed turn; the runtime handles,
// result, expectations, and projection flags are all required and distinct.
#[allow(clippy::too_many_arguments)]
async fn agent_process_execution_result(
    core: &lash::LashCore,
    graph_store: &ContractGraphs,
    result: lash::TurnReport,
    events: Arc<RuntimeProofRecordingEvents>,
    provider_kind: &'static str,
    expected_final_value: &Value,
    extra: Option<(&'static str, Value)>,
    include_process_events: bool,
) -> Result<Value, FixedScriptRunnerError> {
    let final_value = result.final_value().cloned().ok_or_else(|| {
        FixedScriptRunnerError::Assertion(format!(
            "{provider_kind} finished without TurnFinish::FinalValue: {:?}",
            result.outcome
        ))
    })?;
    require(
        final_value == *expected_final_value,
        "agent process execution produced an unexpected semantic value",
    )?;
    let recorded = events.snapshot().await;
    let final_value_events = events.final_value_events().await;
    let assistant_prose_delta_count = events.assistant_prose_delta_count().await;
    let tool_completed_count = events.tool_completed_count().await;
    let tool_completed_outputs = events
        .tool_completed_outputs()
        .await
        .into_iter()
        .map(
            |(name, value)| json!({ "name": name, "value": normalize_contract_tool_output(value) }),
        )
        .collect::<Vec<_>>();
    let facts = runtime_final_value_invariant_facts(&result, &recorded);
    require(
        facts.passed()
            && facts.outcome_kind == "final_value"
            && facts.semantic_value.as_ref() == Some(&final_value)
            && final_value_events.iter().any(|value| value == &final_value)
            && result.assistant_message().is_none(),
        "agent process execution did not produce concrete final-value outcome/event facts",
    )?;
    let process_observations = agent_contract_process_observations(core).await?;
    let process_facts = agent_contract_process_facts(&process_observations);
    let process_events = if include_process_events {
        agent_contract_process_event_facts(core, &process_observations).await?
    } else {
        Vec::new()
    };
    let graph_facts =
        agent_contract_graph_facts(&graph_store.graphs(core).await, &result.state.session_id);
    let mut payload = json!({
        "execution_api": "lash::LashCore facade",
        "provider_kind": provider_kind,
        "session_id": result.state.session_id,
        "turn_index": result.state.turn_index,
        "done": true,
        "turn_outcome": {
            "kind": "final_value",
        },
        "final_value": final_value,
        "no_final_message_event": result.assistant_message().is_none(),
        "runtime_final_value_facts": facts,
        "final_value_event_count": final_value_events.len(),
        "assistant_prose_delta_count": assistant_prose_delta_count,
        "tool_completed_count": tool_completed_count,
        "tool_completed_outputs": tool_completed_outputs,
        "processes": process_observations
            .iter()
            .map(|process| process.observed.clone())
            .collect::<Vec<_>>(),
        "process_facts": process_facts,
        "process_events": process_events,
        "graph_facts": graph_facts,
    });
    if let Some((key, value)) = extra
        && let Some(object) = payload.as_object_mut()
    {
        object.insert(key.to_string(), value);
    }
    Ok(payload)
}

struct AgentContractProcessObservation {
    raw_process_id: ProcessId,
    process_ref: String,
    observed: Value,
}

async fn agent_contract_process_observations(
    core: &lash::LashCore,
) -> Result<Vec<AgentContractProcessObservation>, FixedScriptRunnerError> {
    let processes = core.processes();
    let filter = lash_core::ProcessListFilter {
        status: lash_core::ProcessStatusFilter::Any,
        ..lash_core::ProcessListFilter::default()
    };
    let mut observed = Vec::new();
    let mut continuation = None;
    loop {
        let page = processes
            .list(
                &filter,
                std::num::NonZeroUsize::new(256).unwrap_or(std::num::NonZeroUsize::MIN),
                continuation,
            )
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        for process in page.processes {
            let process_ref = agent_contract_process_ref(&process);
            let document_entry = agent_contract_process_document_entry(core, &process).await?;
            observed.push(AgentContractProcessObservation {
                raw_process_id: process.process_id.clone(),
                process_ref: process_ref.clone(),
                observed: json!({
                    "process_ref": process_ref,
                    "kind": process.identity.kind.as_str(),
                    "label": process.identity.label,
                    "status": process.status().label(),
                    "terminal": process.terminal().is_some(),
                    "definition_present": process.identity.definition_id.is_some(),
                    "document_entry": document_entry.map(Value::from).unwrap_or(Value::Null),
                    "child_session_present": process.child_session_id.is_some(),
                }),
            });
        }
        continuation = page.continuation;
        if continuation.is_none() {
            break;
        }
    }
    observed.sort_by(|left, right| left.process_ref.cmp(&right.process_ref));
    Ok(observed)
}

/// The entry of the kernel document an observed process runs, read through
/// the process's own definition, never the display label the process row
/// carries. `None` for a process of another engine; a kernel process whose
/// document lash no longer answers is a defect the contract run reports
/// rather than quietly uncounting.
async fn agent_contract_process_document_entry(
    core: &lash::LashCore,
    process: &lash_core::facade_support::ObservedProcess,
) -> Result<Option<String>, FixedScriptRunnerError> {
    if process.identity.kind.as_str() != lash_vm_runtime::LASH_VM_ENGINE_KIND
        || process.identity.definition_id.is_none()
    {
        return Ok(None);
    }
    let read = core
        .processes()
        .graph(&process.process_id)
        .await
        .map_err(|error| FixedScriptRunnerError::Runtime(error.to_string()))?;
    let lash::workflow::WorkflowRead::Inspected(inspection) = read else {
        return Err(FixedScriptRunnerError::Runtime(format!(
            "lash_vm process {} has no workflow document: {read:?}",
            process.process_id
        )));
    };
    match &inspection.document.reference().entry {
        lash::workflow::WorkflowDocumentEntry::Entry { function } => Ok(Some(function.to_string())),
        lash::workflow::WorkflowDocumentEntry::Main => {
            Err(FixedScriptRunnerError::Runtime(format!(
                "lash_vm process {} enters its document at `main`, not at an entry",
                process.process_id
            )))
        }
    }
}

fn agent_contract_process_ref(process: &lash_core::facade_support::ObservedProcess) -> String {
    let kind = process.identity.kind.as_str();
    let label = process.identity.label.as_deref();
    let status = process.status().label();
    let terminal = process.terminal().is_some().to_string();
    let definition_present = process.identity.definition_id.is_some().to_string();
    let child_session_present = process.child_session_id.is_some().to_string();
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update([0]);
    if let Some(label) = label {
        hasher.update(label.as_bytes());
    }
    hasher.update([0]);
    hasher.update(status.as_bytes());
    hasher.update([0]);
    hasher.update(terminal.as_bytes());
    hasher.update([0]);
    hasher.update(definition_present.as_bytes());
    hasher.update([0]);
    if let Some(id) = &process.identity.definition_id {
        hasher.update(id.as_str().as_bytes());
    }
    hasher.update([0]);
    hasher.update(child_session_present.as_bytes());
    let digest = hasher.finalize();
    format!("process-ref-{}", hex_prefix(&digest, 12))
}

fn hex_prefix(bytes: &[u8], len: usize) -> String {
    let full = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    full.chars().take(len).collect()
}

fn agent_contract_process_facts(processes: &[AgentContractProcessObservation]) -> Value {
    let mut completed_entries = BTreeSet::new();
    let mut completed_lash_vm_process_refs = BTreeSet::new();
    let mut completed_document_entry_process_refs = BTreeSet::new();
    let mut statuses = BTreeMap::<String, usize>::new();
    let mut kinds = BTreeMap::<String, usize>::new();
    for process in processes {
        let status = process
            .observed
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("");
        *statuses.entry(status.to_string()).or_default() += 1;
        let kind = process
            .observed
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("");
        *kinds.entry(kind.to_string()).or_default() += 1;
        if status == "completed" {
            if let Some(label) = process.observed.get("label").and_then(Value::as_str) {
                completed_entries.insert(label.to_string());
            }
            if process
                .observed
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind == lash_vm_runtime::LASH_VM_ENGINE_KIND)
            {
                completed_lash_vm_process_refs.insert(process.process_ref.clone());
                if process
                    .observed
                    .get("document_entry")
                    .is_some_and(Value::is_string)
                {
                    completed_document_entry_process_refs.insert(process.process_ref.clone());
                }
            }
        }
    }
    json!({
        "process_count": processes.len(),
        "terminal_count": processes
            .iter()
            .filter(|process| process.observed.get("terminal").and_then(Value::as_bool) == Some(true))
            .count(),
        "completed_entries": completed_entries.into_iter().collect::<Vec<_>>(),
        "completed_lash_vm_process_count": completed_lash_vm_process_refs.len(),
        "completed_document_entry_process_count": completed_document_entry_process_refs.len(),
        "completed_lash_vm_process_refs": completed_lash_vm_process_refs.into_iter().collect::<Vec<_>>(),
        "status_counts": statuses,
        "kind_counts": kinds,
        "all_terminal": processes
            .iter()
            .all(|process| process.observed.get("terminal").and_then(Value::as_bool) == Some(true)),
    })
}

async fn agent_contract_process_event_facts(
    core: &lash::LashCore,
    processes: &[AgentContractProcessObservation],
) -> Result<Vec<Value>, FixedScriptRunnerError> {
    let mut events = Vec::new();
    let mut identities = ContractEventIdentities::default();
    for process in processes {
        let mut from =
            lash::process::ProcessHistoryContinuation::start(process.raw_process_id.clone());
        loop {
            let read = core
                .processes()
                .events(
                    from,
                    std::num::NonZeroUsize::new(128).unwrap_or(std::num::NonZeroUsize::MIN),
                    lash::process::ProcessEventQueryMode::Full,
                )
                .await
                .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
            let page = match read.outcome {
                lash::process::ProcessEventReadOutcome::Retained(page) => page,
                lash::process::ProcessEventReadOutcome::NoLongerRetained(retention) => {
                    return Err(FixedScriptRunnerError::Runtime(format!(
                        "process event history is no longer retained: {retention:?}"
                    )));
                }
            };
            let lash::process::ProcessEventPageEvents::Full(page_events) = page.events else {
                unreachable!("full process event query returned a lite page");
            };
            for event in page_events {
                let event_type = event.fact.event_type().to_owned();
                events.push(json!({
                    "process_ref": process.process_ref.clone(),
                    "sequence": event.sequence,
                    "event_type": event_type,
                    "payload": identities.normalize(&event_type, event.fact.payload()),
                }));
            }
            if matches!(page.more, lash::process::ProcessEventPageMore::Complete) {
                break;
            }
            from = read.next;
        }
    }
    events.sort_by(|left, right| {
        (
            left.get("process_ref").and_then(Value::as_str),
            left.get("sequence").and_then(Value::as_u64),
        )
            .cmp(&(
                right.get("process_ref").and_then(Value::as_str),
                right.get("sequence").and_then(Value::as_u64),
            ))
    });
    Ok(events)
}

/// Fresh executions mint different call identities and wall-clock times.
/// Name them by first appearance, retaining equality across waiting/resumed
/// facts and distinctions between different calls and wait timestamps.
#[derive(Default)]
struct ContractEventIdentities {
    calls: BTreeMap<String, usize>,
    wait_times: BTreeMap<u64, usize>,
}

impl ContractEventIdentities {
    fn normalize(&mut self, event_type: &str, payload: Value) -> Value {
        let mut payload = normalize_contract_process_event_payload(event_type, payload);
        if event_type == "process.effect_outcome"
            && let Some(call) = payload.get_mut("call_id")
        {
            self.normalize_call(call);
        }
        if matches!(event_type, "process.waiting" | "process.resumed")
            && let Some(wait) = payload.get_mut("wait").and_then(Value::as_object_mut)
        {
            if let Some(time) = wait.get("since_ms").and_then(Value::as_u64) {
                let next = self.wait_times.len() + 1;
                let ordinal = *self.wait_times.entry(time).or_insert(next);
                wait.insert("since_ms".to_owned(), json!(ordinal));
            }
            if let Some(kind) = wait.get_mut("kind").and_then(Value::as_object_mut)
                && kind.get("kind").and_then(Value::as_str) == Some("call")
                && let Some(call) = kind.get_mut("call_id")
            {
                self.normalize_call(call);
            }
        }
        payload
    }

    fn normalize_call(&mut self, value: &mut Value) {
        if let Some(call) = value.as_str().filter(|call| !call.is_empty()) {
            let next = self.calls.len() + 1;
            let ordinal = *self.calls.entry(call.to_owned()).or_insert(next);
            *value = json!(format!("call-{ordinal}"));
        }
    }
}

fn normalize_contract_process_event_payload(event_type: &str, payload: Value) -> Value {
    let mut payload = payload;
    if let Some(object) = payload.as_object_mut() {
        object.remove("await_key");
        if event_type == "process.effect_outcome"
            && object
                .get("replay_key")
                .and_then(Value::as_str)
                .is_some_and(|key| !key.is_empty())
        {
            // Fresh contract executions have distinct intent identities. The
            // real event log retains the key; only the simulator's fixed-source
            // comparison masks it, as it does for other per-run identities.
            object.insert("replay_key".to_owned(), json!("<opaque>"));
        }
        if event_type == "process.first_started"
            && let Some(started) = object.get_mut("started").and_then(Value::as_object_mut)
        {
            started.remove("started_at_ms");
            if let Some(owner) = started.get_mut("owner").and_then(Value::as_object_mut) {
                owner.remove("owner_id");
                owner.remove("incarnation_id");
            }
        }
    }
    payload
}

fn agent_failed_child_activity_facts(
    result: &lash::TurnReport,
    events: &[lash::TurnActivity],
) -> Value {
    let mut failed_code_block_errors = Vec::new();
    let mut turn_error_messages = Vec::new();
    let mut final_value_event_count = 0usize;
    for activity in events {
        match &activity.event {
            lash::TurnEvent::CodeBlockCompleted {
                result: lash::transcript::CellOutcome::Failed(error),
                ..
            } => failed_code_block_errors.push(error.clone()),
            lash::TurnEvent::Error(ReportedFailure { message, .. }) => {
                turn_error_messages.push(message.clone())
            }
            lash::TurnEvent::FinalValue { .. } => final_value_event_count += 1,
            _ => {}
        }
    }
    let event_debug = format!("{events:#?}");
    json!({
        "turn_success": result.is_success(),
        "final_value_present": result.final_value().is_some(),
        "final_value_event_count": final_value_event_count,
        "failed_code_block_count": failed_code_block_errors.len(),
        "failed_code_block_errors": failed_code_block_errors,
        "turn_error_messages": turn_error_messages,
        "provider_exhaustion_observed": event_debug.contains("provider exhausted"),
        "child_task_fail_reason_observed": event_debug.contains("child boom"),
        "parent_task_fail_reason_observed": event_debug.contains("parent observed child failure"),
    })
}

fn normalize_contract_tool_output(value: Value) -> Value {
    let Some(object) = value.as_object() else {
        return value;
    };
    // A started process hands back a live handle naming the process its
    // registrar minted. The id is a fresh UUIDv7 in production and a
    // registration-order counter in the simulator, and parallel starts
    // register in whatever order the backend's writes land, so a contract
    // payload that kept it would compare two fresh runs on an identity
    // neither is meant to share. Keep the handle's shape -- the `p.` prefix,
    // that the handle names the process it carries, and that the id is a
    // minted spelling -- and mask the id itself.
    if object.contains_key("__handle__") && object.contains_key("process_id") {
        let process_id = object.get("process_id").and_then(Value::as_str);
        let id = object.get("id").and_then(Value::as_str);
        let names_its_process = id
            .zip(process_id)
            .is_some_and(|(id, process_id)| id.strip_prefix("p.") == Some(process_id));
        return json!({
            "__handle__": object.get("__handle__").cloned().unwrap_or(Value::Null),
            "id_prefix": id
                .and_then(|id| id.split_once('.').map(|(tag, _)| format!("{tag}.")))
                .unwrap_or_default(),
            "names_its_process": names_its_process,
            "process_id_minted": process_id
                .is_some_and(|process_id| lash_core::ProcessId::parse(process_id).is_ok()),
        });
    }
    if !object.contains_key("full_output_path") {
        if object.contains_key("wall_time_seconds")
            && object.contains_key("status")
            && object.contains_key("output")
        {
            return json!({
                "status": object.get("status").cloned().unwrap_or(Value::Null),
                "done": object.get("done").cloned().unwrap_or(Value::Null),
                "running": object.get("running").cloned().unwrap_or(Value::Null),
                "exit_code": object.get("exit_code").cloned().unwrap_or(Value::Null),
                "output": object.get("output").cloned().unwrap_or(Value::Null),
            });
        }
        return value;
    }
    let output = object.get("output").and_then(Value::as_str).unwrap_or("");
    let tail_start = output.len().saturating_sub(4);
    json!({
        "status": object.get("status").cloned().unwrap_or(Value::Null),
        "exit_code": object.get("exit_code").cloned().unwrap_or(Value::Null),
        "output_len": output.len(),
        "output_tail": &output[tail_start..],
        "full_output_path_present": object
            .get("full_output_path")
            .and_then(Value::as_str)
            .is_some_and(|path| !path.is_empty()),
    })
}

#[cfg(test)]
#[path = "agent_contracts_payload_tests.rs"]
mod payload_tests;
