use super::super::*;
use super::contracts::{
    GraphContract, assert_all_processes_terminal, assert_completed_process_graph,
    assert_labeled_node, assert_labeled_resource_operation,
    assert_min_completed_child_session_exec_graphs, assert_min_completed_process_graphs,
    assert_no_duplicate_label_step, assert_session_turn_child_graph,
    assert_successful_agent_scenario,
};
use lash_core::llm::types::LlmUsage;
use lash_sansio::sync::MutexExt;
use std::collections::VecDeque;

#[derive(Default)]
pub(super) struct AgentScenarioExpectations {
    pub(super) labeled_resource_titles: Vec<&'static str>,
    pub(super) labeled_node_titles: Vec<&'static str>,
    pub(super) completed_process_entries: Vec<&'static str>,
    pub(super) min_completed_child_session_exec_graphs: usize,
    pub(super) min_completed_process_graphs: usize,
}

pub(super) struct AgentScenario {
    pub(super) name: &'static str,
    pub(super) session_id: String,
    pub(super) scripted_provider_responses: Vec<String>,
    pub(super) scripted_provider_usage: LlmUsage,
    pub(super) root_prompt: &'static str,
    pub(super) expected_final_value: Option<serde_json::Value>,
    pub(super) tool_provider: Option<Arc<dyn ToolProvider>>,
    pub(super) install_subagents: bool,
    pub(super) install_shell_processes: bool,
    pub(super) max_turns: Option<usize>,
    pub(super) expected_contracts: AgentScenarioExpectations,
}

impl AgentScenario {
    pub(super) fn new(name: &'static str, root_prompt: &'static str) -> Self {
        Self {
            name,
            session_id: agent_scenario_session_id(name),
            scripted_provider_responses: Vec::new(),
            scripted_provider_usage: LlmUsage::default(),
            root_prompt,
            expected_final_value: None,
            tool_provider: None,
            install_subagents: false,
            install_shell_processes: false,
            max_turns: None,
            expected_contracts: AgentScenarioExpectations::default(),
        }
    }

    pub(super) fn response(mut self, response: impl Into<String>) -> Self {
        self.scripted_provider_responses.push(response.into());
        self
    }

    pub(super) fn responses<I, S>(mut self, responses: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.scripted_provider_responses = responses.into_iter().map(Into::into).collect();
        self
    }

    pub(super) fn expected_final_value(mut self, value: serde_json::Value) -> Self {
        self.expected_final_value = Some(value);
        self
    }

    pub(super) fn response_usage(mut self, usage: LlmUsage) -> Self {
        self.scripted_provider_usage = usage;
        self
    }

    pub(super) fn tool_provider(mut self, tool_provider: Arc<dyn ToolProvider>) -> Self {
        self.tool_provider = Some(tool_provider);
        self
    }

    pub(super) fn install_subagents(mut self) -> Self {
        self.install_subagents = true;
        self
    }

    pub(super) fn install_shell_processes(mut self) -> Self {
        self.install_shell_processes = true;
        self
    }

    pub(super) fn max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = Some(max_turns);
        self
    }

    pub(super) fn labeled_resource(mut self, title: &'static str) -> Self {
        self.expected_contracts.labeled_resource_titles.push(title);
        self
    }

    pub(super) fn labeled_node(mut self, title: &'static str) -> Self {
        self.expected_contracts.labeled_node_titles.push(title);
        self
    }

    pub(super) fn completed_process(mut self, entry_name: &'static str) -> Self {
        self.expected_contracts
            .completed_process_entries
            .push(entry_name);
        self
    }

    pub(super) fn min_completed_process_graphs(mut self, count: usize) -> Self {
        self.expected_contracts.min_completed_process_graphs = count;
        self
    }

    pub(super) fn min_completed_child_session_exec_graphs(mut self, count: usize) -> Self {
        self.expected_contracts
            .min_completed_child_session_exec_graphs = count;
        self
    }
}

fn agent_scenario_session_id(name: &str) -> String {
    let mut slug = String::from("agent-scenario-");
    let mut previous_dash = true;
    for byte in name.bytes() {
        let next = if byte.is_ascii_alphanumeric() {
            previous_dash = false;
            Some(byte.to_ascii_lowercase() as char)
        } else if !previous_dash {
            previous_dash = true;
            Some('-')
        } else {
            None
        };
        if let Some(ch) = next {
            slug.push(ch);
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

pub(super) struct AgentScenarioRun {
    pub(super) session_id: String,
    pub(super) turn_output: Option<TurnResult>,
    pub(super) streamed_events: Vec<TurnActivity>,
    pub(super) graph_snapshots: Vec<crate::tracing::TraceLashlangGraph>,
    pub(super) prompt_captures: Vec<LlmRequest>,
    pub(super) final_process_list: Vec<lash_core::ProcessHandleSummary>,
    /// Runtime-checkpoint commits the session store actually accepted, in
    /// commit order. Observed at the store seam, never reconstructed.
    pub(super) checkpoint_writes:
        Vec<lash_core::testing::checkpoint_observer::CheckpointWriteEvent>,
}

struct AgentScenarioSetup {
    scripted_provider_responses: Vec<String>,
    scripted_provider_usage: LlmUsage,
    tool_provider: Option<Arc<dyn ToolProvider>>,
    install_subagents: bool,
    install_shell_processes: bool,
    install_llm_tools: bool,
    max_turns: Option<usize>,
}

impl AgentScenarioSetup {
    fn new(scripted_provider_responses: Vec<String>) -> Self {
        Self {
            scripted_provider_responses,
            scripted_provider_usage: LlmUsage::default(),
            tool_provider: None,
            install_subagents: false,
            install_shell_processes: false,
            install_llm_tools: false,
            max_turns: None,
        }
    }

    fn tool_provider(mut self, tool_provider: Arc<dyn ToolProvider>) -> Self {
        self.tool_provider = Some(tool_provider);
        self
    }

    fn response_usage(mut self, usage: LlmUsage) -> Self {
        self.scripted_provider_usage = usage;
        self
    }

    fn maybe_tool_provider(mut self, tool_provider: Option<Arc<dyn ToolProvider>>) -> Self {
        self.tool_provider = tool_provider;
        self
    }

    fn install_subagents(mut self, install_subagents: bool) -> Self {
        self.install_subagents = install_subagents;
        self
    }

    fn install_shell_processes(mut self, install_shell_processes: bool) -> Self {
        self.install_shell_processes = install_shell_processes;
        self
    }

    fn install_llm_tools(mut self) -> Self {
        self.install_llm_tools = true;
        self
    }

    fn max_turns(mut self, max_turns: Option<usize>) -> Self {
        self.max_turns = max_turns;
        self
    }

    fn build(self) -> Result<AgentScenarioRuntime> {
        let checkpoint_writes =
            lash_core::testing::checkpoint_observer::CheckpointWriteCollector::default();
        let graph_store = Arc::new(crate::tracing::TraceLashlangGraphStore::default());
        let process_registry = Arc::new(TestLocalProcessRegistry::default());
        let prompt_captures = Arc::new(StdMutex::new(Vec::new()));
        let provider = scripted_provider(
            self.scripted_provider_responses,
            self.scripted_provider_usage,
            Arc::clone(&prompt_captures),
        );
        let factory = rlm_factory().with_lashlang_execution_sink(
            Arc::clone(&graph_store) as Arc<dyn crate::tracing::TraceSink>
        );
        let mut builder =
            explicit_ephemeral_facets(LashCore::rlm_builder(crate::TurnBudget::Unbounded, factory))
                .provider(provider)
                .model(mock_model_spec())
                .store_factory(Arc::new(
                    lash_core::testing::checkpoint_observer::ObservedSessionStoreFactory::new(
                        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
                        checkpoint_writes.clone(),
                    ),
                ))
                .process_registry(Arc::clone(&process_registry) as Arc<dyn ProcessRegistry>);
        if let Some(tools) = self.tool_provider {
            builder = builder.tools(tools);
        }
        if self.install_subagents {
            builder = builder.plugin(subagents_plugin());
        }
        if self.install_shell_processes {
            builder = builder
                .plugin(Arc::new(
                    lash_tools::shell::StandardShellPluginFactory::new(),
                ))
                .plugin(Arc::new(
                    lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(),
                ))
                .plugin(Arc::new(lash_core::plugin::StaticPluginFactory::new(
                    "agent-scenario-standard-batch",
                    lash_core::facade_support::PluginSpec::new()
                        .with_tool_provider(lash_protocol_standard::standard_batch_tool_provider()),
                )));
        }
        if self.install_llm_tools {
            builder = builder.plugin(Arc::new(lash_llm_tools::LlmToolsPluginFactory::default()));
        }
        if let Some(max_turns) = self.max_turns {
            builder = builder.turn_budget(lash_core::TurnBudget::bounded(max_turns));
        }
        Ok(AgentScenarioRuntime {
            core: builder.build(crate::testing::runtime_lease_owner())?,
            graph_store,
            process_registry,
            prompt_captures,
            checkpoint_writes,
        })
    }
}

struct AgentScenarioRuntime {
    core: LashCore,
    graph_store: Arc<crate::tracing::TraceLashlangGraphStore>,
    process_registry: Arc<TestLocalProcessRegistry>,
    prompt_captures: Arc<StdMutex<Vec<LlmRequest>>>,
    checkpoint_writes: lash_core::testing::checkpoint_observer::CheckpointWriteCollector,
}

impl AgentScenarioRuntime {
    fn prompt_captures_snapshot(&self) -> Vec<LlmRequest> {
        self.prompt_captures.lock_recover().clone()
    }

    async fn final_process_list(&self) -> Result<Vec<lash_core::ProcessHandleSummary>> {
        all_host_process_summaries(&self.core).await
    }
}

pub(super) fn lashlang_block(source: &str) -> String {
    format!("<lashlang>\n{}\n</lashlang>", source.trim())
}

pub(super) async fn run_agent_turn_scenario(case: AgentScenario) -> Result<AgentScenarioRun> {
    let run = run_agent_turn_scenario_without_success_assertions(case).await?;
    assert_successful_agent_scenario(&run);
    Ok(run)
}

pub(super) async fn run_agent_turn_scenario_without_success_assertions(
    case: AgentScenario,
) -> Result<AgentScenarioRun> {
    let runtime = AgentScenarioSetup::new(case.scripted_provider_responses.clone())
        .response_usage(case.scripted_provider_usage.clone())
        .maybe_tool_provider(case.tool_provider.clone())
        .install_subagents(case.install_subagents)
        .install_shell_processes(case.install_shell_processes)
        .max_turns(case.max_turns)
        .build()?;
    let session = runtime.core.session(&case.session_id).open().await?;
    let events = Arc::new(RecordingEvents::default());

    let turn_output = session
        .turn(TurnInput::text(case.root_prompt))
        .stream_to(events.as_ref())
        .await?;
    session.refresh_background_graph().await?;
    let final_process_list = runtime.final_process_list().await?;
    assert_remote_process_dto_surface(
        &runtime.core,
        runtime.process_registry.as_ref(),
        &case.session_id,
    )
    .await;
    assert_remote_process_summaries_round_trip(&final_process_list);
    let run = AgentScenarioRun {
        session_id: case.session_id.clone(),
        turn_output: Some(turn_output),
        streamed_events: events.snapshot().await,
        graph_snapshots: runtime.graph_store.graphs(),
        prompt_captures: runtime.prompt_captures_snapshot(),
        final_process_list,
        checkpoint_writes: runtime.checkpoint_writes.events(),
    };

    if let Some(expected) = &case.expected_final_value {
        let Some(output) = run.turn_output.as_ref() else {
            panic!("{} did not run a turn", case.name);
        };
        assert_eq!(
            output.final_value(),
            Some(expected),
            "{} final value mismatch",
            case.name
        );
    }

    let contract = GraphContract::from_graphs(&run.graph_snapshots);
    for title in case.expected_contracts.labeled_resource_titles {
        assert_labeled_resource_operation(
            &contract,
            title,
            crate::tracing::TraceLashlangNodeStatus::Completed,
        );
        assert_no_duplicate_label_step(&contract, title);
    }
    for title in case.expected_contracts.labeled_node_titles {
        assert_labeled_node(
            &contract,
            title,
            crate::tracing::TraceLashlangNodeStatus::Completed,
        );
        assert_no_duplicate_label_step(&contract, title);
    }
    for entry_name in case.expected_contracts.completed_process_entries {
        assert_completed_process_graph(&contract, entry_name);
    }
    assert_min_completed_process_graphs(
        &contract,
        case.expected_contracts.min_completed_process_graphs,
    );
    assert_min_completed_child_session_exec_graphs(
        &run,
        &case.session_id,
        case.expected_contracts
            .min_completed_child_session_exec_graphs,
    );
    Ok(run)
}

async fn all_host_process_summaries(
    core: &LashCore,
) -> Result<Vec<lash_core::ProcessHandleSummary>> {
    let processes = core
        .processes()
        .list(&lash_core::ProcessListFilter {
            definition: None,
            status: lash_core::ProcessStatusFilter::Any,
            waiting: None,
            ..lash_core::ProcessListFilter::default()
        })
        .await?;
    Ok(processes
        .into_iter()
        .map(observed_process_summary)
        .collect())
}

fn observed_process_summary(
    process: lash_core::facade_support::ObservedProcess,
) -> lash_core::ProcessHandleSummary {
    lash_core::ProcessHandleSummary::new(
        process.process_id,
        process.identity.clone(),
        process.lifecycle,
    )
    .with_definition(process.identity.definition)
}

async fn assert_remote_process_dto_surface(
    core: &LashCore,
    registry: &dyn lash_core::ProcessRegistry,
    session_id: &str,
) {
    let filter = lash_core::ProcessListFilter {
        definition: None,
        status: lash_core::ProcessStatusFilter::Any,
        waiting: None,
        ..lash_core::ProcessListFilter::default()
    };

    let observed = core
        .processes()
        .list(&filter)
        .await
        .expect("list observed processes for remote DTO round trip");
    let remote_list = lash_remote_protocol::RemoteProcessListResponse::try_from(observed.clone())
        .expect("observed process list should convert to remote DTO");
    remote_list
        .validate()
        .expect("remote observed process list should validate");
    let round_trip_observed: Vec<lash_core::facade_support::ObservedProcess> = remote_list
        .try_into()
        .expect("remote observed process list should convert back");
    let observed_ids = observed
        .iter()
        .map(|process| process.process_id.as_str())
        .collect::<Vec<_>>();
    let round_trip_ids = round_trip_observed
        .iter()
        .map(|process| process.process_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(round_trip_ids, observed_ids);

    let snapshot = core
        .processes()
        .session_snapshot(session_id)
        .await
        .expect("capture process work snapshot for remote DTO round trip");
    let remote_snapshot = lash_remote_protocol::RemoteProcessWorkSnapshot::try_from(snapshot)
        .expect("process work snapshot should convert to remote DTO");
    remote_snapshot
        .validate()
        .expect("remote process work snapshot should validate");
    let round_trip_snapshot: lash_core::facade_support::ProcessWorkSnapshot = remote_snapshot
        .try_into()
        .expect("remote process work snapshot should convert back");
    assert_eq!(round_trip_snapshot.session_id, session_id);

    let records = registry
        .list_processes(&filter)
        .await
        .expect("list process records for remote DTO round trip");
    for record in records {
        let process_id = record.id.clone();
        let remote_record = lash_remote_protocol::RemoteProcessRecord::try_from(record)
            .expect("process record should convert to remote DTO");
        remote_record
            .validate("AgentScenarioProcessRecord")
            .expect("remote process record should validate");
        let round_trip_record: lash_core::ProcessRecord = remote_record
            .try_into()
            .expect("remote process record should convert back");
        assert_eq!(round_trip_record.id, process_id);

        let events = registry
            .recent_events(&process_id, 32)
            .await
            .expect("load process event tail for remote DTO round trip");
        let expected_tail = events
            .iter()
            .map(|event| (event.sequence, event.event_type.clone()))
            .collect::<Vec<_>>();
        let remote_events = lash_remote_protocol::RemoteProcessEventsResponse::try_from((
            process_id.clone(),
            events,
        ))
        .expect("process events serialize for the remote protocol");
        remote_events
            .validate()
            .expect("remote process event tail should validate");
        let (round_trip_process_id, round_trip_events): (String, Vec<lash_core::ProcessEvent>) =
            remote_events
                .try_into()
                .expect("remote process event tail should convert back");
        let round_trip_tail = round_trip_events
            .iter()
            .map(|event| (event.sequence, event.event_type.clone()))
            .collect::<Vec<_>>();
        assert_eq!(round_trip_process_id, process_id);
        assert_eq!(round_trip_tail, expected_tail);
    }
}

fn assert_remote_process_summaries_round_trip(summaries: &[lash_core::ProcessHandleSummary]) {
    for summary in summaries {
        let remote = lash_remote_protocol::RemoteProcessSummary::from(summary.clone());
        remote
            .validate("AgentScenarioProcessSummary")
            .expect("remote process summary should validate");
        let round_trip =
            lash_core::ProcessHandleSummary::try_from(remote).expect("remote summary round trip");
        assert_eq!(&round_trip, summary);
    }
}

struct AgentSessionTurnProcessScenario {
    session_id: &'static str,
    child_session_id: &'static str,
    process_id: &'static str,
}

impl Default for AgentSessionTurnProcessScenario {
    fn default() -> Self {
        Self {
            session_id: "agent-scenario-session-turn-root",
            child_session_id: "agent-scenario-session-turn-child",
            process_id: "agent-scenario-session-turn-process",
        }
    }
}

impl AgentSessionTurnProcessScenario {
    async fn run(self) -> Result<()> {
        // Boundary: this mini-scenario owns the host session-turn process API,
        // while shared AgentScenario setup still covers the provider, process
        // registry, graph store, and remote DTO assertions.
        let runtime = self.runtime()?;
        let session = runtime.core.session(self.session_id).open().await?;
        let handle = session
            .processes()
            .start(
                self.start_request(),
                inline_scope(lash_core::ExecutionScope::process(self.process_id)),
            )
            .await?;
        assert_eq!(handle.process_id, self.process_id);
        session.refresh_background_graph().await?;
        self.assert_process_output(&runtime).await?;
        self.assert_agent_contracts(&runtime).await?;
        Ok(())
    }

    fn runtime(&self) -> Result<AgentScenarioRuntime> {
        AgentScenarioSetup::new(vec![lashlang_block(
            r#"finish { child: "done", scoped: true }"#,
        )])
        .install_subagents(true)
        .build()
    }

    fn start_request(&self) -> lash_core::ProcessStartRequest {
        lash_core::ProcessStartRequest::new(
            self.process_id,
            lash_core::ProcessInput::SessionTurn {
                definition_key: "agent-scenario-session-turn:v1".to_string(),
                create_request: Box::new(self.child_create_request()),
                turn_input: Box::new(TurnInput::text("run child session turn")),
                output_contract: lash_core::ToolOutputContract::Static,
            },
            lash_core::RecoveryDisposition::Rerunnable,
            lash_core::ProcessOriginator::host(),
        )
    }

    fn child_create_request(&self) -> lash_core::SessionCreateRequest {
        let child_policy = lash_core::SessionPolicy {
            model: mock_model_spec(),
            turn_budget: lash_core::TurnBudget::bounded(2),
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        };
        lash_core::SessionCreateRequest::child(
            self.session_id,
            lash_core::SessionStartPoint::Empty,
            child_policy,
            lash_core::PluginOptions::default(),
            "agent-scenario-session-turn",
        )
        .with_session_id(self.child_session_id)
    }

    async fn assert_process_output(&self, runtime: &AgentScenarioRuntime) -> Result<()> {
        let registry: Arc<dyn lash_core::ProcessRegistry> = runtime.process_registry.clone();
        let await_output = lash_core::facade_support::ProcessAwaiter::polling(registry)
            .await_terminal(self.process_id)
            .await?;
        let lash_core::ProcessAwaitOutput::Success { value, .. } = await_output else {
            panic!("session-turn process did not succeed: {await_output:#?}");
        };
        assert_eq!(
            value.get("child_session_id"),
            Some(&serde_json::json!(self.child_session_id))
        );
        let turn: lash_core::facade_support::AssembledTurn = value
            .get("turn")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .expect("session-turn output should decode")
            .expect("session-turn output should contain a turn");
        assert_eq!(
            turn.outcome,
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue {
                value: serde_json::json!({ "child": "done", "scoped": true })
            })
        );
        Ok(())
    }

    async fn assert_agent_contracts(&self, runtime: &AgentScenarioRuntime) -> Result<()> {
        let final_process_list = runtime.final_process_list().await?;
        assert_remote_process_dto_surface(
            &runtime.core,
            runtime.process_registry.as_ref(),
            self.session_id,
        )
        .await;
        assert_remote_process_summaries_round_trip(&final_process_list);
        let run = AgentScenarioRun {
            session_id: self.session_id.to_string(),
            turn_output: None,
            streamed_events: Vec::new(),
            graph_snapshots: runtime.graph_store.graphs(),
            prompt_captures: runtime.prompt_captures_snapshot(),
            final_process_list,
            checkpoint_writes: runtime.checkpoint_writes.events(),
        };
        assert_eq!(run.prompt_captures.len(), 1);
        assert_all_processes_terminal(&run.final_process_list);
        assert_session_turn_child_graph(&run, self.child_session_id, self.process_id);
        Ok(())
    }
}

struct AgentDurableInputSuspensionScenario {
    session_id: &'static str,
    request_id: &'static str,
}

impl Default for AgentDurableInputSuspensionScenario {
    fn default() -> Self {
        Self {
            session_id: "agent-scenario-durable-input-request",
            request_id: "request-1",
        }
    }
}

impl AgentDurableInputSuspensionScenario {
    async fn run(self) -> Result<()> {
        // Boundary: this mini-scenario is intentionally live because the owned
        // invariant is suspension before resolving the durable await key.
        let (key_tx, key_rx) = oneshot::channel();
        let tools = Arc::new(DurableInputTools::new(key_tx));
        let runtime = self.runtime(Arc::clone(&tools) as Arc<dyn ToolProvider>)?;
        let session = runtime.core.session(self.session_id).open().await?;
        let events = Arc::new(RecordingEvents::default());
        let turn_session = session.clone();
        let turn_events = Arc::clone(&events);
        let mut turn = tokio::spawn(async move {
            turn_session
                .turn(TurnInput::text(
                    "Start a process that asks for durable input.",
                ))
                .stream_to(turn_events.as_ref())
                .await
        });

        let key = self.await_suspension_key(key_rx, events.as_ref()).await;
        self.assert_turn_suspended_before_resolution(&mut turn, events.as_ref())
            .await;
        self.resolve_key(&runtime, key).await?;
        let turn_output = turn.await.expect("turn task")?;
        session.refresh_background_graph().await?;

        self.assert_turn_completed(&turn_output, tools.as_ref());
        self.assert_agent_contracts(&runtime).await?;
        Ok(())
    }

    fn runtime(&self, tools: Arc<dyn ToolProvider>) -> Result<AgentScenarioRuntime> {
        AgentScenarioSetup::new(vec![
            lashlang_block(
                r#"
process request_answer(tools: Tools) {
  result = await tools.mock_input_request({ question: "Need input?" })?
  finish result
}
handle = start request_answer(tools: tools)
result = (await handle)?
finish result.answer"#,
            ),
            lashlang_block("finish { recovered: true }"),
        ])
        .tool_provider(tools)
        .build()
    }

    async fn await_suspension_key(
        &self,
        key_rx: oneshot::Receiver<std::result::Result<lash_core::AwaitEventKey, String>>,
        events: &RecordingEvents,
    ) -> lash_core::AwaitEventKey {
        let key_result = tokio::time::timeout(std::time::Duration::from_secs(1), key_rx)
            .await
            .expect("durable input tool should publish await key")
            .expect("durable input key sender should stay alive");
        let key = match key_result {
            Ok(key) => key,
            Err(err) => {
                panic!(
                    "durable input tool failed before awaiting external input: {err}; events: {:#?}",
                    events.snapshot().await
                )
            }
        };
        assert!(
            matches!(
                key.wait,
                lash_core::AwaitEventWaitIdentity::ToolCompletion { .. }
            ),
            "durable input tool should use a tool-completion await key: {:?}",
            key.wait
        );
        key
    }

    async fn assert_turn_suspended_before_resolution(
        &self,
        turn: &mut tokio::task::JoinHandle<Result<TurnResult>>,
        events: &RecordingEvents,
    ) {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        if let Ok(joined) = tokio::time::timeout(std::time::Duration::from_millis(20), turn).await {
            let result = joined.expect("turn task completed before durable input resolution");
            panic!(
                "turn completed before the durable input request was resolved: {result:#?}; events: {:#?}",
                events.snapshot().await
            );
        }
    }

    async fn resolve_key(
        &self,
        runtime: &AgentScenarioRuntime,
        key: lash_core::AwaitEventKey,
    ) -> Result<()> {
        let answer = serde_json::json!({
            "request_id": self.request_id,
            "answer": "approved"
        });
        let outcome = runtime
            .core
            .completions()
            .resolve(key, lash_core::Resolution::Ok(answer))
            .await?;
        assert_eq!(outcome, lash_core::ResolveOutcome::Accepted);
        Ok(())
    }

    fn assert_turn_completed(&self, turn_output: &TurnResult, tools: &DurableInputTools) {
        assert!(matches!(
            turn_output.outcome,
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue { .. })
        ));
        assert_eq!(
            turn_output.final_value(),
            Some(&serde_json::json!("approved"))
        );
        assert_eq!(tools.attempt_count(), 1);
    }

    async fn assert_agent_contracts(&self, runtime: &AgentScenarioRuntime) -> Result<()> {
        let final_process_list = runtime.final_process_list().await?;
        assert_remote_process_dto_surface(
            &runtime.core,
            runtime.process_registry.as_ref(),
            self.session_id,
        )
        .await;
        assert_remote_process_summaries_round_trip(&final_process_list);
        assert_eq!(
            final_process_list.len(),
            1,
            "durable input request should not start a child process"
        );
        assert_all_processes_terminal(&final_process_list);
        let process_id = final_process_list[0].process_id.clone();
        let process_events = runtime.core.processes().events(&process_id, 0).await?;
        assert!(
            process_events.iter().any(|event| {
                event.event_type == "process.yield"
                    && event.payload.get("type")
                        == Some(&serde_json::json!("work.input_request.opened"))
                    && event.payload.get("answer").is_none()
                    && event.payload.get("request_id") == Some(&serde_json::json!(self.request_id))
            }),
            "durable input request event was not appended: {process_events:#?}"
        );
        assert!(
            process_events
                .iter()
                .all(|event| event.event_type != "process.waiting"),
            "durable input request should not rely on wait_signal: {process_events:#?}"
        );
        assert_eq!(runtime.prompt_captures_snapshot().len(), 1);
        let contract = GraphContract::from_graphs(&runtime.graph_store.graphs());
        assert_min_completed_process_graphs(&contract, 1);
        Ok(())
    }
}

pub(super) async fn run_agent_session_turn_process_scenario() -> Result<()> {
    AgentSessionTurnProcessScenario::default().run().await
}

pub(super) async fn run_agent_durable_input_request_scenario() -> Result<()> {
    AgentDurableInputSuspensionScenario::default().run().await
}

pub(super) async fn run_agent_process_llm_query_scenario() -> Result<()> {
    let runtime = AgentScenarioSetup::new(vec![
        lashlang_block(
            r#"
process enrich(event: { email: str }) {
  enriched = await llm.query({
    task: "Classify the supplied email",
    inputs: { event: event },
    output: Type { category: str, confidence: float }
  })?
  finish enriched
}
handle = start enrich(event: { email: "hello@example.com" })
finish (await handle)?"#,
        ),
        r#"{"kind":"value","value":{"category":"personal","confidence":0.98},"error":null}"#
            .to_string(),
    ])
    .install_llm_tools()
    .build()?;
    let session = runtime
        .core
        .session("agent-scenario-process-llm-query")
        .open()
        .await?;
    let result = session
        .turn(TurnInput::text("Enrich the email in a durable process."))
        .run()
        .await?;
    assert_eq!(
        result.final_value(),
        Some(&serde_json::json!({
            "category": "personal",
            "confidence": 0.98
        }))
    );
    let requests = runtime.prompt_captures_snapshot();
    assert_eq!(requests.len(), 2, "outer turn plus one llm_query call");
    assert!(requests[1].stream_events.is_none());
    assert!(matches!(
        requests[1].output_spec,
        Some(lash_core::llm::types::LlmOutputSpec::JsonSchema(_))
    ));
    Ok(())
}

pub(super) async fn run_agent_direct_completion_attempt_retry_scenario() -> Result<()> {
    let runtime = AgentScenarioSetup::new(vec![
        lashlang_block(
            r#"
process retry_direct(tools: Tools) {
  value = await tools.retrying_direct({})?
  finish value
}
handle = start retry_direct(tools: tools)
finish (await handle)?"#,
        ),
        "first-provider-result".to_string(),
        "second-provider-result".to_string(),
    ])
    .tool_provider(Arc::new(RetryingDirectTools))
    .build()?;
    let session = runtime
        .core
        .session("agent-scenario-direct-completion-attempt-retry")
        .open()
        .await?;
    let result = session
        .turn(TurnInput::text(
            "Retry the complete atomic tool attempt once.",
        ))
        .run()
        .await?;
    assert_eq!(
        result.final_value(),
        Some(&serde_json::json!("second-provider-result"))
    );
    let requests = runtime.prompt_captures_snapshot();
    assert_eq!(
        requests.len(),
        3,
        "outer turn plus one provider call for each of two tool attempts"
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| format!("{:?}", message.blocks).contains("attempt 1"))
    );
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|message| format!("{:?}", message.blocks).contains("attempt 2"))
    );
    Ok(())
}

fn scripted_provider(
    responses: Vec<String>,
    usage: LlmUsage,
    prompt_captures: Arc<StdMutex<Vec<LlmRequest>>>,
) -> ProviderHandle {
    let responses = Arc::new(TokioMutex::new(VecDeque::from(responses)));
    crate::testing::TestProvider::builder()
        .kind("agent-scenario")
        .complete(move |request| {
            let responses = Arc::clone(&responses);
            let usage = usage.clone();
            let prompt_captures = Arc::clone(&prompt_captures);
            async move {
                prompt_captures.lock_recover().push(request.clone());
                let Some(text) = responses.lock().await.pop_front() else {
                    return Err(lash_core::llm::transport::LlmTransportError::new(
                        "scripted agent scenario provider exhausted its expected responses",
                    ));
                };
                Ok(LlmResponse {
                    full_text: text.clone(),
                    parts: vec![LlmOutputPart::Text {
                        text,
                        response_meta: None,
                    }],
                    usage,
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

fn subagents_plugin() -> Arc<dyn PluginFactory> {
    Arc::new(lash_subagents::SubagentsPluginFactory::new(Arc::new(
        lash_subagents::CapabilityRegistry::new().with(Arc::new(
            lash_subagents::StaticCapability::new("default", SessionSpec::inherit()),
        )),
    )))
}
