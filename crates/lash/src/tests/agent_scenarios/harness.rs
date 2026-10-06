use super::super::*;
use super::contracts::assert_successful_agent_scenario;
use lash_core::llm::types::LlmUsage;
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;
use std::collections::VecDeque;

pub(super) struct AgentScenario {
    pub(super) name: &'static str,
    pub(super) session_id: SessionId,
    pub(super) scripted_provider_responses: Vec<String>,
    pub(super) root_prompt: &'static str,
    pub(super) install_subagents: bool,
    pub(super) max_turns: Option<usize>,
    /// A process the harness registers and completes before the turn, whose
    /// handle a scripted response names as [`PRECOMPLETED_PROCESS_HANDLE`].
    pub(super) precompleted_process: Option<lash_core::ProcessAwaitOutput>,
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
}

impl AgentScenario {
    pub(super) fn new(name: &'static str, root_prompt: &'static str) -> Self {
        Self {
            name,
            session_id: agent_scenario_session_id(name),
            scripted_provider_responses: Vec::new(),
            root_prompt,
            install_subagents: false,
            max_turns: None,
            precompleted_process: None,
            seeded_attachment_writes: Vec::new(),
            expects_refused_cell: false,
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

    pub(super) fn install_subagents(mut self) -> Self {
        self.install_subagents = true;
        self
    }

    pub(super) fn max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = Some(max_turns);
        self
    }

    pub(super) fn precompleted_process(mut self, output: lash_core::ProcessAwaitOutput) -> Self {
        self.precompleted_process = Some(output);
        self
    }

    pub(super) fn seeded_attachment_write(
        mut self,
        attachment_id: lash_core::AttachmentId,
    ) -> Self {
        self.seeded_attachment_writes.push(attachment_id);
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
    SessionId::fixture(slug)
}

pub(super) struct AgentScenarioRun {
    pub(super) session_id: SessionId,
    pub(super) turn_output: Option<TurnReport>,
    pub(super) streamed_events: Vec<TurnActivity>,
    pub(super) graph_snapshots: Vec<crate::tracing::TraceLashlangGraph>,
    pub(super) final_process_list: Vec<lash_core::ProcessHandleView>,
    /// Runtime-checkpoint commits the session store actually accepted, in
    /// commit order. Observed at the store seam, never reconstructed.
    pub(super) checkpoint_writes:
        Vec<lash_core::testing::checkpoint_observer::CheckpointWriteEvent>,
    /// Attachment roots submitted by the runtime-checkpoint commit that settles
    /// the root turn.
    pub(super) committed_attachment_ids: Vec<lash_core::AttachmentId>,
}

struct AgentScenarioSetup {
    scripted_provider_responses: Vec<String>,
    scripted_provider_usage: LlmUsage,
    tool_provider: Option<Arc<dyn ToolProvider>>,
    install_subagents: bool,
    install_process_controls: bool,
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
            install_process_controls: false,
            install_llm_tools: false,
            max_turns: None,
        }
    }

    fn tool_provider(mut self, tool_provider: Arc<dyn ToolProvider>) -> Self {
        self.tool_provider = Some(tool_provider);
        self
    }

    fn install_subagents(mut self, install_subagents: bool) -> Self {
        self.install_subagents = install_subagents;
        self
    }

    fn install_process_controls(mut self, install_process_controls: bool) -> Self {
        self.install_process_controls = install_process_controls;
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

    async fn build(self) -> Result<AgentScenarioRuntime> {
        let checkpoint_writes =
            lash_core::testing::checkpoint_observer::CheckpointWriteCollector::default();
        let graph_store = Arc::new(crate::tracing::TraceLashlangGraphStore::default());
        let prompt_captures = Arc::new(StdMutex::new(Vec::new()));
        let response_substitutions = Arc::new(StdMutex::new(Vec::new()));
        let provider = scripted_provider(
            self.scripted_provider_responses,
            self.scripted_provider_usage,
            Arc::clone(&prompt_captures),
            Arc::clone(&response_substitutions),
        );
        let observed_writes = checkpoint_writes.clone();
        let backend =
            DecoratedBackend::over(double_backend().await).session_store_factory(move |inner| {
                Arc::new(
                    lash_core::testing::checkpoint_observer::ObservedDeploymentStore::new(
                        inner,
                        observed_writes,
                    ),
                )
            });
        let backend: lash_core::Backend = backend.into();
        let tracing = lash_core::trace::TraceRuntime::new(backend.clock())
            .with_product_observer(graph_store.clone());
        let factory = rlm_factory(&backend);
        let store_factory = backend.session_store_factory();
        let turn_budget = self
            .max_turns
            .map_or(crate::TurnBudget::Unbounded, crate::TurnBudget::bounded);
        let mut builder = explicit_ephemeral_facets(LashCore::rlm_builder(backend, factory))
            .trace_runtime(tracing)
            .serve_test_llm_profile(provider, mock_llm_profile_spec());
        if let Some(tools) = self.tool_provider {
            builder = builder.tools(tools);
        }
        if self.install_subagents {
            builder = builder.plugin(subagents_plugin());
        }
        if self.install_process_controls {
            builder = builder.plugin(Arc::new(
                lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                    lash_core::lifetime::session_or_starter,
                ),
            ));
        }
        if self.install_llm_tools {
            builder = builder.plugin(Arc::new(lash_llm_tools::LlmToolsPluginFactory::default()));
        }
        let core = builder.build(crate::testing::runtime_lease_owner())?;
        serve_processes(&core);
        let process_registry = core.process_registry();
        Ok(AgentScenarioRuntime {
            core,
            spec: mock_session_spec().turn_budget(turn_budget),
            store_factory,
            graph_store,
            process_registry,
            prompt_captures,
            response_substitutions,
            checkpoint_writes,
        })
    }
}

struct AgentScenarioRuntime {
    core: LashCore,
    /// The spec the scenario's sessions are created from: the mock model
    /// under the setup's turn budget.
    spec: crate::SessionSpec,
    store_factory: Arc<dyn lash_core::DeploymentStore>,
    graph_store: Arc<crate::tracing::TraceLashlangGraphStore>,
    process_registry: Arc<dyn ProcessRegistry>,
    prompt_captures: Arc<StdMutex<Vec<LlmRequest>>>,
    /// Placeholders the scripted provider replaces in each response it serves,
    /// for values the harness learns only after the runtime is built, such as
    /// a minted process id (ADR 0107).
    response_substitutions: Arc<StdMutex<Vec<(String, String)>>>,
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
    super::transcript::assert_typed_checkpoint_transcript(&run.checkpoint_writes);
    Ok(run)
}

pub(super) async fn run_agent_turn_scenario_without_success_assertions(
    case: AgentScenario,
) -> Result<AgentScenarioRun> {
    let runtime = AgentScenarioSetup::new(case.scripted_provider_responses.clone())
        .install_subagents(case.install_subagents)
        .max_turns(case.max_turns)
        .build()
        .await?;
    let session = runtime
        .core
        .session(case.session_id.clone())
        .created_with(runtime.spec.clone())
        .await
        .open()
        .await?;
    if !case.seeded_attachment_writes.is_empty() {
        // Stand in for the writer that really uploaded these bytes. The
        // evidence a store keeps is per digest, not per session, so a
        // dedicated seeding session records it exactly as a peer writer would.
        let seed_session_id = SessionId::fixture(format!("{}-attachment-seed", case.session_id));
        let seed_store = lash_core::runtime::admit_session_view(
            &runtime.store_factory,
            &lash_core::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: seed_session_id.clone(),
                relation: lash_core::SessionRelation::Root,
                config: lash_core::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                )
                .into(),
                head: lash_core::SessionCreationHead::Config,
            },
        )
        .await?;
        for attachment_id in &case.seeded_attachment_writes {
            let write = lash_core::AttachmentWrite {
                attachment_id: attachment_id.clone(),
                claim: lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::Session(
                    seed_session_id.clone(),
                ))
                .expect("a session claim is unguarded"),
            };
            let lash_core::AttachmentWriteFence::Granted(permit) =
                lash_core::AttachmentReferrers::begin_attachment_write(
                    seed_store.store().as_ref(),
                    &write,
                )
                .await?
            else {
                panic!("a seeded attachment write must be granted");
            };
            lash_core::AttachmentReferrers::complete_attachment_write(
                seed_store.store().as_ref(),
                &write,
                permit,
            )
            .await?;
        }
    }
    if let Some(output) = case.precompleted_process.clone() {
        let process_id = runtime
            .process_registry
            .register_process_with_observers(
                lash_core::ProcessRegistration::new(
                    lash_core::ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    lash_core::ProcessProvenance::session(lash_core::SessionScope::new(
                        &case.session_id,
                    )),
                    lash_core::Lifetime::Detached,
                )
                .with_admitted_identity(
                    lash_core::AdmittedProcessIdentity::for_testing(
                        lash_core::ProcessIdentity::new("test.awaited-child"),
                    ),
                ),
                std::slice::from_ref(&case.session_id),
            )
            .await?
            .id;
        runtime
            .process_registry
            .complete_process(
                &process_id,
                output,
                lash_core::ProcessCompletionAuthority::external_owner(),
            )
            .await?;
        runtime.response_substitutions.lock_recover().push((
            PRECOMPLETED_PROCESS_HANDLE.to_string(),
            lash_core::HandleId::process(&process_id)
                .as_str()
                .to_string(),
        ));
    }
    let events = Arc::new(RecordingEvents::default());

    let turn_output = session
        .send(TurnInput::text(case.root_prompt))
        .output_into(events.as_ref())
        .await?;
    session.refresh_background_graph().await?;
    if !case.expects_refused_cell {
        assert_no_refused_cell(case.name, &events.snapshot().await);
    }
    let final_process_list = runtime.final_process_list().await?;
    assert_remote_process_dto_surface(
        &runtime.core,
        runtime.process_registry.as_ref(),
        &case.session_id,
    )
    .await;
    assert_remote_process_summaries_round_trip(&final_process_list);
    let checkpoint_writes = runtime.checkpoint_writes.events();
    // The turn opens with its start commit; the commit that settles it, the
    // session's last, is the one that persists its tool calls.
    let settling_revision = checkpoint_writes
        .iter()
        .filter(|write| write.session_id == case.session_id)
        .map(|write| write.revision_before)
        .max();
    let run = AgentScenarioRun {
        session_id: case.session_id.clone(),
        turn_output: Some(turn_output),
        streamed_events: events.snapshot().await,
        graph_snapshots: runtime.graph_store.graphs(),
        final_process_list,
        committed_attachment_ids: settling_revision
            .and_then(|revision| {
                runtime
                    .checkpoint_writes
                    .committed_attachment_ids(&case.session_id, revision)
            })
            .unwrap_or_default(),
        checkpoint_writes,
    };

    super::transcript::assert_typed_checkpoint_transcript(&run.checkpoint_writes);
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
        let TurnEvent::CodeBlockCompleted { error, .. } = &activity.event else {
            continue;
        };
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

async fn all_host_process_summaries(core: &LashCore) -> Result<Vec<lash_core::ProcessHandleView>> {
    let processes = core
        .processes()
        .list(&lash_core::ProcessListFilter {
            definition_id: None,
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
        process.identity.clone(),
        process.lifecycle,
    )
    .with_definition_id(process.identity.definition_id)
}

async fn assert_remote_process_dto_surface(
    core: &LashCore,
    registry: &dyn lash_core::ProcessRegistry,
    session_id: &SessionId,
) {
    let filter = lash_core::ProcessListFilter {
        definition_id: None,
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
        .session_snapshot((session_id).clone())
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

        let outcome = registry
            .event_page_after(
                &process_id,
                0,
                std::num::NonZeroUsize::new(32).expect("nonzero page limit"),
                lash_core::ProcessEventQueryMode::Full,
            )
            .await
            .expect("load process event page for remote DTO round trip");
        let lash_core::ProcessEventReadOutcome::Retained(page) = &outcome else {
            panic!("a live process's history is retained");
        };
        let lash_core::ProcessEventPageEvents::Full(page_events) = &page.events else {
            panic!("a full projection returns full events");
        };
        let expected_tail = page_events
            .iter()
            .map(|event| (event.sequence, event.event_type.clone()))
            .collect::<Vec<_>>();
        let expected_more = page.more.clone();
        let cursor = lash_sansio::ProcessCursor::new(
            lash_sansio::PROCESS_CURSOR_UNROUTED_EPOCH,
            lash_sansio::ProcessCursorReference::for_process(&process_id),
            0,
            page_events.last().map_or(0, |event| event.sequence),
        )
        .expect("scenario cursor");
        let remote_events = lash_remote_protocol::RemoteProcessEventsResponse::try_from((
            process_id.clone(),
            outcome,
            cursor.clone(),
        ))
        .expect("process events page serializes for the remote protocol");
        remote_events
            .validate()
            .expect("remote process event page should validate");
        let (round_trip_process_id, round_trip_outcome, round_trip_cursor): (
            lash_core::ProcessId,
            lash_core::ProcessEventReadOutcome<lash_core::ProcessEventPage>,
            lash_sansio::ProcessCursor,
        ) = remote_events
            .try_into()
            .expect("remote process event page should convert back");
        let lash_core::ProcessEventReadOutcome::Retained(round_trip_page) = &round_trip_outcome
        else {
            panic!("round trip preserves the retained page");
        };
        let lash_core::ProcessEventPageEvents::Full(round_trip_events) = &round_trip_page.events
        else {
            panic!("round trip preserves the full projection");
        };
        let round_trip_tail = round_trip_events
            .iter()
            .map(|event| (event.sequence, event.event_type.clone()))
            .collect::<Vec<_>>();
        assert_eq!(round_trip_process_id, process_id);
        assert_eq!(round_trip_cursor, cursor);
        assert_eq!(round_trip_tail, expected_tail);
        assert_eq!(round_trip_page.more, expected_more);
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
    .install_process_controls(true)
    .build()
    .await?;
    let session = runtime
        .core
        .session(
            crate::SessionId::parse("agent-scenario-process-llm-query")
                .expect("nonblank host identity"),
        )
        .created_with(runtime.spec.clone())
        .await
        .open()
        .await?;
    let result = session
        .send(TurnInput::text("Enrich the email in a durable process."))
        .output()
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
    .install_process_controls(true)
    .build()
    .await?;
    let session = runtime
        .core
        .session(
            crate::SessionId::parse("agent-scenario-direct-completion-attempt-retry")
                .expect("nonblank host identity"),
        )
        .created_with(runtime.spec.clone())
        .await
        .open()
        .await?;
    let result = session
        .send(TurnInput::text(
            "Retry the complete atomic tool attempt once.",
        ))
        .output()
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

/// The placeholder a scripted response writes for the handle id of the
/// scenario's precompleted process, whose id the registrar mints only after
/// the responses are scripted.
pub(super) const PRECOMPLETED_PROCESS_HANDLE: &str = "@precompleted-process-handle@";

fn scripted_provider(
    responses: Vec<String>,
    usage: LlmUsage,
    prompt_captures: Arc<StdMutex<Vec<LlmRequest>>>,
    substitutions: Arc<StdMutex<Vec<(String, String)>>>,
) -> ProviderHandle {
    let responses = Arc::new(TokioMutex::new(VecDeque::from(responses)));
    crate::testing::TestProvider::builder()
        .kind("agent-scenario")
        .complete(move |request| {
            let responses = Arc::clone(&responses);
            let usage = usage.clone();
            let prompt_captures = Arc::clone(&prompt_captures);
            let substitutions = Arc::clone(&substitutions);
            async move {
                prompt_captures.lock_recover().push(request.clone());
                let Some(mut text) = responses.lock().await.pop_front() else {
                    return Err(lash_core::llm::transport::LlmTransportError::new(
                        "scripted agent scenario provider exhausted its expected responses",
                    ));
                };
                for (placeholder, value) in substitutions.lock_recover().iter() {
                    text = text.replace(placeholder.as_str(), value);
                }
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
    Arc::new(lash_subagents::SubagentsPluginFactory::new(
        Arc::new(lash_subagents::CapabilityRegistry::new().with(Arc::new(
            lash_subagents::StaticCapability::new("default", SessionSpec::inherit()),
        ))),
        lash_core::lifetime::starter,
    ))
}
