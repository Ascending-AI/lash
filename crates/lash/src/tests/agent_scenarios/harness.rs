use super::super::*;
use super::contracts::{
    GraphContract, NodeStatusFact, assert_all_processes_terminal,
    assert_completed_lifted_process_graphs, assert_labeled_node, assert_labeled_resource_operation,
    assert_min_completed_child_session_exec_graphs, assert_min_completed_process_graphs,
    assert_no_duplicate_label_step, assert_session_turn_child_graph,
    assert_successful_agent_scenario,
};
use lash_core::llm::types::LlmUsage;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;
use std::collections::VecDeque;

#[derive(Default)]
pub(super) struct AgentScenarioExpectations {
    pub(super) completed_lifted_processes: Option<usize>,
    pub(super) labeled_resource_titles: Vec<&'static str>,
    pub(super) labeled_node_titles: Vec<&'static str>,
    pub(super) min_completed_child_session_exec_graphs: usize,
    pub(super) min_completed_process_graphs: usize,
    /// `(kind, count)`: the labels are lift digests, so a scenario pins how
    /// many records of a kind the session observer exposes, not their names.
    pub(super) observer_visible_processes: Vec<(&'static str, usize)>,
}

pub(super) struct AgentScenario {
    pub(super) name: &'static str,
    pub(super) session_id: SessionId,
    pub(super) scripted_provider_responses: Vec<String>,
    pub(super) scripted_provider_usage: LlmUsage,
    pub(super) root_prompt: &'static str,
    pub(super) expected_final_value: Option<serde_json::Value>,
    pub(super) tool_provider: Option<Arc<dyn ToolProvider>>,
    pub(super) install_subagents: bool,
    pub(super) install_process_composition: bool,
    pub(super) max_turns: Option<usize>,
    pub(super) precompleted_process: Option<(ProcessId, lash_core::ProcessAwaitOutput)>,
    /// Digests whose bytes a scenario pretends were already uploaded by some
    /// other writer (a child process, a peer session). Adoption is gated on
    /// recorded upload evidence, so the scenario must record it rather than
    /// conjure a reference to bytes no store ever accepted.
    pub(super) seeded_attachment_writes: Vec<lash_core::AttachmentId>,
    /// A scenario that scripts a program the runtime is *meant* to refuse or
    /// fail sets this. Every other scenario asserts that no scripted cell was
    /// refused: a refusal otherwise reads downstream as "the process never
    /// started", which is how a retired-form regression once hid here.
    pub(super) expects_refused_cell: bool,
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
            install_process_composition: false,
            max_turns: None,
            precompleted_process: None,
            seeded_attachment_writes: Vec::new(),
            expects_refused_cell: false,
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

    /// Declares that this scenario scripts a cell the runtime is expected to
    /// refuse or fail, so the refusal check below must not fire.
    pub(super) fn expects_refused_cell(mut self) -> Self {
        self.expects_refused_cell = true;
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

    pub(super) fn install_process_composition(mut self) -> Self {
        self.install_process_composition = true;
        self
    }

    pub(super) fn max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = Some(max_turns);
        self
    }

    pub(super) fn precompleted_process(
        mut self,
        process_id: impl Into<ProcessId>,
        output: lash_core::ProcessAwaitOutput,
    ) -> Self {
        self.precompleted_process = Some((process_id.into(), output));
        self
    }

    pub(super) fn seeded_attachment_write(
        mut self,
        attachment_id: lash_core::AttachmentId,
    ) -> Self {
        self.seeded_attachment_writes.push(attachment_id);
        self
    }

    /// How many lifted process bodies must show a completed executed graph.
    pub(super) fn completed_lifted_processes(mut self, count: usize) -> Self {
        self.expected_contracts.completed_lifted_processes = Some(count);
        self
    }

    /// The program named this resource operation with an `@label` doc comment,
    /// so the executed graph must carry that title on the operation's own node
    /// rather than on a step beside it.
    pub(super) fn labeled_resource(mut self, title: &'static str) -> Self {
        self.expected_contracts.labeled_resource_titles.push(title);
        self
    }

    /// The same, for a labeled node that is not a resource operation.
    pub(super) fn labeled_node(mut self, title: &'static str) -> Self {
        self.expected_contracts.labeled_node_titles.push(title);
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

    pub(super) fn observer_visible_processes(mut self, kind: &'static str, count: usize) -> Self {
        self.expected_contracts
            .observer_visible_processes
            .push((kind, count));
        self
    }
}

fn agent_scenario_session_id(name: &str) -> SessionId {
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
    SessionId::from(slug)
}

pub(super) struct AgentScenarioRun {
    pub(super) session_id: SessionId,
    pub(super) turn_output: Option<TurnReport>,
    pub(super) streamed_events: Vec<TurnActivity>,
    pub(super) graph_snapshots: Vec<crate::tracing::TraceLashlangGraph>,
    pub(super) prompt_captures: Vec<LlmRequest>,
    pub(super) final_process_list: Vec<lash_core::ProcessHandleView>,
    /// Runtime-checkpoint commits the session store actually accepted, in
    /// commit order. Observed at the store seam, never reconstructed.
    pub(super) checkpoint_writes:
        Vec<lash_core::testing::checkpoint_observer::CheckpointWriteEvent>,
    /// Attachment roots submitted by the root turn's runtime-checkpoint commit.
    pub(super) committed_attachment_ids: Vec<lash_core::AttachmentId>,
}

struct AgentScenarioSetup {
    scripted_provider_responses: Vec<String>,
    scripted_provider_usage: LlmUsage,
    tool_provider: Option<Arc<dyn ToolProvider>>,
    install_subagents: bool,
    install_process_composition: bool,
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
            install_process_composition: false,
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

    fn install_process_composition(mut self, install_process_composition: bool) -> Self {
        self.install_process_composition = install_process_composition;
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
        let store_factory: Arc<dyn lash_core::SessionStoreFactory> = Arc::new(
            lash_core::testing::checkpoint_observer::ObservedSessionStoreFactory::new(
                Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
                checkpoint_writes.clone(),
            ),
        );
        let mut builder =
            explicit_ephemeral_facets(LashCore::rlm_builder(crate::TurnBudget::Unbounded, factory))
                .provider(provider)
                .model(mock_model_spec())
                .store_factory(Arc::clone(&store_factory))
                .process_registry(Arc::clone(&process_registry) as Arc<dyn ProcessRegistry>);
        if let Some(tools) = self.tool_provider {
            builder = builder.tools(tools);
        }
        if self.install_subagents {
            builder = builder.plugin(subagents_plugin());
        }
        if self.install_process_composition {
            builder = builder
                .plugin(Arc::new(
                    lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(),
                ))
                .plugin(Arc::new(lash_core::plugin::StaticPluginFactory::new(
                    "agent-scenario-standard-batch",
                    lash_core::facade_support::PluginSpec::new().with_orchestrating_tool(
                        lash_protocol_standard::standard_batch_orchestrating_tool(),
                    ),
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
            store_factory,
            graph_store,
            process_registry,
            prompt_captures,
            checkpoint_writes,
        })
    }
}

struct AgentScenarioRuntime {
    core: LashCore,
    store_factory: Arc<dyn lash_core::SessionStoreFactory>,
    graph_store: Arc<crate::tracing::TraceLashlangGraphStore>,
    process_registry: Arc<TestLocalProcessRegistry>,
    prompt_captures: Arc<StdMutex<Vec<LlmRequest>>>,
    checkpoint_writes: lash_core::testing::checkpoint_observer::CheckpointWriteCollector,
}

impl AgentScenarioRuntime {
    fn prompt_captures_snapshot(&self) -> Vec<LlmRequest> {
        self.prompt_captures.lock_recover().clone()
    }

    async fn final_process_list(&self) -> Result<Vec<lash_core::ProcessHandleView>> {
        all_host_process_summaries(&self.core).await
    }
}

pub(super) fn typescript_block(source: &str) -> String {
    format!("<typescript>\n{}\n</typescript>", source.trim())
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
        .install_process_composition(case.install_process_composition)
        .max_turns(case.max_turns)
        .build()?;
    let session = runtime.core.session(&case.session_id).open().await?;
    if !case.seeded_attachment_writes.is_empty() {
        // Stand in for the writer that really uploaded these bytes. The
        // evidence a store keeps is per digest, not per session, so a
        // dedicated seeding session records it exactly as a peer writer would.
        let seed_session_id = SessionId::from(format!("{}-attachment-seed", case.session_id));
        let seed_store = lash_core::SessionStoreFactory::create_store(
            runtime.store_factory.as_ref(),
            &lash_core::SessionStoreCreateRequest {
                pending_observer_intents: Vec::new(),
                session_id: seed_session_id.clone(),
                relation: lash_core::SessionRelation::Root,
                policy: lash_core::SessionPolicy::new(crate::TurnBudget::Unbounded),
            },
        )
        .await?;
        for attachment_id in &case.seeded_attachment_writes {
            let intent = lash_core::AttachmentIntent {
                attachment_id: attachment_id.clone(),
                session_id: seed_session_id.clone(),
                canonical_uri: format!("lash-attachment://blake3/{attachment_id}"),
                intent_at_epoch_ms: 1,
                owner: None,
            };
            let lash_core::AttachmentWriteFence::Granted(permit) =
                lash_core::AttachmentManifest::begin_attachment_write(
                    seed_store.as_ref(),
                    intent.clone(),
                )
                .await?
            else {
                panic!("a seeded attachment write must be granted");
            };
            lash_core::AttachmentManifest::complete_attachment_write(
                seed_store.as_ref(),
                &intent,
                permit,
            )
            .await?;
        }
    }
    if let Some((process_id, output)) = case.precompleted_process.clone() {
        runtime
            .process_registry
            .register_process_with_observers(
                lash_core::ProcessRegistration::new(
                    process_id.clone(),
                    lash_core::ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    lash_core::RecoveryContract::ExternallyOwned,
                    lash_core::ProcessProvenance::session(lash_core::SessionScope::new(
                        &case.session_id,
                    )),
                    lash_core::ProcessLifecyclePolicy::new(
                        lash_core::ParentScope::Host,
                        lash_core::OnParentEnd::Abandon,
                    ),
                )
                .with_admitted_identity(
                    lash_core::AdmittedProcessIdentity::for_testing(
                        lash_core::ProcessIdentity::new("test.awaited-child"),
                    ),
                ),
                std::slice::from_ref(&case.session_id),
            )
            .await?;
        runtime
            .process_registry
            .complete_process(
                &process_id,
                output,
                lash_core::ProcessCompletionAuthority::external_owner(),
            )
            .await?;
    }
    let events = Arc::new(RecordingEvents::default());

    let turn_output = session
        .turn(TurnInput::text(case.root_prompt))
        .stream_to(events.as_ref())
        .await?;
    session.refresh_background_graph().await?;
    if !case.expects_refused_cell {
        assert_no_refused_cell(case.name, &events.snapshot().await);
    }
    assert_session_process_admission_contract(
        runtime.process_registry.as_ref(),
        &case.session_id,
        &case.expected_contracts.observer_visible_processes,
    )
    .await;
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
        committed_attachment_ids: runtime
            .checkpoint_writes
            .committed_attachment_ids(&case.session_id, 0)
            .unwrap_or_default(),
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
    if let Some(expected) = case.expected_contracts.completed_lifted_processes {
        assert_completed_lifted_process_graphs(&contract, expected);
    }
    for title in case.expected_contracts.labeled_resource_titles {
        assert_labeled_resource_operation(&contract, title, NodeStatusFact::Completed);
        assert_no_duplicate_label_step(&contract, title);
    }
    for title in case.expected_contracts.labeled_node_titles {
        assert_labeled_node(&contract, title, NodeStatusFact::Completed);
        assert_no_duplicate_label_step(&contract, title);
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

/// Fails on the refusal itself rather than on its shadow.
///
/// A scripted program the dialect refuses produces a failed cell and no
/// process, no tool call and no final value. Every downstream assertion then
/// reports an empty observation, which reads as a runtime defect instead of a
/// scripted source that no longer parses. This names the refusal directly.
fn assert_no_refused_cell(name: &str, events: &[TurnActivity]) {
    for activity in events {
        let TurnEvent::CodeBlockCompleted { error, success, .. } = &activity.event else {
            continue;
        };
        if *success {
            continue;
        }
        let Some(failure) = error else { continue };
        assert!(
            !matches!(
                failure.kind,
                lash_core::CellFailureKind::Policy | lash_core::CellFailureKind::Program
            ),
            "{name}: the runtime refused the scripted program ({:?}): {}\n\
             the scripted source must be authored on the live dialect surface; \
             a scenario that means to script a refusal declares expects_refused_cell()",
            failure.kind,
            failure.message,
        );
    }
}

async fn assert_session_process_admission_contract(
    registry: &dyn lash_core::ProcessRegistry,
    session_id: &SessionId,
    expected_processes: &[(&str, usize)],
) {
    if expected_processes.is_empty() {
        return;
    }
    let observed = registry
        .list_observed_by(
            session_id,
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("list runtime processes through the session observer");
    let observed_identities = observed
        .iter()
        .map(|process| (&process.id, &process.identity, &process.status))
        .collect::<Vec<_>>();
    for (kind, count) in expected_processes {
        // The label is the lifted declaration's digest name, so the pin is the
        // kind and how many records of it the observer exposes.
        let matching = observed
            .iter()
            .filter(|process| {
                process.identity.kind == *kind
                    && process.identity.label.as_deref().is_some_and(|label| {
                        label.starts_with(lashlang::LIFTED_PROCESS_NAME_PREFIX)
                    })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            matching.len(),
            *count,
            "the parent session observer must expose {count} lifted {kind} process records; observed={observed_identities:?}"
        );
        for process in matching {
            assert_eq!(process.status, lash_core::ProcessStatus::Completed);
            let observers = registry
                .observers_for_process(&process.id)
                .await
                .expect("load process observer edges");
            assert!(
                observers.iter().any(|observer| observer == session_id),
                "completed process {} must retain the spawning session observer edge; observers={observers:?}",
                process.id
            );
            let events = registry
                .events_after(&process.id, 0)
                .await
                .expect("load observer-visible process lifecycle events");
            let first_started = events
                .iter()
                .position(|event| event.event_type == "process.first_started")
                .unwrap_or_else(|| {
                    panic!(
                        "missing process.first_started for observer-visible process {}: {events:?}",
                        process.id
                    )
                });
            let completed = events
                .iter()
                .position(|event| event.event_type == "process.completed")
                .unwrap_or_else(|| {
                    panic!(
                        "missing process.completed for observer-visible process {}: {events:?}",
                        process.id
                    )
                });
            assert!(
                first_started < completed,
                "process.first_started must precede process.completed for {}: {events:?}",
                process.id
            );
        }
    }
}

async fn all_host_process_summaries(core: &LashCore) -> Result<Vec<lash_core::ProcessHandleView>> {
    let processes = core
        .processes()
        .list(&lash_core::ProcessListFilter {
            definition: None,
            status: lash_core::ProcessStatusFilter::Any,

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
) -> lash_core::ProcessHandleView {
    lash_core::ProcessHandleView::new(
        process.process_id,
        process.incarnation,
        process.identity.clone(),
        process.lifecycle,
    )
    .with_definition(process.identity.definition)
}

async fn assert_remote_process_dto_surface(
    core: &LashCore,
    registry: &dyn lash_core::ProcessRegistry,
    session_id: &SessionId,
) {
    let filter = lash_core::ProcessListFilter {
        definition: None,
        status: lash_core::ProcessStatusFilter::Any,

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
        let process_ref = lash_core::ProcessRef::from_record(&record);
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
            process_ref.clone(),
            events,
        ))
        .expect("process events serialize for the remote protocol");
        remote_events
            .validate()
            .expect("remote process event tail should validate");
        let (round_trip_process_ref, round_trip_events): (
            lash_core::ProcessRef,
            Vec<lash_core::ProcessEvent>,
        ) = remote_events
            .try_into()
            .expect("remote process event tail should convert back");
        let round_trip_tail = round_trip_events
            .iter()
            .map(|event| (event.sequence, event.event_type.clone()))
            .collect::<Vec<_>>();
        assert_eq!(round_trip_process_ref, process_ref);
        assert_eq!(round_trip_tail, expected_tail);
    }
}

fn assert_remote_process_summaries_round_trip(summaries: &[lash_core::ProcessHandleView]) {
    for summary in summaries {
        let remote = lash_remote_protocol::RemoteProcessHandleView::from(summary.clone());
        remote
            .validate("AgentScenarioProcessSummary")
            .expect("remote process summary should validate");
        let round_trip =
            lash_core::ProcessHandleView::try_from(remote).expect("remote summary round trip");
        assert_eq!(&round_trip, summary);
    }
}

struct AgentSessionTurnProcessScenario {
    session_id: SessionId,
    child_session_id: SessionId,
    process_id: ProcessId,
}

impl Default for AgentSessionTurnProcessScenario {
    fn default() -> Self {
        Self {
            session_id: SessionId::from("agent-scenario-session-turn-root"),
            child_session_id: SessionId::from("agent-scenario-session-turn-child"),
            process_id: ProcessId::from("agent-scenario-session-turn-process"),
        }
    }
}

impl AgentSessionTurnProcessScenario {
    async fn run(self) -> Result<()> {
        // Boundary: this mini-scenario owns the host session-turn process API,
        // while shared AgentScenario setup still covers the provider, process
        // registry, graph store, and remote DTO assertions.
        let runtime = self.runtime()?;
        let session = runtime.core.session(&self.session_id).open().await?;
        let handle = session
            .admin()
            .processes()
            .start(
                self.start_request(),
                native_scope(lash_core::ExecutionScope::process(self.process_id.clone())),
            )
            .await?;
        assert_eq!(handle.process_id, self.process_id);
        session.refresh_background_graph().await?;
        self.assert_process_output(&runtime).await?;
        self.assert_agent_contracts(&runtime).await?;
        Ok(())
    }

    fn runtime(&self) -> Result<AgentScenarioRuntime> {
        AgentScenarioSetup::new(vec![typescript_block(
            r#"finish({ child: "done", scoped: true });"#,
        )])
        .install_subagents(true)
        .build()
    }

    fn start_request(&self) -> lash_core::ProcessStartRequest {
        lash_core::ProcessStartRequest::new(
            self.process_id.clone(),
            lash_core::ProcessInput::SessionTurn {
                definition_key: "agent-scenario-session-turn:v1".to_string(),
                create_request: Box::new(self.child_create_request()),
                turn_input: Box::new(TurnInput::text("run child session turn")),
                output_contract: lash_core::ToolOutputContract::Static,
            },
            lash_core::RecoveryContract::Rerunnable,
            lash_core::ProcessOriginator::host(),
            lash_core::ProcessLifecyclePolicy::new(
                lash_core::ParentScope::Host,
                lash_core::OnParentEnd::Abandon,
            ),
        )
    }

    fn child_create_request(&self) -> lash_core::SessionCreateRequest {
        let child_policy = lash_core::SessionPolicy {
            model: mock_model_spec(),
            turn_budget: lash_core::TurnBudget::bounded(2),
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        };
        lash_core::SessionCreateRequest::child(
            self.session_id.clone(),
            lash_core::SessionStartPoint::Empty,
            child_policy,
            lash_core::PluginOptions::default(),
            "agent-scenario-session-turn",
        )
        .with_session_id(self.child_session_id.clone())
    }

    async fn assert_process_output(&self, runtime: &AgentScenarioRuntime) -> Result<()> {
        let registry: Arc<dyn lash_core::ProcessRegistry> = runtime.process_registry.clone();
        let await_output = lash_core::NativeProcessWork::for_registry(registry)
            .await_terminal(&self.process_id)
            .await?;
        let output = await_output.into_tool_output();
        assert!(
            output.is_success(),
            "session-turn process did not succeed: {output:#?}"
        );
        let value = output.into_value_for_projection();
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
            &self.session_id,
        )
        .await;
        assert_remote_process_summaries_round_trip(&final_process_list);
        let run = AgentScenarioRun {
            session_id: SessionId::from(self.session_id.to_string()),
            turn_output: None,
            streamed_events: Vec::new(),
            graph_snapshots: runtime.graph_store.graphs(),
            prompt_captures: runtime.prompt_captures_snapshot(),
            final_process_list,
            checkpoint_writes: runtime.checkpoint_writes.events(),
            committed_attachment_ids: runtime
                .checkpoint_writes
                .committed_attachment_ids(&self.session_id, 0)
                .unwrap_or_default(),
        };
        assert_eq!(run.prompt_captures.len(), 1);
        assert_all_processes_terminal(&run.final_process_list);
        assert_session_turn_child_graph(&run, &self.child_session_id, &self.process_id);
        Ok(())
    }
}

struct AgentDurableInputSuspensionScenario {
    session_id: SessionId,
    request_id: &'static str,
}

impl Default for AgentDurableInputSuspensionScenario {
    fn default() -> Self {
        Self {
            session_id: SessionId::from("agent-scenario-durable-input-request"),
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
        let session = runtime.core.session(&self.session_id).open().await?;
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
            typescript_block(
                r#"
const requestAnswer = async () => {
  const result = await tools.mock_input_request({ question: "Need input?" });
  return result;
};
const handle = await processes.start({ definition: requestAnswer });
const result = await handle;
finish(result.answer);"#,
            ),
            typescript_block("finish({ recovered: true });"),
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
        turn: &mut tokio::task::JoinHandle<Result<TurnReport>>,
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

    fn assert_turn_completed(&self, turn_output: &TurnReport, tools: &DurableInputTools) {
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
            &self.session_id,
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
        typescript_block(
            r#"
const enrich = async (event) => {
  const enriched = await llm.query({
    task: "Classify the supplied email",
    inputs: { event: event },
    output: { category: "str", confidence: "float" }
  });
  return enriched;
};
const handle = await processes.start({ definition: enrich, args: { event: { email: "hello@example.com" } } });
finish(await handle);"#,
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
        typescript_block(
            r#"
const retryDirect = async () => {
  const value = await tools.retrying_direct({});
  return value;
};
const handle = await processes.start({ definition: retryDirect });
finish(await handle);"#,
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
