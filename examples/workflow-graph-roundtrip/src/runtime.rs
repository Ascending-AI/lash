use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash::LashCore;
use lash::persistence::ProcessStartReceipt;
use lash::process::*;
use lash::tracing::{TraceLanguageExecutionPayload, TraceNodeFact};
use lash::workflow::document::{Document, EffectIdentity, Name, Site, TaskIdentity};
use lash::workflow::edit::Draft;
use lash::workflow::view::{ActionObservation, ActionState, WorkflowCoverage, WorkflowView};
use lash::workflow::{
    ProcessWorkflowOverlay, ReplaySegments, SegmentAnswer, WorkflowDocument, WorkflowDocumentRead,
    WorkflowEnvironment, WorkflowExecutionOverlayAccumulator, WorkflowOverlayFact,
    WorkflowOverlayOccurrence, WorkflowOverlaySettlement, WorkflowOverlayTerminal, WorkflowPublish,
};
use tokio::sync::mpsc;
use tokio_stream::StreamExt as _;

use crate::{DisplayDelta, DisplayState, RunEvent, RunPath, RunStatus};

#[derive(Debug, thiserror::Error)]
pub(crate) enum RunError {
    #[error(transparent)]
    Lash(#[from] lash::EmbedError),
    #[error("{0}")]
    Refused(lash::workflow::WorkflowAdmissionRefusal),
    #[error(transparent)]
    Overlay(#[from] lash::workflow::WorkflowOverlayFoldError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Environment(#[from] ProcessEnvRefusal),
    #[error(transparent)]
    Activity(#[from] lash::workflow::view::ActivityRefusal),
    #[error("{0}")]
    Invalid(String),
}

/// The process component that contributes the host's tools to a workflow
/// process: its config is `()`.
const HOST_TOOLS: ProcessConfigKey<()> =
    ProcessConfigKey::new("workflow-host-tools", lash::plugins::FormatVersion::ONE);

/// The environment a process of a saved workflow runs under, which is the
/// environment lash admits the workflow against: the host's tools, from the
/// one component that provides them.
pub(crate) fn environment() -> Result<ProcessEnv, RunError> {
    let host_tools = HOST_TOOLS.tool_provider(lash::tool_provider_key!("tools"));
    Ok(crate::display::tool_definitions()
        .iter()
        .fold(
            ProcessEnv::builder(lash::MaxToolCalls::new(1024)).component(HOST_TOOLS, &()),
            |env, tool| env.link_tool(host_tools.clone(), tool),
        )
        .build()?)
}

/// A saved version as lash holds it: the definition a run starts, the
/// environment it starts under, and the pin that retains both.
#[derive(Clone)]
pub(crate) struct Published {
    pub(crate) definition: ProcessDefinition,
    launch: EngineLaunch,
    pub(crate) pin: HostArtifactPin,
}

impl Published {
    pub(crate) fn start_request(
        &self,
        key: &str,
        inputs: serde_json::Map<String, serde_json::Value>,
    ) -> ProcessStartRequest {
        ProcessStartRequest::definition(
            self.definition.id.clone(),
            Some(self.definition.signature.clone()),
            inputs,
            self.launch.clone(),
            ProcessOriginator::host(),
            Lifetime::Detached,
        )
        .with_host_start_key(key)
    }
}

/// A draft Lash admitted and pinned for execution.
pub(crate) struct Publication {
    pub(crate) published: Published,
}

/// What this host offers a workflow document: its effects with their
/// signatures, and its library functions.
pub(crate) async fn workflow_environment(core: &LashCore) -> Result<WorkflowEnvironment, RunError> {
    core.host_artifacts()
        .workflow_environment(&environment()?)
        .await?
        .ok_or_else(|| RunError::Invalid("this core reads no workflow documents".into()))
}

/// `document` with the manifest this host admits it under: every effect
/// carries the signature the host offers it under, and every library
/// function the code reaches is listed by identity.
pub(crate) fn complete(
    mut document: Document,
    environment: &WorkflowEnvironment,
) -> Result<Document, RunError> {
    for (effect, signature) in &mut document.manifest.effects {
        *signature = environment
            .effects()
            .get(effect)
            .ok_or_else(|| RunError::Invalid(format!("this host offers no effect `{effect}`")))?
            .clone();
    }
    document.manifest.functions =
        lash::workflow::graph::requirements(&document, environment.functions()).functions;
    Ok(document)
}

/// Publishes entry `entry` of `draft` under a pin of its own. Lash admits
/// the document against the run environment.
pub(crate) async fn publish(
    core: &LashCore,
    draft: &Draft,
    entry: &Name,
) -> Result<Publication, RunError> {
    let pin = HostArtifactPin::mint();
    let artifacts = core.host_artifacts();
    let environment = environment()?;
    // Choose run counts once, with the settings the published start records.
    let settings = Some(
        core.processes()
            .kernel_engine_defaults()?
            .observing(lash::process::Observe::Counts)
            .into_engine_settings(),
    );
    let result = async {
        let offered = artifacts
            .workflow_environment(&environment)
            .await?
            .ok_or_else(|| RunError::Invalid("this core reads no workflow documents".into()))?;
        let document = draft.document();
        let graph = lash::workflow::graph::derive(document, offered.functions())
            .map_err(|refusal| RunError::Invalid(refusal.to_string()))?;
        let effects =
            lash::workflow::graph::entry_effects(document, &graph, offered.functions(), entry)
                .map_err(|refusal| RunError::Invalid(refusal.to_string()))?;
        // The editor offers all host effects; each published pair links only
        // the effects its entry calls or spawns, including called functions.
        let environment = environment.restrict(effects.keys())?;
        match artifacts
            .publish_workflow(&pin, draft.document(), entry, &environment, None)
            .await?
        {
            WorkflowPublish::Published(publication) => {
                let env_ref = artifacts.publish_process_env(&pin, &environment).await?;
                Ok(Publication {
                    published: Published {
                        definition: publication.definition,
                        launch: EngineLaunch::new(env_ref, settings),
                        pin: pin.clone(),
                    },
                })
            }
            WorkflowPublish::Refused(refusal) => Err(RunError::Refused(refusal)),
            WorkflowPublish::SourceMapRefused(refusal) => Err(RunError::Invalid(format!(
                "the source map is refused: {refusal}"
            ))),
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
    let mut workflow = process_overlay(&snapshot.read_view);
    let (mut events, mut terminal) = overlay.snapshot(snapshot.read_view)?;
    loop {
        let coverage = overlay.coverage(&document, workflow.as_ref(), terminal);
        if terminal {
            // The run's own end, which the overlay's exact counts and the
            // replayed path complete.
            let path = run_path(&core, &overlay.process, &document).await?;
            if let Some(last) = events.last_mut() {
                (last.coverage, last.path) = (coverage, Some(path));
            }
        } else if let Some(coverage) = coverage {
            let mut event = overlay.event(None, RunStatus::Started, DisplayDelta::default(), None);
            event.call_only = true;
            event.coverage = Some(coverage);
            events.push(event);
        }
        for mut event in events {
            if let Some(call_id) = &event.call_id
                && let Some(record) = core
                    .processes()
                    .tool_call(&overlay.process, call_id)
                    .await?
            {
                event.status = if record.output.is_success() {
                    RunStatus::Succeeded
                } else {
                    RunStatus::Failed
                };
                event.call = Some(crate::contract::CallData {
                    input: record.args,
                    output: record.output.value_for_projection(),
                });
                if let Some(identity) = event.occurrence.clone()
                    && overlay.view.action_node(&identity.site).is_some()
                {
                    let evidence = ActionObservation {
                        identity: identity.clone(),
                        state: if record.output.is_success() {
                            ActionState::Completed
                        } else {
                            ActionState::Failed
                        },
                        outcome: event.call.as_ref().map(|call| call.output.clone()),
                    };
                    event.activity = overlay
                        .view
                        .activity(overlay.view.document, &[evidence])?
                        .into_iter()
                        .find(|activity| activity.site == identity.site);
                }
            }
            if event.coverage.is_some()
                && let Some(workflow) = workflow.as_ref()
            {
                event.node_coverage = Some(overlay.view.coverage(&workflow.ran_counts()));
            }
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
        let item = item?;
        // Counts change with a run segment's live delta, its committed
        // delta and the terminal; lash's process overlay folds them, so
        // read it again rather than counting here.
        let recount = match &item {
            ProcessObservationStreamItem::Event(event) => match &event.payload {
                ProcessObservationEventPayload::LanguageExecution(observation) => matches!(
                    observation.execution.payload,
                    TraceLanguageExecutionPayload::Ran { .. }
                ),
                ProcessObservationEventPayload::Committed { event } => matches!(
                    event.fact,
                    ProcessLifecycleFact::Ran(_) | ProcessLifecycleFact::Terminal { .. }
                ),
                ProcessObservationEventPayload::StepBodyStarted(_) => false,
            },
            ProcessObservationStreamItem::Gap { .. } => false,
        };
        if recount {
            workflow = process_overlay(&observed.snapshot().await?.read_view);
        }
        (events, terminal) = match item {
            ProcessObservationStreamItem::Event(event) => match &event.payload {
                ProcessObservationEventPayload::LanguageExecution(observation) => {
                    (overlay.language_observation(observation)?, false)
                }
                // The admitted body of a tool step started: its site runs.
                ProcessObservationEventPayload::StepBodyStarted(observation) => {
                    (overlay.step_body_started(observation)?, false)
                }
                ProcessObservationEventPayload::Committed { event } => (
                    overlay.durable(event)?,
                    matches!(event.fact, ProcessLifecycleFact::Terminal { .. }),
                ),
            },
            ProcessObservationStreamItem::Gap { replacement, .. } => {
                let read_view = replacement.into_read_view();
                workflow = process_overlay(&read_view);
                // A gap retires provisional history. Applied display operations
                // remain host state; retained effect evidence rebuilds bindings.
                for reset in overlay.reset_after_gap() {
                    if sender.send(Ok(reset)).await.is_err() {
                        return Ok(());
                    }
                }
                overlay.snapshot(read_view)?
            }
        };
    }
}

/// lash's workflow overlay of a retained process.
fn process_overlay(view: &ProcessReadView) -> Option<ProcessWorkflowOverlay> {
    match view {
        ProcessReadView::Retained(view) => view.workflow.clone(),
        _ => None,
    }
}

/// The path a finished run took through `document`'s entry: each segment
/// lash's replay verified, through `workflow::view::path`, in segment order.
async fn run_path(
    core: &LashCore,
    process: &lash::ProcessId,
    document: &WorkflowDocument,
) -> Result<RunPath, RunError> {
    let mut segments = core
        .processes()
        .replay(process, ReplaySegments::All)
        .await?;
    segments.sort_by_key(|segment| segment.segment);
    let mut steps = Vec::new();
    for segment in segments {
        match segment.answer {
            SegmentAnswer::Verified { path } => {
                steps.extend(lash::workflow::view::path(
                    document.document(),
                    &document.entry_unit(),
                    &path,
                ));
            }
            unverified => {
                return Ok(RunPath::Unavailable {
                    reason: format!("segment {}: {unverified:?}", segment.segment),
                });
            }
        }
    }
    Ok(RunPath::Verified { steps })
}

/// The document the process runs, read from lash by the reference the
/// process's own snapshot names.
async fn execution_document(
    core: &LashCore,
    view: &ProcessReadView,
) -> Result<WorkflowDocument, RunError> {
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
        WorkflowDocumentRead::Read(read) => Ok(read.document),
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
/// reduced to a status per site, beside the host's own display state.
struct Overlay {
    view: WorkflowView,
    accumulator: WorkflowExecutionOverlayAccumulator,
    identities: BTreeMap<(Site, TaskIdentity), EffectIdentity>,
    process: lash::ProcessId,
    workflow_version: u64,
    definition: String,
    /// What each site was last sent as: its status and how many of its
    /// occurrences had started and ended by then.
    shown: BTreeMap<Site, (RunStatus, u64, u64)>,
    sequence: u64,
    display: DisplayState,
    completed_calls: BTreeMap<String, Site>,
    delivered: BTreeSet<String>,
    host: Arc<crate::display::HostTools>,
    /// The coverage last sent.
    coverage: Option<WorkflowCoverage>,
}

impl Overlay {
    fn new(
        document: &WorkflowDocument,
        process: lash::ProcessId,
        workflow_version: u64,
        host: Arc<crate::display::HostTools>,
    ) -> Result<Self, RunError> {
        let definition = document.identity().to_string();
        let mut accumulator = WorkflowExecutionOverlayAccumulator::default();
        accumulator.set_document(lash::workflow::WorkflowOverlayDocument::of(document));
        Ok(Self {
            view: lash::workflow::view::derive(document)
                .map_err(|error| RunError::Invalid(error.to_string()))?,
            accumulator,
            identities: BTreeMap::new(),
            process,
            workflow_version,
            definition,
            shown: BTreeMap::new(),
            sequence: 0,
            display: DisplayState::default(),
            completed_calls: BTreeMap::new(),
            delivered: BTreeSet::new(),
            host,
            coverage: None,
        })
    }

    /// An event at `site`, or about the run as a whole when there is none.
    fn event(
        &mut self,
        site: Option<Site>,
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
            reset: false,
            call_only: false,
            site,
            status,
            display_delta,
            display: self.display.clone(),
            error,
            approval_key: None,
            call_id: None,
            call: None,
            output: None,
            coverage: None,
            path: None,
            activity: None,
            occurrence: None,
            node_coverage: None,
        }
    }

    /// The run's coverage of `document`'s entry from lash's process overlay
    /// `workflow`, when it changed since it was last sent or `always`.
    fn coverage(
        &mut self,
        document: &WorkflowDocument,
        workflow: Option<&ProcessWorkflowOverlay>,
        always: bool,
    ) -> Option<WorkflowCoverage> {
        let coverage = lash::workflow::view::coverage(
            document.document(),
            &document.entry_unit(),
            &workflow?.ran_counts(),
        );
        if self.coverage.as_ref() == Some(&coverage) && !always {
            return None;
        }
        self.coverage = Some(coverage.clone());
        Some(coverage)
    }

    fn reset_after_gap(&mut self) -> Vec<RunEvent> {
        self.accumulator.reset_live();
        self.identities.clear();
        self.completed_calls.clear();
        self.shown.clear();
        let mut event = self.event(
            None,
            RunStatus::Incomplete,
            DisplayDelta::default(),
            Some(
                "Observation gap: prior live evidence was discarded; retained outcomes follow."
                    .into(),
            ),
        );
        event.reset = true;
        vec![event]
    }

    fn remember(&mut self, at: &EffectIdentity) {
        if self.view.action_node(&at.site).is_none() {
            return;
        }
        let key = (at.site.clone(), at.task.clone());
        if self
            .identities
            .get(&key)
            .is_none_or(|held| held.occurrence <= at.occurrence)
        {
            self.identities.insert(key, at.clone());
        }
    }

    fn step_body_started(
        &mut self,
        observation: &StepBodyStartedObservation,
    ) -> Result<Vec<RunEvent>, RunError> {
        self.remember(&observation.step.at);
        self.accumulator.step_body_started(observation)?;
        self.changed()
    }

    /// The sites whose state in lash's overlay changed since they were last
    /// sent. A site shows its task in flight, else its latest ended one.
    fn changed(&mut self) -> Result<Vec<RunEvent>, RunError> {
        let Some(overlay) = self.accumulator.snapshot() else {
            return Ok(Vec::new());
        };
        self.identities.retain(|(site, task), _| {
            overlay
                .sites
                .iter()
                .any(|row| row.site.site == *site && row.site.task == *task)
        });
        let observations: Vec<_> = overlay
            .sites
            .iter()
            .filter_map(|site| {
                let (state, outcome) = match &site.state.occurrence {
                    WorkflowOverlayOccurrence::Unobserved => return None,
                    WorkflowOverlayOccurrence::Running { .. } => (ActionState::Running, None),
                    WorkflowOverlayOccurrence::Waiting { .. } => (ActionState::Waiting, None),
                    WorkflowOverlayOccurrence::Completed { .. } => (ActionState::Completed, None),
                    WorkflowOverlayOccurrence::Failed { failure, .. } => (
                        ActionState::Failed,
                        Some(serde_json::json!({"message": failure.message(), "cause": failure})),
                    ),
                    WorkflowOverlayOccurrence::Cancelled { .. } => (
                        ActionState::Cancelled,
                        Some(serde_json::json!({"message": "cancelled"})),
                    ),
                    WorkflowOverlayOccurrence::Incomplete { .. } => (ActionState::Incomplete, None),
                };
                Some(ActionObservation {
                    identity: self
                        .identities
                        .get(&(site.site.site.clone(), site.site.task.clone()))
                        .filter(|identity| {
                            Some(identity.occurrence) == site.state.occurrence.occurrence()
                        })?
                        .clone(),
                    state,
                    outcome,
                })
            })
            .collect();
        let activity = self.view.activity(self.view.document, &observations)?;
        let mut events = Vec::new();
        for node in activity {
            let Some(state) = node.state else {
                continue;
            };
            let status = match state {
                ActionState::Running => RunStatus::Started,
                ActionState::Waiting => RunStatus::Waiting,
                ActionState::Completed => RunStatus::Succeeded,
                ActionState::Failed | ActionState::Cancelled => RunStatus::Failed,
                ActionState::Incomplete => RunStatus::Incomplete,
            };
            let error = node
                .occurrences
                .iter()
                .filter_map(|occurrence| occurrence.outcome.as_ref())
                .find_map(|outcome| outcome.get("message").and_then(serde_json::Value::as_str))
                .map(str::to_owned);
            let (started, ended) = overlay
                .sites
                .iter()
                .filter(|site| site.site.site == node.site)
                .fold((0, 0), |(started, ended), site| {
                    (
                        started + site.state.summary.started_count,
                        ended + site.state.summary.terminal_count,
                    )
                });
            let shown = (status, started, ended);
            if self.shown.get(&node.site) == Some(&shown) {
                continue;
            }
            self.shown.insert(node.site.clone(), shown);
            let mut event = self.event(
                Some(node.site.clone()),
                status,
                DisplayDelta::default(),
                error,
            );
            event.activity = Some(node);
            events.push(event);
        }
        Ok(events)
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
                events.extend(self.changed()?);
                events.extend(self.deliver_display()?);
                let mut event = self.event(
                    None,
                    status,
                    DisplayDelta::default(),
                    (status == RunStatus::Failed).then(|| format!("{outcome:?}")),
                );
                if let lash::process::ProcessTerminal::Settled { output } = outcome {
                    event.output = Some(output.value_for_projection());
                }
                events.push(event);
            }
            _ => {}
        }
        Ok(events)
    }

    /// A committed effect outcome: durable evidence of one call at a site,
    /// which outlives the provisional overlay.
    fn effect(&mut self, occurrence: &ProcessEffectOccurrence) -> Vec<RunEvent> {
        self.remember(&occurrence.at);
        let status = match occurrence.outcome_class {
            ProcessEffectOutcomeClass::Success => {
                if let Some(call) = &occurrence.call_id {
                    self.completed_calls
                        .insert(call.to_string(), occurrence.at.site.clone());
                }
                RunStatus::Succeeded
            }
            ProcessEffectOutcomeClass::Failure | ProcessEffectOutcomeClass::Cancelled => {
                RunStatus::Failed
            }
        };
        let mut event = self.event(
            Some(occurrence.at.site.clone()),
            status,
            DisplayDelta::default(),
            occurrence.code.as_ref().map(ToString::to_string),
        );
        event.occurrence = Some(occurrence.at.clone());
        event.call_id = occurrence.call_id.clone();
        event.call_only = true;
        let observed = self.accumulator.snapshot().is_some_and(|overlay| {
            overlay
                .sites
                .iter()
                .any(|site| site.site.site == occurrence.at.site)
        });
        let mut events = vec![event];
        if !observed {
            let mut event = self.event(
                Some(occurrence.at.site.clone()),
                status,
                DisplayDelta::default(),
                occurrence.code.as_ref().map(ToString::to_string),
            );
            event.occurrence = Some(occurrence.at.clone());
            events.push(event);
        }
        events
    }

    fn waiting(&mut self, wait: &WaitState) -> RunEvent {
        let mut event = self.event(
            wait.site.as_ref().map(|at| at.site.clone()),
            RunStatus::Waiting,
            DisplayDelta::default(),
            None,
        );
        event.occurrence = wait.site.clone();
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
            let delta = crate::display::apply_tool(
                &mut self.display,
                &operation.operation,
                &operation.args,
            )
            .map_err(RunError::Invalid)?;
            let mut event = self.event(Some(node), RunStatus::Succeeded, delta, None);
            event.call_only = true;
            events.push(event);
        }
        Ok(events)
    }

    fn snapshot(&mut self, view: ProcessReadView) -> Result<(Vec<RunEvent>, bool), RunError> {
        let ProcessReadView::Retained(view) = view else {
            return Err(RunError::Invalid(
                "the process is no longer retained".into(),
            ));
        };
        if let Some(workflow) = &view.workflow {
            for site in &workflow.sites {
                self.remember(&site.at);
            }
        }
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
        events.extend(self.changed()?);
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
                    None,
                    status,
                    DisplayDelta::default(),
                    view.process
                        .terminal()
                        .filter(|_| status == RunStatus::Failed)
                        .map(|outcome| format!("{outcome:?}")),
                ),
            );
        }
        if terminal
            && let Some(lash::process::ProcessTerminal::Settled { output }) =
                view.process.terminal()
            && let Some(event) = events.last_mut()
        {
            event.output = Some(output.value_for_projection());
        }
        Ok((events, terminal))
    }

    fn observed_call(&self, at: &EffectIdentity) -> Option<lash::ToolCallId> {
        self.accumulator
            .snapshot()?
            .history
            .into_iter()
            .find_map(|event| match event.fact {
                WorkflowOverlayFact::StepBodyStarted { step } if step.at == *at => {
                    Some(step.call_id)
                }
                _ => None,
            })
    }

    fn language_observation(
        &mut self,
        observation: &LanguageExecutionObservation,
    ) -> Result<Vec<RunEvent>, RunError> {
        // VM terminals may omit the call id. The admitted body's exact
        // occurrence supplies it while that observation remains retained.
        self.accumulator.observe(observation)?;
        if let TraceLanguageExecutionPayload::Node { at, .. } = &observation.execution.payload {
            self.remember(at);
        }
        let mut events = self.changed()?;
        if let TraceLanguageExecutionPayload::Node { at, fact } = &observation.execution.payload {
            let call = match fact {
                TraceNodeFact::Completed { call_id } => call_id
                    .clone()
                    .or_else(|| self.observed_call(at))
                    .map(|call| {
                        self.completed_calls
                            .insert(call.to_string(), at.site.clone());
                        (call, RunStatus::Succeeded, None)
                    }),
                TraceNodeFact::Failed { call_id, failure } => call_id
                    .clone()
                    .or_else(|| self.observed_call(at))
                    .map(|call| (call, RunStatus::Failed, Some(failure.message().to_owned()))),
                _ => None,
            };
            if let Some((call, status, error)) = call {
                let mut event = self.event(
                    Some(at.site.clone()),
                    status,
                    DisplayDelta::default(),
                    error,
                );
                event.call_id = Some(call);
                event.occurrence = Some(at.clone());
                event.call_only = true;
                events.push(event);
            }
        }
        events.extend(self.deliver_display()?);
        Ok(events)
    }
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
    let provider: Arc<dyn lash::tools::ToolProvider> =
        Arc::new(lash::tools::StaticToolProvider::new(
            crate::display::tool_definitions(),
            tools.as_ref().clone(),
        ));
    let host_tools = lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(HOST_TOOLS.component()),
        lash::plugins::PluginSpec::new()
            .with_tool_provider(lash::tool_provider_key!("tools"), provider),
    )
    .as_component(HOST_TOOLS.format())?;
    let workers = lash::vm::WorkerService::default();
    let bounds = lash::vm::RunBounds {
        charge: 1_000_000,
        memory: 64 * 1024 * 1024,
        ..workers.config().run_bounds
    };
    let core = LashCore::standard_builder(backend)
        .plugin(Arc::new(lash::vm::KernelProcessPluginFactory::new(
            workers,
            lash::vm::TYPESCRIPT_DIALECT,
            bounds,
        )))
        .process_component(Arc::new(host_tools), HOST_TOOLS)
        .trace_detail(lash::tracing::TraceDetailPolicy::extended_models())
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("workflow-graph"),
            lash::persistence::LeaseIncarnationId::new(uuid::Uuid::new_v4().to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use lash::tracing::{
        TraceLanguageExecution, TraceLanguageExecutionIdentity, TraceRuntimeScope,
        TraceRuntimeSubject,
    };
    use lash::workflow::document::{DocumentId, EffectIdentity, TaskIdentity, Unit};
    use lash::workflow::{WorkflowDocumentEntry, WorkflowDocumentRef};

    fn overlay() -> Overlay {
        overlay_for("kernel 1\nnumbers float\nmain { do sleep 1 } fn callee() { do sleep 1 }")
    }

    fn overlay_for(text: &str) -> Overlay {
        let document = lash::workflow::document::parse_document(text).unwrap();
        let holder = WorkflowDocument::derive(
            document,
            lash::workflow::WorkflowDocumentEntry::Main,
            &lash::workflow::document::FunctionRegistry::default(),
        )
        .unwrap();
        let mut view = lash::workflow::view::derive(&holder).unwrap();
        view.document = DocumentId::from_bytes([1; 32]);
        Overlay {
            view,
            accumulator: WorkflowExecutionOverlayAccumulator::default(),
            identities: BTreeMap::new(),
            process: lash::ProcessId::fixture("adapter"),
            workflow_version: 1,
            definition: DocumentId::from_bytes([1; 32]).to_string(),
            shown: BTreeMap::new(),
            sequence: 0,
            display: DisplayState::default(),
            completed_calls: BTreeMap::new(),
            delivered: BTreeSet::new(),
            host: Arc::new(crate::display::HostTools::default()),
            coverage: None,
        }
    }

    fn observation(occurrence: u64, fact: TraceNodeFact) -> LanguageExecutionObservation {
        LanguageExecutionObservation {
            language: None,
            observed_at_ms: occurrence * 10 + 1,
            execution: TraceLanguageExecution {
                event_key: format!("{occurrence}:{fact:?}"),
                identity: TraceLanguageExecutionIdentity {
                    scope: TraceRuntimeScope::none(),
                    subject: TraceRuntimeSubject::Process {
                        process_id: lash::ProcessId::fixture("adapter"),
                    },
                    document: WorkflowDocumentRef {
                        document: DocumentId::from_bytes([1; 32]),
                        entry: WorkflowDocumentEntry::Main,
                    },
                    entry_name: "main".into(),
                    engine_execution_id: None,
                    generation: None,
                },
                payload: TraceLanguageExecutionPayload::Node {
                    at: EffectIdentity {
                        task: TaskIdentity::Main,
                        site: Site::new(Unit::Main, [0, 0]),
                        occurrence,
                        loops: Vec::new(),
                    },
                    fact,
                },
            },
        }
    }

    fn body_start(occurrence: u64, call_id: lash::ToolCallId) -> StepBodyStartedObservation {
        StepBodyStartedObservation {
            observed_at_ms: occurrence * 10,
            step: lash::tracing::StepBodyStarted {
                process_id: lash::ProcessId::fixture("adapter"),
                at: EffectIdentity {
                    task: TaskIdentity::Main,
                    site: Site::new(Unit::Main, [0, 0]),
                    occurrence,
                    loops: Vec::new(),
                },
                call_id,
                attempt: 1,
            },
        }
    }

    /// COARSE-CALL: another callable's observed action does not become a
    /// caller tile or invalidate the caller's activity snapshot.
    #[test]
    fn coarse_call_keeps_callee_observations_outside_the_caller_view() {
        let mut overlay = overlay();
        let mut callee = observation(0, TraceNodeFact::Started { call_id: None });
        if let TraceLanguageExecutionPayload::Node { at, .. } = &mut callee.execution.payload {
            at.site = Site::new(
                Unit::Function(lash::workflow::document::Name::new("callee")),
                [0, 0],
            );
        }
        assert!(overlay.language_observation(&callee).unwrap().is_empty());
    }

    /// COARSE-RESUME: the exact observed occurrence includes its loop ancestry.
    #[test]
    fn coarse_resume_keeps_the_observed_loop_identity() {
        let mut overlay =
            overlay_for("kernel 1\nnumbers float\nmain { for item in [1] { do sleep 1 } }");
        let mut running = observation(3, TraceNodeFact::Started { call_id: None });
        let TraceLanguageExecutionPayload::Node { at, .. } = &mut running.execution.payload else {
            panic!("node fixture");
        };
        at.site = Site::new(Unit::Main, [0, 1, 0, 0]);
        at.loops = vec![lash::workflow::document::LoopIteration {
            site: Site::new(Unit::Main, [0]),
            iteration: 3,
        }];
        let identity = at.clone();
        let events = overlay.language_observation(&running).unwrap();
        assert_eq!(
            events[0].activity.as_ref().unwrap().occurrences[0].identity,
            identity
        );
    }

    #[test]
    fn a_process_success_does_not_invent_a_missing_occurrence_outcome() {
        let mut overlay = overlay();
        overlay
            .language_observation(&observation(0, TraceNodeFact::Started { call_id: None }))
            .unwrap();
        overlay.accumulator.settle(WorkflowOverlaySettlement {
            terminal: WorkflowOverlayTerminal::Completed,
            occurred_at: None,
        });
        let events = overlay.changed().unwrap();
        assert_eq!(
            serde_json::to_value(&events[0]).unwrap()["status"],
            "incomplete"
        );
    }

    #[test]
    fn a_gap_retires_shown_state_and_tells_the_client_to_replace_its_view() {
        let mut overlay = overlay();
        overlay
            .language_observation(&observation(0, TraceNodeFact::Started { call_id: None }))
            .unwrap();
        assert!(!overlay.shown.is_empty());
        let events = overlay.reset_after_gap();
        assert!(
            overlay.shown.is_empty(),
            "lost provisional statuses must not survive a gap"
        );
        assert_eq!(events.len(), 1);
        let event = serde_json::to_value(&events[0]).unwrap();
        assert_eq!(event["reset"], true);
        assert!(
            events[0].error.is_some(),
            "the replacement must explain missing live evidence"
        );
    }

    #[test]
    fn live_call_data_continues_past_the_durable_summary_without_repainting_running_work() {
        let mut overlay = overlay();
        let mut running = observation(0, TraceNodeFact::Started { call_id: None });
        if let TraceLanguageExecutionPayload::Node { at, .. } = &mut running.execution.payload {
            at.task = TaskIdentity::Spawned(lash::workflow::document::SpawnIdentity {
                parent: Arc::new(TaskIdentity::Main),
                site: Site::new(Unit::Main, [1]),
                occurrence: 0,
            });
        }
        overlay.language_observation(&running).unwrap();
        let mut last_durable = None;
        let retained = lash::EvidenceLimits::standard().process_effect_occurrences();
        for occurrence in 0..=retained {
            let call = lash::ToolCallId::fixture(&format!("call-{occurrence}"));
            overlay
                .step_body_started(&body_start(occurrence, call.clone()))
                .unwrap();
            let other_call = lash::ToolCallId::fixture(&format!("other-task-{occurrence}"));
            let mut concurrent = body_start(occurrence, other_call);
            if let TraceLanguageExecutionPayload::Node { at, .. } = &running.execution.payload {
                concurrent.step.at.task = at.task.clone();
            }
            overlay.step_body_started(&concurrent).unwrap();
            let events = overlay
                .language_observation(&observation(
                    occurrence,
                    TraceNodeFact::Completed { call_id: None },
                ))
                .unwrap();
            let event = events
                .iter()
                .find(|event| event.call_id.as_ref() == Some(&call))
                .expect(
                    "every observed call has inspector data even without a durable summary record",
                );
            assert_eq!(serde_json::to_value(event).unwrap()["callOnly"], true);
            assert!(
                events
                    .iter()
                    .filter(|event| serde_json::to_value(event).unwrap()["callOnly"] != true)
                    .all(|event| event.status == RunStatus::Started),
                "another in-flight occurrence keeps its running aggregate"
            );
            let durable = ProcessEffectOccurrence {
                vocabulary_version: 1,
                at: EffectIdentity {
                    task: TaskIdentity::Main,
                    site: Site::new(Unit::Main, [0, 0]),
                    occurrence,
                    loops: Vec::new(),
                },
                operation: "fixture".into(),
                outcome_class: ProcessEffectOutcomeClass::Success,
                code: None,
                call_id: Some(call),
                replay_key: format!("call-{occurrence}"),
            };
            if occurrence < retained {
                assert!(
                    overlay.effect(&durable).iter().all(|event| event.call_only),
                    "a durable completion must not repaint a concurrently running task"
                );
                last_durable = Some(durable);
            }
        }
        let replayed = overlay
            .language_observation(&observation(0, TraceNodeFact::Completed { call_id: None }))
            .unwrap();
        assert!(
            replayed
                .iter()
                .any(|event| event.call_id.as_ref() == Some(&lash::ToolCallId::fixture("call-0"))),
            "a retained older occurrence uses its own body binding, not the latest call"
        );
        let failed = lash::ToolCallId::fixture("failed-after-cap");
        overlay
            .step_body_started(&body_start(retained + 1, failed.clone()))
            .unwrap();
        let events = overlay
            .language_observation(&observation(
                retained + 1,
                TraceNodeFact::Failed {
                    call_id: None,
                    failure: lash::tracing::TraceLanguageExecutionFailure::Runtime {
                        code: "fixture".into(),
                        message: "failed".into(),
                    },
                },
            ))
            .unwrap();
        let call = events
            .iter()
            .find(|event| event.call_id.as_ref() == Some(&failed))
            .expect("failed calls also remain inspectable after the cap");
        assert_eq!(call.status, RunStatus::Failed);
        assert!(call.call_only);
        assert!(!overlay.completed_calls.contains_key(&failed.to_string()));
        overlay.accumulator.settle(WorkflowOverlaySettlement {
            terminal: WorkflowOverlayTerminal::Completed,
            occurred_at: None,
        });
        overlay.changed().unwrap();
        assert!(
            overlay
                .effect(&last_durable.unwrap())
                .iter()
                .all(|event| event.call_only),
            "a durable success cannot replace another task's failed or incomplete aggregate"
        );
        overlay.reset_after_gap();
        let unbound = overlay
            .language_observation(&observation(0, TraceNodeFact::Completed { call_id: None }))
            .unwrap();
        assert!(
            unbound.iter().all(|event| event.call_id.is_none()),
            "a gap retires call bindings with their provisional observations"
        );
    }
}
