use super::*;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;

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

fn contract_store_factory(clock: Arc<dyn lash_core::Clock>) -> Arc<dyn SessionStoreFactory> {
    let inner: Arc<dyn SessionStoreFactory> = Arc::new(
        lash::persistence::InMemorySessionStoreFactory::with_clock(clock),
    );
    CONTRACT_CHECKPOINT_COLLECTOR.with(|slot| {
        slot.borrow().as_ref().map_or(inner.clone(), |collector| {
            Arc::new(ObservedSessionStoreFactory::new(inner, collector.clone()))
                as Arc<dyn SessionStoreFactory>
        })
    })
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
    "agent.started_process_subagent_spawn",
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
        semantic_oracle: "agent.started_process_subagent_spawn",
        source_path: "crates/lash/src/tests/agent_scenarios/cases.rs",
        source_scenario: "agent_scenario_started_process_labeled_subagent_spawn",
        anchor: FixedContractAnchor::ProviderActor,
        execute: run_agent_started_process_subagent_spawn,
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

fn run_agent_started_process_subagent_spawn(
    runtime: &tokio::runtime::Runtime,
) -> Result<Value, FixedScriptRunnerError> {
    runtime.block_on(agent_started_process_subagent_spawn_execution())
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
/** @label Lookup app state */
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

async fn agent_started_process_tool_call_graph_execution() -> Result<Value, FixedScriptRunnerError>
{
    let expected = json!({ "ok": true });
    let result = facade_agent_process_execution(
        "lash_runtime agent started process tool",
        &SessionId::from("sim-agent-started-process-tool-contract"),
        "Start a process that calls the app lookup tool.",
        vec![
            r#"<typescript>
const lookup = async () => {
  /** @label Lookup app state in process */
  const value = await tools.app_lookup({});
  return value;
};
const handle = await processes.start({ definition: lookup });
const result = await handle;
finish(result);
</typescript>"#,
        ],
        &expected,
        Some(Arc::new(ContractAppTools) as Arc<dyn lash_core::ToolProvider>),
    )
    .await?;
    Ok(result)
}

async fn agent_durable_input_suspension_resolution_execution()
-> Result<Value, FixedScriptRunnerError> {
    let result = facade_agent_durable_input_execution().await?;
    Ok(result)
}

async fn agent_nested_process_start_await_execution() -> Result<Value, FixedScriptRunnerError> {
    let expected = json!({ "parent": "done" });
    let result = facade_agent_process_execution(
        "lash_runtime agent nested process",
        &SessionId::from("sim-agent-nested-process-contract"),
        "Start a parent process that starts and awaits a child process.",
        vec![
            r#"<typescript>
const child = async () => {
  return { child: "done" };
};
const parent = async () => {
  /** @label Start nested child process */
  const inner = await (await processes.start({ definition: child }));
  return { parent: inner.child };
};
const handle = await processes.start({ definition: parent });
const result = await handle;
finish(result);
</typescript>"#,
        ],
        &expected,
        None,
    )
    .await?;
    Ok(result)
}

async fn agent_started_process_subagent_spawn_execution() -> Result<Value, FixedScriptRunnerError> {
    let expected = json!({ "len": 2 });
    let result = facade_agent_process_execution_with_options(
        "lash_runtime agent started process subagent",
        &SessionId::from("sim-agent-started-process-subagent-contract"),
        "Run a Lashlang process that spawns a subagent and returns its value.",
        vec![
            r#"<typescript>
const spawnChild = async () => {
  /** @label Spawn subagent with web search */
  const result = await agents.spawn({
    capability: "default",
    task: "Finish `{ len: chunk.length }` using the seeded `chunk` variable.",
    seed: { chunk: ["a", "b"] },
    output: { len: "int" }
  });
  return result;
};
const handle = await processes.start({ definition: spawnChild });
const result = await handle;
finish(result);
</typescript>"#,
            r#"<typescript>
finish({ len: chunk.length });
</typescript>"#,
        ],
        &expected,
        None,
        true,
        None,
    )
    .await?;
    Ok(result)
}

async fn agent_session_turn_process_child_execution() -> Result<Value, FixedScriptRunnerError> {
    let expected = json!({ "child": "done" });
    let result = facade_final_value_execution_with_process_surface(
        "lash_runtime agent session-turn process child",
        &SessionId::from("sim-agent-session-turn-process-child-contract"),
        "Start a child process and await its result.",
        r#"<typescript>
const child = async () => {
  return { child: "done" };
};
const handle = await processes.start({ definition: child });
const result = await handle;
finish(result);
</typescript>"#,
        &expected,
    )
    .await?;
    Ok(result)
}

async fn agent_failed_child_preserves_failure_graph_execution()
-> Result<Value, FixedScriptRunnerError> {
    let (core, graph_store) = agent_process_contract_core_with_options(
        "lash_runtime agent failed child graph",
        vec![
            r#"<typescript>
/** @label Spawn failing subagent */
const result = await agents.spawn({
  capability: "default",
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
        Some(1),
    )?;
    let session = core
        .session("sim-agent-failed-child-contract")
        .open()
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let events = Arc::new(RuntimeProofRecordingEvents::default());
    let result = session
        .turn(lash::TurnInput::text(
            "Spawn a child that fails and preserve its execution graph.",
        ))
        .stream_to(events.as_ref())
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    session
        .refresh_background_graph()
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let recorded = events.snapshot().await;
    let process_observations = agent_contract_process_observations(&core).await?;
    let process_facts = agent_contract_process_facts(&process_observations);
    let graph_facts = agent_contract_graph_facts(&graph_store.graphs(), &result.state.session_id);
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

async fn agent_parallel_spawn_and_join_execution() -> Result<Value, FixedScriptRunnerError> {
    let expected = json!({ "joined": ["left", "right"] });
    let result = facade_final_value_execution_with_process_surface(
        "lash_runtime agent parallel process join",
        &SessionId::from("sim-agent-parallel-spawn-join-contract"),
        "Start two processes, await both, and finish their joined result.",
        r#"<typescript>
const child = async (value) => {
  return value;
};
/** @label Start left process */
const left = await processes.start({ definition: child, args: { value: "left" } });
/** @label Start right process */
const right = await processes.start({ definition: child, args: { value: "right" } });
const leftValue = await left;
const rightValue = await right;
finish({ joined: [leftValue, rightValue] });
</typescript>"#,
        &expected,
    )
    .await?;
    Ok(result)
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
    let clock = crate::clock::SimClock::new();
    let events = Arc::new(RuntimeProofRecordingEvents::default());
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let mut builder = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .with_native_queued_work()
        .effect_host(Arc::new(
            lash::durability::NativeEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        .lease_timings(crate::lease::sim_runtime_lease_timings())
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(contract_store_factory(clock.clone()))
        .clock(clock.clone())
        .process_registry(
            Arc::new(lash_core::TestLocalProcessRegistry::default().with_clock(clock))
                as Arc<dyn lash_core::ProcessRegistry>,
        )
        .provider(fixed_texts_provider(provider_kind, provider_responses))
        .model(
            lash_core::ModelSpec::builder(provider_kind)
                .context_window_tokens(200_000)
                .build()
                .map_err(|error| FixedScriptRunnerError::Assertion(error.to_string()))?,
        );
    if let Some(tools) = tools {
        builder = builder.tools(tools);
    }
    if process_surface == ProcessSurface::Installed {
        builder = builder.plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(),
        ));
    }
    let core = builder
        .build(crate::sim_process_owner())
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let session = core
        .session(session_id)
        .open()
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let result = session
        .turn(lash::TurnInput::text(prompt))
        .require_finish()
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?
        .stream_to(events.as_ref())
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
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
    install_subagents: bool,
    max_turns: Option<usize>,
) -> Result<Value, FixedScriptRunnerError> {
    let (core, graph_store) = agent_process_contract_core_with_options(
        provider_kind,
        provider_responses,
        tools,
        install_subagents,
        max_turns,
    )?;
    let session = core
        .session(session_id)
        .open()
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let events = Arc::new(RuntimeProofRecordingEvents::default());
    let result = session
        .turn(lash::TurnInput::text(prompt))
        .stream_to(events.as_ref())
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
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

async fn facade_agent_durable_input_execution() -> Result<Value, FixedScriptRunnerError> {
    let (key_tx, mut key_rx) =
        tokio::sync::oneshot::channel::<Result<lash_core::AwaitEventKey, String>>();
    let tools = Arc::new(ContractDurableInputTools::new(key_tx));
    facade_agent_durable_input_execution_with(
        Arc::clone(&tools),
        tools as Arc<dyn lash_core::ToolProvider>,
        Arc::new(
            lash::durability::NativeEffectHost::default().allow_process_lifetime_completion_keys(),
        ),
        &mut key_rx,
    )
    .await
}

async fn facade_agent_durable_input_execution_with(
    tools: Arc<ContractDurableInputTools>,
    registered_tools: Arc<dyn lash_core::ToolProvider>,
    effect_host: Arc<dyn lash_core::EffectHost>,
    key_rx: &mut tokio::sync::oneshot::Receiver<Result<lash_core::AwaitEventKey, String>>,
) -> Result<Value, FixedScriptRunnerError> {
    let (core, graph_store) = agent_process_contract_core_with_effect_host(
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
        effect_host,
    )?;
    let session = core
        .session("sim-agent-durable-input-contract")
        .open()
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    let events = Arc::new(RuntimeProofRecordingEvents::default());
    let turn_session = session.clone();
    let turn_events = Arc::clone(&events);
    let turn = tokio::spawn(async move {
        turn_session
            .turn(lash::TurnInput::text(
                "Start a process that asks for durable input.",
            ))
            .stream_to(turn_events.as_ref())
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))
    });
    let key = wait_for_contract_durable_input_key(key_rx).await?;
    let completed_before_resolution = events.tool_completed_count().await;
    let suspended_before_resolution = !turn.is_finished() && completed_before_resolution == 0;
    let await_tool_call_id_present = match &key.wait {
        lash_core::AwaitEventWaitIdentity::ToolCompletion { tool_call_id } => {
            !tool_call_id.is_empty()
        }
        other => {
            return Err(FixedScriptRunnerError::Assertion(format!(
                "durable input used non-tool-completion await key `{other:?}`"
            )));
        }
    };
    let resolve_outcome = core
        .completions()
        .resolve(
            key,
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
        "resolve_accepted": matches!(resolve_outcome, lash_core::ResolveOutcome::Accepted),
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

fn agent_process_contract_core_with_effect_host(
    provider_kind: &'static str,
    provider_responses: Vec<&'static str>,
    tools: Option<Arc<dyn lash_core::ToolProvider>>,
    effect_host: Arc<dyn lash_core::EffectHost>,
) -> Result<(lash::LashCore, Arc<lash::tracing::TraceLashlangGraphStore>), FixedScriptRunnerError> {
    agent_process_contract_core_with_options_and_effect_host(
        provider_kind,
        provider_responses,
        tools,
        false,
        None,
        effect_host,
    )
}

fn agent_process_contract_core_with_options(
    provider_kind: &'static str,
    provider_responses: Vec<&'static str>,
    tools: Option<Arc<dyn lash_core::ToolProvider>>,
    install_subagents: bool,
    max_turns: Option<usize>,
) -> Result<(lash::LashCore, Arc<lash::tracing::TraceLashlangGraphStore>), FixedScriptRunnerError> {
    agent_process_contract_core_with_options_and_effect_host(
        provider_kind,
        provider_responses,
        tools,
        install_subagents,
        max_turns,
        Arc::new(
            lash::durability::NativeEffectHost::default().allow_process_lifetime_completion_keys(),
        ),
    )
}

// Full specification of the simulator's facade-level process harness. The
// effect host is injectable so boundary tests can observe the same production
// execution path without creating a parallel runner.
#[allow(clippy::too_many_arguments)]
fn agent_process_contract_core_with_options_and_effect_host(
    provider_kind: &'static str,
    provider_responses: Vec<&'static str>,
    tools: Option<Arc<dyn lash_core::ToolProvider>>,
    install_subagents: bool,
    max_turns: Option<usize>,
    effect_host: Arc<dyn lash_core::EffectHost>,
) -> Result<(lash::LashCore, Arc<lash::tracing::TraceLashlangGraphStore>), FixedScriptRunnerError> {
    let clock = crate::clock::SimClock::new();
    let graph_store = Arc::new(lash::tracing::TraceLashlangGraphStore::default());
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    )
    .with_lashlang_execution_sink(Arc::clone(&graph_store) as Arc<dyn lash::tracing::TraceSink>);
    let mut builder = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        // The process surface is rendered from the tool catalogue, so a host that
        // wants `processes.*` inside a cell installs the plugin that supplies it.
        // Without it every fixed process contract's first cell dies on
        // "unknown module `processes`" -- the mirrored facade agent scenarios
        // install `SessionProcessAdminPluginFactory` for exactly this reason.
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(),
        ))
        .with_native_queued_work()
        .effect_host(effect_host)
        .lease_timings(crate::lease::sim_runtime_lease_timings())
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(contract_store_factory(clock.clone()))
        .clock(clock.clone())
        .process_registry(
            Arc::new(lash_core::TestLocalProcessRegistry::default().with_clock(clock))
                as Arc<dyn lash_core::ProcessRegistry>,
        )
        .provider(fixed_texts_provider(provider_kind, provider_responses))
        .model(
            lash_core::ModelSpec::builder(provider_kind)
                .context_window_tokens(200_000)
                .build()
                .map_err(|error| FixedScriptRunnerError::Assertion(error.to_string()))?,
        );
    if let Some(tools) = tools {
        builder = builder.tools(tools);
    }
    if install_subagents {
        builder = builder.plugin(agent_contract_subagents_plugin());
    }
    if let Some(max_turns) = max_turns {
        builder = builder.turn_budget(lash::TurnBudget::bounded(max_turns));
    }
    let core = builder
        .build(crate::sim_process_owner())
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
    Ok((core, graph_store))
}

fn agent_contract_subagents_plugin() -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(lash_subagents::SubagentsPluginFactory::new(Arc::new(
        lash_subagents::CapabilityRegistry::new().with(Arc::new(
            lash_subagents::StaticCapability::new(
                "default",
                lash_core::facade_support::SessionSpec::inherit(),
            ),
        )),
    )))
}

async fn wait_for_contract_durable_input_key(
    key_rx: &mut tokio::sync::oneshot::Receiver<Result<lash_core::AwaitEventKey, String>>,
) -> Result<lash_core::AwaitEventKey, FixedScriptRunnerError> {
    match key_rx.await {
        Ok(Ok(key)) => Ok(key),
        Ok(Err(err)) => Err(FixedScriptRunnerError::Runtime(err)),
        Err(_) => Err(FixedScriptRunnerError::Assertion(
            "durable input tool dropped await-key sender".to_string(),
        )),
    }
}

// Assembles the contract proof from a completed turn; the runtime handles,
// result, expectations, and projection flags are all required and distinct.
#[allow(clippy::too_many_arguments)]
async fn agent_process_execution_result(
    core: &lash::LashCore,
    graph_store: &lash::tracing::TraceLashlangGraphStore,
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
    let graph_facts = agent_contract_graph_facts(&graph_store.graphs(), &result.state.session_id);
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
    let mut observed = core
        .processes()
        .list(&lash_core::ProcessListFilter {
            definition: None,
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        })
        .await
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?
        .into_iter()
        .map(|process| {
            let process_ref = agent_contract_process_ref(&process);
            AgentContractProcessObservation {
                raw_process_id: process.process_id.clone(),
                process_ref: process_ref.clone(),
                observed: json!({
                    "process_ref": process_ref,
                    "kind": process.kind(),
                    "label": process.label(),
                    "status": process.lifecycle.label(),
                    "terminal": process.terminal(),
                    "definition_present": process.identity.definition.is_some(),
                    "child_session_present": process.child_session_id.is_some(),
                }),
            }
        })
        .collect::<Vec<_>>();
    observed.sort_by(|left, right| left.process_ref.cmp(&right.process_ref));
    Ok(observed)
}

fn agent_contract_process_ref(process: &lash_core::facade_support::ObservedProcess) -> String {
    let kind = process.kind();
    let label = process.label();
    let status = process.lifecycle.label();
    let terminal = process.terminal().to_string();
    let definition_present = process.identity.definition.is_some().to_string();
    let child_session_present = process.child_session_id.is_some().to_string();
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update([0]);
    hasher.update(label.as_bytes());
    hasher.update([0]);
    hasher.update(status.as_bytes());
    hasher.update([0]);
    hasher.update(terminal.as_bytes());
    hasher.update([0]);
    hasher.update(definition_present.as_bytes());
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
    let mut completed_lashlang_process_refs = BTreeSet::new();
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
                .is_some_and(|kind| kind == lash_lashlang_runtime::LASHLANG_ENGINE_KIND)
            {
                completed_lashlang_process_refs.insert(process.process_ref.clone());
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
        "completed_lashlang_process_count": completed_lashlang_process_refs.len(),
        "completed_lashlang_process_refs": completed_lashlang_process_refs.into_iter().collect::<Vec<_>>(),
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
    for process in processes {
        for event in core
            .processes()
            .events(&process.raw_process_id, 0)
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?
        {
            let event_type = event.event_type;
            events.push(json!({
                "process_ref": process.process_ref.clone(),
                "sequence": event.sequence,
                "event_type": event_type,
                "payload": normalize_contract_process_event_payload(&event_type, event.payload),
            }));
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

fn normalize_contract_process_event_payload(event_type: &str, payload: Value) -> Value {
    let mut payload = payload;
    if let Some(object) = payload.as_object_mut() {
        object.remove("await_key_id");
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

fn agent_contract_graph_facts(
    graphs: &[lash::tracing::TraceLashlangGraph],
    root_session_id: &SessionId,
) -> Value {
    let mut completed_process_entries = BTreeSet::new();
    let mut completed_labeled_resources = BTreeSet::new();
    let mut failed_labeled_resources = BTreeSet::new();
    let mut completed_labeled_nodes = BTreeSet::new();
    let mut child_links = BTreeSet::new();
    let mut graph_status_counts = BTreeMap::<String, usize>::new();
    let mut child_session_exec_completed_count = 0usize;
    let mut child_session_exec_failed_count = 0usize;
    for graph in graphs {
        *graph_status_counts
            .entry(trace_lashlang_status_label(graph.status).to_string())
            .or_default() += 1;
        if graph.scope.session_id.as_ref() != Some(root_session_id)
            && matches!(
                &graph.subject,
                lash::tracing::TraceRuntimeSubject::Effect { .. }
            )
        {
            match graph.status {
                lash::tracing::TraceLanguageExecutionStatus::Completed => {
                    child_session_exec_completed_count += 1;
                }
                lash::tracing::TraceLanguageExecutionStatus::Failed => {
                    child_session_exec_failed_count += 1;
                }
                _ => {}
            }
        }
        if graph.entry_kind == "process"
            && matches!(
                graph.subject,
                lash::tracing::TraceRuntimeSubject::Process { .. }
            )
            && graph.status == lash::tracing::TraceLanguageExecutionStatus::Completed
        {
            completed_process_entries.insert(graph.entry_name.clone());
        }
        for node in &graph.nodes {
            let title = node
                .label_metadata
                .as_ref()
                .map(|label| label.title.as_str());
            if node.kind == "resource_operation"
                && matches!(
                    &node.observation,
                    lash::tracing::TraceLashlangNodeObservation::Completed { .. }
                )
                && let Some(title) = title
            {
                completed_labeled_resources.insert(title.to_string());
            }
            if node.kind == "resource_operation"
                && matches!(
                    &node.observation,
                    lash::tracing::TraceLashlangNodeObservation::Failed { .. }
                )
                && let Some(title) = title
            {
                failed_labeled_resources.insert(title.to_string());
            }
            if matches!(
                &node.observation,
                lash::tracing::TraceLashlangNodeObservation::Completed { .. }
            ) && let Some(title) = title
            {
                completed_labeled_nodes.insert(title.to_string());
            }
        }
        for child in &graph.children {
            child_links.insert(format!(
                "{}->{}",
                graph.entry_name,
                child.child_entry_name.as_deref().unwrap_or("<unknown>")
            ));
        }
    }
    json!({
        "graph_count": graphs.len(),
        "status_counts": graph_status_counts,
        "completed_process_entries": completed_process_entries.into_iter().collect::<Vec<_>>(),
        "completed_labeled_resources": completed_labeled_resources.into_iter().collect::<Vec<_>>(),
        "failed_labeled_resources": failed_labeled_resources.into_iter().collect::<Vec<_>>(),
        "completed_labeled_nodes": completed_labeled_nodes.into_iter().collect::<Vec<_>>(),
        "child_links": child_links.into_iter().collect::<Vec<_>>(),
        "child_session_exec_completed_count": child_session_exec_completed_count,
        "child_session_exec_failed_count": child_session_exec_failed_count,
    })
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
                success: false,
                error: Some(error),
                ..
            } => failed_code_block_errors.push(error.clone()),
            lash::TurnEvent::Error { message } => turn_error_messages.push(message.clone()),
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

fn trace_lashlang_status_label(
    status: lash::tracing::TraceLanguageExecutionStatus,
) -> &'static str {
    match status {
        lash::tracing::TraceLanguageExecutionStatus::Running => "running",
        lash::tracing::TraceLanguageExecutionStatus::Completed => "completed",
        lash::tracing::TraceLanguageExecutionStatus::Failed => "failed",
        lash::tracing::TraceLanguageExecutionStatus::Cancelled => "cancelled",
    }
}

fn normalize_contract_tool_output(value: Value) -> Value {
    let Some(object) = value.as_object() else {
        return value;
    };
    // A started process hands back a live handle whose id carries the turn's
    // tool-intent replay key. That key is stable across a replay of one turn
    // and deliberately distinct between two independent executions, so a
    // contract payload that kept the digest would compare two fresh runs on an
    // identity neither is meant to share. Keep the handle's shape -- the
    // sequence prefix and the replay-key scheme -- and mask the digest, the
    // same way process refs and labels already travel as masked hashes.
    if object.contains_key("__handle__") && object.contains_key("process_id") {
        let process_id = object.get("process_id").and_then(Value::as_str);
        let id = object.get("id").and_then(Value::as_str);
        return json!({
            "__handle__": object.get("__handle__").cloned().unwrap_or(Value::Null),
            "id_prefix": id
                .and_then(|id| process_id.and_then(|key| id.strip_suffix(key)))
                .unwrap_or_default(),
            "process_id_scheme": process_id
                .and_then(|key| key.rsplit_once(':').map(|(scheme, _)| scheme))
                .unwrap_or_default(),
            "process_id_digest_present": process_id
                .and_then(|key| key.rsplit_once(':').map(|(_, digest)| !digest.is_empty()))
                .unwrap_or(false),
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
#[path = "agent_contracts_effect_boundary_tests.rs"]
mod effect_boundary_tests;
