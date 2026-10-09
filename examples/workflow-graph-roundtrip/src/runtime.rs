use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use lash::LashCore;
use lash::process::*;
use lash::rlm::lang::{LinkedModule, ProcessRef};
use lash::tracing::{TraceEvent, TraceLanguageExecutionPayload};
use lash::workflow::{WorkflowDocument, WorkflowRead};
use tokio::sync::mpsc;

use crate::{DisplayDelta, DisplayState, RunEvent, RunStatus};

#[derive(Debug, thiserror::Error)]
pub(crate) enum RunError {
    #[error(transparent)]
    Lash(#[from] lash::EmbedError),
    #[error(transparent)]
    Link(#[from] lash::typescript::Diagnostic),
    #[error(transparent)]
    Definition(#[from] ProcessDefinitionDraftError),
    #[error(transparent)]
    Display(#[from] lash::rlm::lang::ExecutionHostError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(String),
}

/// A saved version's admission: the module a run publishes and the process
/// it starts. The run's graph is not kept here: lash answers it for the
/// process it started ([`RunView::read`]).
#[derive(Clone)]
pub(crate) struct AdmittedWorkflow {
    linked: LinkedModule,
    entry: Option<ProcessRef>,
}

impl AdmittedWorkflow {
    pub(crate) fn admit(source: &str) -> Result<Self, RunError> {
        let linked = lash::typescript::link(source, &host_environment())?;
        let entry = linked
            .artifact
            .ir()
            .declarations
            .iter()
            .find_map(|declaration| match declaration {
                lash::rlm::lang::Declaration::Process(process) => {
                    linked.artifact.process_ref(process.name.as_str()).cloned()
                }
                _ => None,
            });
        Ok(Self { linked, entry })
    }
}

pub(crate) struct PreparedRun {
    admitted: AdmittedWorkflow,
    workflow_version: u64,
    process_name: String,
}

/// What a run's overlay binds to, read from lash for the process itself:
/// the definition it executes and the nodes of its entry process.
struct RunView {
    workflow_version: u64,
    definition: String,
    root_node: String,
    nodes: BTreeSet<String>,
}

impl RunView {
    async fn read(
        core: &LashCore,
        process: &lash::ProcessId,
        workflow_version: u64,
    ) -> Result<Self, RunError> {
        match core.processes().graph(process).await? {
            WorkflowRead::Inspected(inspection) => Self::of(&inspection.document, workflow_version),
            unreadable => Err(RunError::Invalid(format!(
                "the run's workflow cannot be read: {unreadable:?}"
            ))),
        }
    }

    fn of(document: &WorkflowDocument, workflow_version: u64) -> Result<Self, RunError> {
        let definition = document
            .graph
            .source_identity
            .clone()
            .ok_or_else(|| RunError::Invalid("the run's graph names no artifact".into()))?;
        let map = trace_lashlang_process_map(&document.graph, &document.entry)
            .ok_or_else(|| RunError::Invalid("the run's process has no execution map".into()))?;
        let root_node = document
            .graph
            .process(&document.entry)
            .ok_or_else(|| RunError::Invalid("the run's process has no graph".into()))?
            .id
            .to_string();
        Ok(Self {
            workflow_version,
            definition,
            root_node,
            nodes: map.nodes.into_iter().map(|node| node.id).collect(),
        })
    }
}

impl PreparedRun {
    pub(crate) fn new(
        admitted: &AdmittedWorkflow,
        workflow_version: u64,
    ) -> Result<Self, RunError> {
        let entry = admitted
            .entry
            .as_ref()
            .ok_or_else(|| RunError::Invalid("saved workflow has no process to run".into()))?;
        let process_name = admitted
            .linked
            .artifact
            .exports()
            .processes
            .keys()
            .find(|name| {
                admitted
                    .linked
                    .artifact
                    .process_ref(name)
                    .is_some_and(|reference| reference == entry)
            })
            .ok_or_else(|| RunError::Invalid("saved entry is not exported".into()))?
            .clone();
        Ok(Self {
            admitted: admitted.clone(),
            workflow_version,
            process_name,
        })
    }

    pub(crate) async fn publish(
        &self,
        core: &LashCore,
        key: &str,
    ) -> Result<(ProcessStartRequest, HostArtifactPin), RunError> {
        let pin = HostArtifactPin::mint();
        let artifacts = core.host_artifacts();
        let result = async {
            let artifact = &self.admitted.linked.artifact;
            artifacts.publish_module(&pin, artifact).await?;
            let identity = lash::rlm::lang::ProcessDefinitionIdentity::from_artifact_export(
                artifact,
                &self.process_name,
            )
            .ok_or_else(|| RunError::Invalid("saved entry is not exported".into()))?;
            let definition = artifacts
                .publish_definition(&pin, &identity.draft()?)
                .await?;
            let env = ProcessExecutionEnvSpec::new(
                lash::plugins::AdmittedPluginConfig::default(),
                lash::runtime::SessionPolicy::new(
                    lash::TurnBudget::bounded(32),
                    lash::MaxToolCalls::new(1024),
                    lash::NoProgressBudget::bounded(12),
                ),
                lash::plugins::SessionToolAccess::ambient(),
            );
            let env_ref = artifacts.publish_process_env(&pin, &env).await?;
            Ok(ProcessStartRequest::new(
                ProcessStartTarget::Definition {
                    definition_id: definition.id,
                    signature_claim: Some(definition.signature),
                    args: Default::default(),
                },
                ProcessOriginator::host(),
                Lifetime::Detached,
            )
            .with_host_start_key(key)
            .with_env_ref(env_ref))
        }
        .await;
        // The admission caller releases this only after start acquires the closure.
        match result {
            Ok(request) => Ok((request, pin)),
            Err(error) => {
                artifacts.release(pin).await?;
                Err(error)
            }
        }
    }

    pub(crate) async fn observe(
        self,
        core: LashCore,
        process: lash::ProcessId,
        sender: mpsc::Sender<Result<RunEvent, RunError>>,
        host: Arc<crate::display::HostTools>,
    ) -> Result<(), RunError> {
        // Subscribe before reading the graph: the live route buffers what the
        // process publishes while lash reads its workflow.
        let mut live = core
            .processes()
            .subscribe_observation(&process, None)
            .await?;
        let mut overlay = Overlay {
            view: RunView::read(&core, &process, self.workflow_version).await?,
            process: process.clone(),
            sequence: 0,
            display: DisplayState::default(),
            completed_calls: BTreeMap::new(),
            delivered: BTreeSet::new(),
            host,
            observed: BTreeSet::new(),
        };
        if let Some(item) = live.recv().await.map_err(lash::EmbedError::from)? {
            for event in overlay.observation(item) {
                if sender.send(Ok(event)).await.is_err() {
                    return Ok(());
                }
            }
        }
        let mut from = ProcessEventsFrom::Start(process.clone());
        let mut poll = tokio::time::interval(Duration::from_millis(25));
        loop {
            let page = core
                .processes()
                .events(
                    from.clone(),
                    NonZeroUsize::new(128)
                        .ok_or_else(|| RunError::Invalid("zero page bound".into()))?,
                    ProcessEventQueryMode::Full,
                )
                .await?;
            let ProcessEventReadOutcome::Retained(page_events) = page.outcome else {
                return Err(RunError::Invalid(
                    "the process event history is unavailable".into(),
                ));
            };
            let ProcessEventPageEvents::Full(events) = page_events.events else {
                return Err(RunError::Invalid(
                    "the process event feed returned a Lite page".into(),
                ));
            };
            for event in events {
                let terminal = matches!(&event.fact, ProcessLifecycleFact::Terminal { .. });
                if terminal {
                    // The final snapshot catches call bindings and pure-node transitions
                    // published while the durable reader drained its last page.
                    let mut final_view = core
                        .processes()
                        .subscribe_observation(&process, None)
                        .await?;
                    if let Some(item) = final_view.recv().await.map_err(lash::EmbedError::from)? {
                        for event in overlay.observation(item) {
                            if sender.send(Ok(event)).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                }
                for projected in overlay.durable(&event)? {
                    if sender.send(Ok(projected)).await.is_err() {
                        return Ok(());
                    }
                }
                if terminal {
                    return Ok(());
                }
            }
            if let Some(cursor) = page.cursor {
                from = ProcessEventsFrom::After(cursor);
            }
            if matches!(page_events.more, ProcessEventPageMore::More { .. }) {
                continue;
            }
            tokio::select! {
                _ = sender.closed() => return Ok(()),
                _ = poll.tick() => {},
                item = live.recv() => {
                    if let Some(item) = item.map_err(lash::EmbedError::from)? {
                        for event in overlay.observation(item) {
                            if sender.send(Ok(event)).await.is_err() { return Ok(()); }
                        }
                    }
                }
            }
        }
    }
}

struct Overlay {
    view: RunView,
    process: lash::ProcessId,
    sequence: u64,
    display: DisplayState,
    completed_calls: BTreeMap<String, String>,
    delivered: BTreeSet<String>,
    host: Arc<crate::display::HostTools>,
    observed: BTreeSet<String>,
}

impl Overlay {
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
            workflow_version: self.view.workflow_version,
            definition: self.view.definition.clone(),
            sequence: self.sequence,
            node_id,
            status,
            display_delta,
            display: self.display.clone(),
            error,
            approval_key: None,
        }
    }

    fn durable(&mut self, event: &ObservedProcessEvent) -> Result<Vec<RunEvent>, RunError> {
        let mut events = Vec::new();
        match &event.fact {
            ProcessLifecycleFact::EffectOutcome(occurrence) => {
                if !self.view.nodes.contains(&occurrence.node_id) {
                    return Err(RunError::Invalid(format!(
                        "process event names a node outside the saved execution map: {}",
                        occurrence.node_id
                    )));
                }
                let status = match occurrence.outcome_class {
                    ProcessEffectOutcomeClass::Success => RunStatus::Succeeded,
                    ProcessEffectOutcomeClass::Failure | ProcessEffectOutcomeClass::Cancelled => {
                        RunStatus::Failed
                    }
                };
                events.push(self.event(
                    occurrence.node_id.clone(),
                    status,
                    DisplayDelta::default(),
                    occurrence.code.as_ref().map(ToString::to_string),
                ));
            }
            ProcessLifecycleFact::Waiting { wait } => {
                let mut event = self.event(
                    self.view.root_node.clone(),
                    RunStatus::Waiting,
                    DisplayDelta::default(),
                    None,
                );
                if let WaitKind::Call { call_id, .. } = &wait.kind {
                    event.approval_key = self.host.approval_key(&self.process, call_id.as_str());
                }
                events.push(event);
            }
            ProcessLifecycleFact::Terminal { outcome, .. } => {
                let status = if outcome.status() == TerminalProcessStatus::Completed {
                    RunStatus::Succeeded
                } else {
                    RunStatus::Failed
                };
                events.extend(self.deliver_display()?);
                events.push(self.event(
                    self.view.root_node.clone(),
                    status,
                    DisplayDelta::default(),
                    (status == RunStatus::Failed).then(|| format!("{outcome:?}")),
                ));
            }
            _ => {}
        }
        Ok(events)
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
                &[lash::rlm::lang::from_json(operation.args)],
            )?;
            events.push(self.event(node, RunStatus::Succeeded, delta, None));
        }
        Ok(events)
    }

    fn observation(&mut self, item: ProcessObservationItem) -> Vec<RunEvent> {
        let payloads = match item {
            ProcessObservationItem::Event { record, .. } => match record.event {
                TraceEvent::LanguageExecution { event, .. } => vec![event],
                _ => Vec::new(),
            },
            ProcessObservationItem::Snapshot { snapshot, .. }
            | ProcessObservationItem::Gap { snapshot, .. } => snapshot
                .live
                .graph
                .map(|graph| {
                    graph
                        .history
                        .into_iter()
                        .map(|record| record.event)
                        .collect()
                })
                .unwrap_or_default(),
            ProcessObservationItem::Committed { .. } => Vec::new(),
        };
        let mut events = Vec::new();
        for observed in payloads {
            if observed.identity.source_identity != self.view.definition
                || !self.observed.insert(observed.event_key.clone())
            {
                continue;
            }
            if let Some(event) = self.language(observed.payload) {
                events.push(event);
            }
        }
        match self.deliver_display() {
            Ok(delivered) => events.extend(delivered),
            Err(error) => events.push(self.event(
                self.view.root_node.clone(),
                RunStatus::Failed,
                DisplayDelta::default(),
                Some(error.to_string()),
            )),
        }
        events
    }

    fn language(&mut self, payload: TraceLanguageExecutionPayload) -> Option<RunEvent> {
        // The engine resumes the VM with a settled step's recorded output.
        // NodeCompleted identifies that logical call even after the bounded
        // effect summary stops carrying individual loop occurrences.
        if let TraceLanguageExecutionPayload::NodeCompleted {
            node_id,
            call_id: Some(call),
            ..
        } = &payload
            && self.view.nodes.contains(node_id)
        {
            self.completed_calls
                .insert(call.to_string(), node_id.clone());
        }
        let (node, status, error) = match payload {
            TraceLanguageExecutionPayload::NodeStarted { node_id, .. }
            | TraceLanguageExecutionPayload::NodeResumed { node_id, .. } => {
                (node_id, RunStatus::Started, None)
            }
            TraceLanguageExecutionPayload::NodeWaiting { node_id, .. } => {
                if !self.view.nodes.contains(&node_id) {
                    return None;
                }
                return Some(self.event(
                    node_id,
                    RunStatus::Waiting,
                    DisplayDelta::default(),
                    None,
                ));
            }
            TraceLanguageExecutionPayload::NodeCompleted { node_id, .. }
            | TraceLanguageExecutionPayload::BranchSelected { node_id, .. } => {
                (node_id, RunStatus::Succeeded, None)
            }
            TraceLanguageExecutionPayload::NodeFailed {
                node_id, failure, ..
            } => (
                node_id,
                RunStatus::Failed,
                Some(failure.message().to_owned()),
            ),
            TraceLanguageExecutionPayload::NodeCancelled { node_id, .. } => {
                (node_id, RunStatus::Failed, Some("cancelled".into()))
            }
            _ => return None,
        };
        self.view
            .nodes
            .contains(&node)
            .then(|| self.event(node, status, DisplayDelta::default(), error))
    }
}

pub(crate) fn host_environment() -> lash::rlm::lang::LashlangHostEnvironment {
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

    config.lashlang_language_features = lash::rlm::lang::LashlangLanguageFeatures::default()
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

/// Starts run through the engine's process admission API.
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

    #[test]
    fn a_call_wait_shows_the_run_waiting_without_an_unregistered_approval() {
        let linked = lash::typescript::link(crate::DEFAULT_WORKFLOW, &host_environment())
            .expect("the default workflow admits");
        let graph =
            lash::typescript::workflow_graph::workflow_graph_from_artifact(&linked.artifact);
        let entry = linked
            .artifact
            .exports()
            .processes
            .keys()
            .next()
            .expect("the default workflow exports a process")
            .clone();
        let view = RunView::of(
            &WorkflowDocument {
                graph,
                source: String::new(),
                entry,
            },
            1,
        )
        .expect("the run view");
        let mut overlay = Overlay {
            view,
            process: lash::ProcessId::fixture("call-wait-overlay"),
            sequence: 0,
            display: DisplayState::default(),
            completed_calls: BTreeMap::new(),
            delivered: BTreeSet::new(),
            host: Arc::new(crate::display::HostTools::default()),
            observed: BTreeSet::new(),
        };
        let event = ObservedProcessEvent {
            sequence: 1,
            fact: ProcessLifecycleFact::Waiting {
                wait: WaitState {
                    kind: WaitKind::Call {
                        call_id: lash::ToolCallId::fixture("call-wait-overlay"),
                        tool_id: lash::tools::ToolId::new("tool:overlay"),
                    },
                    since_ms: 42,
                    site: None,
                },
            },
            occurred_at_ms: 42,
        };
        let projected = overlay.durable(&event).expect("project the call wait");
        let [waiting] = projected.as_slice() else {
            panic!("one overlay event per wait: {projected:?}");
        };
        assert_eq!(waiting.status, RunStatus::Waiting);
        assert!(
            waiting.approval_key.is_none(),
            "a call the host registered no approval for offers no key"
        );
    }
}
