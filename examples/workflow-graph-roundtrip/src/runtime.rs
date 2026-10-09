use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash::LashCore;
use lash::persistence::ProcessStartReceipt;
use lash::process::*;
use lash::tracing::TraceLanguageExecutionPayload;
use lash::vm::ir::{
    AssignPathStep, Expr, WorkflowDeclaration, WorkflowNodeId, WorkflowProjection,
    lifted_process_identity,
};
use lash::workflow::{
    WorkflowCorrespondence, WorkflowCorrespondenceEntry, WorkflowDocumentRead, WorkflowDraft,
    WorkflowDraftHandle, WorkflowEntry, WorkflowExecutionDocument,
    WorkflowExecutionOverlayAccumulator, WorkflowGraph, WorkflowOverlayOccurrence,
    WorkflowOverlaySettlement, WorkflowOverlayTerminal, WorkflowPublish,
};
use tokio::sync::mpsc;
use tokio_stream::StreamExt as _;

use crate::{DisplayDelta, DisplayState, RunEvent, RunStatus};

#[derive(Debug, thiserror::Error)]
pub(crate) enum RunError {
    #[error(transparent)]
    Lash(#[from] lash::EmbedError),
    #[error(transparent)]
    Refused(#[from] lash::workflow::WorkflowAdmissionRefusal),
    #[error(transparent)]
    Overlay(#[from] lash::workflow::WorkflowOverlayFoldError),
    #[error(transparent)]
    Display(#[from] lash::vm::ExecutionHostError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Graph(#[from] lash::workflow::WorkflowGraphError),
    #[error("{0}")]
    Invalid(String),
}

/// The environment a process of a saved workflow runs under, which is the
/// environment lash admits the workflow against.
fn environment() -> ProcessExecutionEnvSpec {
    ProcessExecutionEnvSpec::new(
        lash::plugins::AdmittedPluginConfig::default(),
        lash::runtime::SessionPolicy::new(
            lash::TurnBudget::bounded(32),
            lash::MaxToolCalls::new(1024),
            lash::NoProgressBudget::bounded(12),
        ),
        lash::plugins::SessionToolAccess::ambient(),
    )
}

/// A saved version as lash holds it: the definition a run starts, the
/// environment it starts under, and the pin that retains both.
#[derive(Clone)]
pub(crate) struct Published {
    definition: ProcessDefinition,
    env_ref: ProcessExecutionEnvRef,
    pub(crate) pin: HostArtifactPin,
}

impl Published {
    pub(crate) fn start_request(&self, key: &str) -> ProcessStartRequest {
        ProcessStartRequest::new(
            ProcessStartTarget::Definition {
                definition_id: self.definition.id.clone(),
                signature_claim: Some(self.definition.signature.clone()),
                args: Default::default(),
            },
            ProcessOriginator::host(),
            Lifetime::Detached,
        )
        .with_host_start_key(key)
        .with_env_ref(self.env_ref.clone())
    }
}

/// A draft lash admitted: what it published, the admitted document, and the
/// admitted id of every node the draft still holds.
pub(crate) struct Publication {
    pub(crate) published: Published,
    pub(crate) graph: WorkflowGraph,
    pub(crate) ids: BTreeMap<WorkflowDraftHandle, WorkflowNodeId>,
}

/// Where each node a correspondence still holds ended up.
pub(crate) fn surviving_ids(
    correspondence: &WorkflowCorrespondence,
) -> BTreeMap<WorkflowDraftHandle, WorkflowNodeId> {
    let mut ids = BTreeMap::new();
    for entry in &correspondence.entries {
        match entry {
            WorkflowCorrespondenceEntry::Retained { handle, to, .. }
            | WorkflowCorrespondenceEntry::Moved { handle, to, .. }
            | WorkflowCorrespondenceEntry::Inserted { handle, to, .. } => {
                ids.insert(*handle, to.clone());
            }
            WorkflowCorrespondenceEntry::Split { into, .. } => {
                ids.extend(into.iter().cloned());
            }
            _ => {}
        }
    }
    ids
}

/// The host opens one top-level workflow for editing. Inline processes inside
/// it remain part of its document, but are not the workflow selected to run.
/// A document with several top-level workflows needs an explicit host choice.
pub(crate) fn select_entry(draft: &WorkflowDraft) -> Result<WorkflowEntry, RunError> {
    let program = lash::workflow::workflow_program_from_graph(draft.document())?;
    let main = WorkflowProjection::for_main(&program);
    // Source-authored drafts still bind process literals; admitted documents
    // bind references instead. The projection supplies the literal's exact
    // site, so its container is resolved without walking nested process bodies.
    let bound_in_main = main
        .body()
        .statements
        .iter()
        .filter_map(|statement| {
            let (expression, path) = match statement.expr {
                Expr::LabelAnnotated { expr, .. } => (expr.as_ref(), statement.ast_path.child(0)),
                expression => (expression, statement.ast_path.clone()),
            };
            let Expr::Assign { target, expr } = expression else {
                return None;
            };
            let value_index = target
                .steps
                .iter()
                .filter(|step| matches!(step, AssignPathStep::Index(_)))
                .count() as u32;
            match expr.as_ref() {
                Expr::ProcessRef { process } => Some(process.to_string()),
                Expr::ProcessLiteral(literal) => Some(lifted_process_identity(
                    &literal.body,
                    &path.child(value_index).steps,
                )),
                _ => None,
            }
        })
        .collect::<BTreeSet<_>>();
    let mut workflows = draft
        .document()
        .declarations
        .iter()
        .filter_map(|declaration| {
            let WorkflowDeclaration::Process(process) = declaration else {
                return None;
            };
            (process.origin.is_declared() || bound_in_main.contains(&process.name))
                .then_some(process)
        });
    match (workflows.next(), workflows.next()) {
        (Some(process), None) => Ok(WorkflowEntry::Process(process.id.clone())),
        _ => Err(RunError::Invalid(
            "open a document with one top-level workflow to select its entry".into(),
        )),
    }
}

/// Publishes the host-selected `entry` of `draft` under a pin of its own.
/// Lash admits the document's IR against the run environment in its VM workers.
pub(crate) async fn publish(
    core: &LashCore,
    draft: &WorkflowDraft,
    entry: WorkflowEntry,
) -> Result<Publication, RunError> {
    let pin = HostArtifactPin::mint();
    let artifacts = core.host_artifacts();
    let environment = environment();
    let result = async {
        match artifacts
            .publish_workflow(&pin, draft, entry, &environment)
            .await?
        {
            WorkflowPublish::Published(publication) => {
                let env_ref = artifacts.publish_process_env(&pin, &environment).await?;
                Ok(Publication {
                    published: Published {
                        definition: publication.definition,
                        env_ref,
                        pin: pin.clone(),
                    },
                    graph: publication.document.graph,
                    ids: surviving_ids(&publication.correspondence),
                })
            }
            WorkflowPublish::Refused(refusal) => Err(RunError::Refused(refusal)),
            WorkflowPublish::Unsupported { engine_kind } => Err(RunError::Invalid(format!(
                "engine `{engine_kind}` admits no workflow documents"
            ))),
        }
    }
    .await;
    if result.is_err() {
        artifacts.release(pin).await?;
    }
    result
}

/// Follows `process` on its one recovering feed and sends what the execution
/// overlay shows of it until the process ends or the receiver goes away.
pub(crate) async fn observe(
    core: LashCore,
    process: lash::ProcessId,
    workflow_version: u64,
    sender: mpsc::Sender<Result<RunEvent, RunError>>,
    host: Arc<crate::display::HostTools>,
) -> Result<(), RunError> {
    let observed = core.processes().observe(&process);
    let snapshot = observed.snapshot().await?;
    // The feed retains everything published while the document is read.
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    let document = execution_document(&core, &snapshot.read_view).await?;
    let mut overlay = Overlay::new(&document, process, workflow_version, host)?;
    let (mut events, mut terminal) = overlay.snapshot(snapshot.read_view)?;
    loop {
        for event in events {
            if sender.send(Ok(event)).await.is_err() {
                return Ok(());
            }
        }
        if terminal {
            return Ok(());
        }
        let item = tokio::select! {
            _ = sender.closed() => return Ok(()),
            item = feed.next() => item,
        };
        let Some(item) = item else {
            return Err(RunError::Invalid(
                "the process observation feed ended".into(),
            ));
        };
        (events, terminal) = match item? {
            ProcessObservationStreamItem::Event(event) => match &event.payload {
                ProcessObservationEventPayload::LanguageExecution(observation) => {
                    (overlay.language_observation(observation)?, false)
                }
                // The admitted body of a tool step started: its site runs.
                ProcessObservationEventPayload::StepBodyStarted(observation) => {
                    overlay.accumulator.step_body_started(observation)?;
                    (overlay.changed(), false)
                }
                ProcessObservationEventPayload::Committed { event } => (
                    overlay.durable(event)?,
                    matches!(event.fact, ProcessLifecycleFact::Terminal { .. }),
                ),
            },
            ProcessObservationStreamItem::Gap { observation, .. } => {
                // A gap retires provisional history. Applied display operations
                // remain host state; retained effect evidence rebuilds bindings.
                overlay.accumulator.reset_live();
                overlay.completed_calls.clear();
                overlay.snapshot(observation.read_view)?
            }
        };
    }
}

/// The document the process runs, read from lash by the reference the
/// process's own snapshot names.
async fn execution_document(
    core: &LashCore,
    view: &ProcessReadView,
) -> Result<WorkflowExecutionDocument, RunError> {
    let ProcessReadView::Retained(view) = view else {
        return Err(RunError::Invalid(
            "the process is no longer retained".into(),
        ));
    };
    let ProcessDocumentIdentity::Available(reference) = &view.document else {
        return Err(RunError::Invalid(format!(
            "the run names no workflow document: {:?}",
            view.document
        )));
    };
    match core.host_artifacts().execution_document(reference).await? {
        WorkflowDocumentRead::Read(document) => Ok(*document),
        unreadable => Err(RunError::Invalid(format!(
            "the run's workflow document cannot be read: {unreadable:?}"
        ))),
    }
}

fn settlement(
    status: ProcessStatus,
    occurred_at_ms: Option<u64>,
) -> Option<WorkflowOverlaySettlement> {
    let terminal = match status {
        ProcessStatus::Completed => WorkflowOverlayTerminal::Completed,
        ProcessStatus::Failed => WorkflowOverlayTerminal::Failed,
        ProcessStatus::Cancelled => WorkflowOverlayTerminal::Cancelled,
        ProcessStatus::Abandoned => WorkflowOverlayTerminal::Abandoned,
        _ => return None,
    };
    Some(WorkflowOverlaySettlement {
        terminal,
        occurred_at: occurred_at_ms
            .and_then(|at| i64::try_from(at).ok())
            .and_then(chrono::DateTime::from_timestamp_millis),
    })
}

/// One run as this host shows it: lash's execution overlay of the process,
/// reduced to a status per node, beside the host's own display state.
struct Overlay {
    accumulator: WorkflowExecutionOverlayAccumulator,
    process: lash::ProcessId,
    workflow_version: u64,
    definition: String,
    root_node: String,
    /// What each node was last sent as: its status and how many of its
    /// occurrences had started and ended by then.
    shown: BTreeMap<String, (RunStatus, u64, u64)>,
    sequence: u64,
    display: DisplayState,
    completed_calls: BTreeMap<String, String>,
    delivered: BTreeSet<String>,
    host: Arc<crate::display::HostTools>,
}

impl Overlay {
    fn new(
        document: &WorkflowExecutionDocument,
        process: lash::ProcessId,
        workflow_version: u64,
        host: Arc<crate::display::HostTools>,
    ) -> Result<Self, RunError> {
        let definition = document
            .graph
            .source_identity
            .clone()
            .ok_or_else(|| RunError::Invalid("the run's graph names no artifact".into()))?;
        let root_node = document
            .entry
            .as_deref()
            .and_then(|entry| document.graph.process(entry))
            .ok_or_else(|| RunError::Invalid("the run's process has no graph".into()))?
            .id
            .to_string();
        let mut accumulator = WorkflowExecutionOverlayAccumulator::default();
        accumulator.set_document(document.overlay_document());
        Ok(Self {
            accumulator,
            process,
            workflow_version,
            definition,
            root_node,
            shown: BTreeMap::new(),
            sequence: 0,
            display: DisplayState::default(),
            completed_calls: BTreeMap::new(),
            delivered: BTreeSet::new(),
            host,
        })
    }

    fn event(
        &mut self,
        node_id: String,
        status: RunStatus,
        display_delta: DisplayDelta,
        error: Option<String>,
    ) -> RunEvent {
        self.sequence += 1;
        RunEvent {
            run_id: self.process.to_string(),
            workflow_version: self.workflow_version,
            definition: self.definition.clone(),
            sequence: self.sequence,
            node_id,
            status,
            display_delta,
            display: self.display.clone(),
            error,
            approval_key: None,
        }
    }

    /// The nodes whose state in lash's overlay changed since they were last
    /// sent. A node shows its site in flight, else its latest ended one.
    fn changed(&mut self) -> Vec<RunEvent> {
        let Some(overlay) = self.accumulator.snapshot() else {
            return Vec::new();
        };
        let mut nodes = BTreeMap::<&str, (u8, RunStatus, Option<String>, u64, u64)>::new();
        for site in &overlay.sites {
            let (rank, status, error) = match &site.occurrence {
                WorkflowOverlayOccurrence::Unobserved => continue,
                WorkflowOverlayOccurrence::Running { .. } => (3, RunStatus::Started, None),
                WorkflowOverlayOccurrence::Waiting { .. } => (3, RunStatus::Waiting, None),
                WorkflowOverlayOccurrence::Failed { failure, .. } => {
                    (2, RunStatus::Failed, Some(failure.message().to_owned()))
                }
                WorkflowOverlayOccurrence::Cancelled { .. } => {
                    (2, RunStatus::Failed, Some("cancelled".to_owned()))
                }
                WorkflowOverlayOccurrence::Completed { .. } => (1, RunStatus::Succeeded, None),
                WorkflowOverlayOccurrence::Incomplete { terminal, .. } => match terminal {
                    WorkflowOverlayTerminal::Completed => (1, RunStatus::Succeeded, None),
                    _ => (2, RunStatus::Failed, Some("the run ended first".to_owned())),
                },
            };
            let node = nodes
                .entry(site.site.node_id.as_str())
                .or_insert((0, status, None, 0, 0));
            if rank > node.0 {
                (node.0, node.1, node.2) = (rank, status, error);
            }
            node.3 += site.summary.started_count;
            node.4 += site.summary.terminal_count;
        }
        let mut events = Vec::new();
        for (node, (_, status, error, started, ended)) in nodes {
            let shown = (status, started, ended);
            if self.shown.get(node) == Some(&shown) {
                continue;
            }
            self.shown.insert(node.to_owned(), shown);
            events.push(self.event(node.to_owned(), status, DisplayDelta::default(), error));
        }
        events
    }

    fn durable(&mut self, event: &ObservedProcessEvent) -> Result<Vec<RunEvent>, RunError> {
        let mut events = Vec::new();
        match &event.fact {
            ProcessLifecycleFact::EffectOutcome(occurrence) => {
                events.extend(self.effect(occurrence));
                events.extend(self.deliver_display()?);
            }
            ProcessLifecycleFact::Waiting { wait } => events.push(self.waiting(wait)),
            ProcessLifecycleFact::Terminal { outcome, .. } => {
                let status = if outcome.status() == TerminalProcessStatus::Completed {
                    RunStatus::Succeeded
                } else {
                    RunStatus::Failed
                };
                // The committed end settles what the overlay still shows in
                // flight; it never invents an execution of an untouched node.
                if let Some(settlement) =
                    settlement(outcome.status().into(), Some(event.occurred_at_ms))
                {
                    self.accumulator.settle(settlement);
                }
                events.extend(self.changed());
                events.extend(self.deliver_display()?);
                events.push(self.event(
                    self.root_node.clone(),
                    status,
                    DisplayDelta::default(),
                    (status == RunStatus::Failed).then(|| format!("{outcome:?}")),
                ));
            }
            _ => {}
        }
        Ok(events)
    }

    /// A committed effect outcome: durable evidence of one call of a node,
    /// which outlives the provisional overlay.
    fn effect(&mut self, occurrence: &ProcessEffectOccurrence) -> Vec<RunEvent> {
        let status = match occurrence.outcome_class {
            ProcessEffectOutcomeClass::Success => {
                if let Some(call) = &occurrence.call_id {
                    self.completed_calls
                        .insert(call.to_string(), occurrence.node_id.clone());
                }
                RunStatus::Succeeded
            }
            ProcessEffectOutcomeClass::Failure | ProcessEffectOutcomeClass::Cancelled => {
                RunStatus::Failed
            }
        };
        vec![self.event(
            occurrence.node_id.clone(),
            status,
            DisplayDelta::default(),
            occurrence.code.as_ref().map(ToString::to_string),
        )]
    }

    fn waiting(&mut self, wait: &WaitState) -> RunEvent {
        let mut event = self.event(
            self.root_node.clone(),
            RunStatus::Waiting,
            DisplayDelta::default(),
            None,
        );
        if let WaitKind::Call { call_id, .. } = &wait.kind {
            event.approval_key = self.host.approval_key(&self.process, call_id.as_str());
        }
        event
    }

    fn deliver_display(&mut self) -> Result<Vec<RunEvent>, RunError> {
        let mut events = Vec::new();
        for operation in self.host.display_calls(&self.process) {
            if self.delivered.contains(&operation.call_id) {
                continue;
            }
            let Some(node) = self.completed_calls.get(&operation.call_id).cloned() else {
                break;
            };
            self.delivered.insert(operation.call_id.clone());
            let (_, delta) = crate::display::apply_tool(
                &mut self.display,
                &operation.operation,
                &[lash::vm::from_json(operation.args)],
            )?;
            events.push(self.event(node, RunStatus::Succeeded, delta, None));
        }
        Ok(events)
    }

    fn snapshot(&mut self, view: ProcessReadView) -> Result<(Vec<RunEvent>, bool), RunError> {
        let ProcessReadView::Retained(view) = view else {
            return Err(RunError::Invalid(
                "the process is no longer retained".into(),
            ));
        };
        let mut events = Vec::new();
        for node in view.effects.report.nodes() {
            for occurrence in &node.occurrences {
                events.extend(self.effect(occurrence));
            }
        }
        events.extend(self.deliver_display()?);
        if let Some(settlement) = settlement(
            view.process.status(),
            view.process.lifecycle.terminal_at_ms(),
        ) {
            self.accumulator.settle(settlement);
        }
        events.extend(self.changed());
        let status = match view.process.status() {
            ProcessStatus::Completed => RunStatus::Succeeded,
            ProcessStatus::Failed | ProcessStatus::Cancelled | ProcessStatus::Abandoned => {
                RunStatus::Failed
            }
            ProcessStatus::Waiting => RunStatus::Waiting,
            _ => RunStatus::Started,
        };
        let terminal = matches!(status, RunStatus::Succeeded | RunStatus::Failed);
        if status == RunStatus::Waiting {
            for wait in view.process.waits() {
                events.push(self.waiting(wait));
            }
        } else {
            events.push(
                self.event(
                    self.root_node.clone(),
                    status,
                    DisplayDelta::default(),
                    view.process
                        .terminal()
                        .filter(|_| status == RunStatus::Failed)
                        .map(|outcome| format!("{outcome:?}")),
                ),
            );
        }
        Ok((events, terminal))
    }

    fn language_observation(
        &mut self,
        observation: &LanguageExecutionObservation,
    ) -> Result<Vec<RunEvent>, RunError> {
        // The engine resumes the VM with a settled step's recorded output.
        // NodeCompleted identifies that logical call even after the bounded
        // effect summary stops carrying individual loop occurrences.
        if let TraceLanguageExecutionPayload::NodeCompleted {
            node_id,
            call_id: Some(call),
            ..
        } = &observation.execution.payload
        {
            self.completed_calls
                .insert(call.to_string(), node_id.clone());
        }
        self.accumulator.observe(observation)?;
        let mut events = self.changed();
        events.extend(self.deliver_display()?);
        Ok(events)
    }
}

pub(crate) fn host_environment() -> lash::vm::LashVmHostEnvironment {
    crate::operations::host_environment()
}

/// The example's engine and the host tools whose effect ledger it observes.
#[derive(Clone)]
pub struct WorkflowHost {
    pub(crate) core: LashCore,
    pub(crate) tools: Arc<crate::display::HostTools>,
}

impl WorkflowHost {
    pub fn core(&self) -> &LashCore {
        &self.core
    }
}

pub fn core(backend: lash::Backend) -> lash::Result<WorkflowHost> {
    let tools = Arc::new(crate::display::HostTools::default());
    let mut config = lash::rlm::RlmProtocolPluginConfig::builder()
        .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
        .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
        .channel(lash::rlm::RlmChannel::Cell)
        .build();

    config.lash_vm_language_features = lash::vm::LashVmLanguageFeatures::default()
        .with_label_annotations()
        .into();
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        config,
        Arc::new(lash::rlm::TypescriptDialect),
        &backend,
    );
    let core = LashCore::rlm_builder(backend, factory)
        .tools(Arc::new(lash::tools::StaticToolProvider::new(
            crate::operations::tool_definitions(),
            tools.as_ref().clone(),
        )))
        .trace_level(lash::tracing::TraceLevel::Extended)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "workflow-graph",
            uuid::Uuid::new_v4().to_string(),
        ))?;
    Ok(WorkflowHost { core, tools })
}

/// Starts runs through the engine's process admission API.
#[derive(Clone)]
pub(crate) struct CommandClient(lash::LashCore);

impl CommandClient {
    pub(crate) fn new(core: lash::LashCore) -> Self {
        Self(core)
    }

    pub(crate) async fn start(
        &self,
        request: ProcessStartRequest,
    ) -> Result<ProcessStartReceipt, RunError> {
        Ok(self
            .0
            .processes()
            .start(request, self.0.effect_host())
            .await?)
    }
}
