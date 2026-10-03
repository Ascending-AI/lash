use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use lash::LashCore;
use lash::process::*;
use lash::restate::restate_sdk;
use lash::rlm::lang::{LinkedModule, ProcessRef, WorkflowGraph};
use lash::tracing::{TraceEvent, TraceLanguageExecutionPayload};
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
    Ingress(Box<lash::restate::RestateHttpError>),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(String),
}

impl From<lash::restate::RestateHttpError> for RunError {
    fn from(error: lash::restate::RestateHttpError) -> Self {
        Self::Ingress(Box::new(error))
    }
}

#[derive(Clone)]
pub(crate) struct AdmittedWorkflow {
    linked: LinkedModule,
    view: WorkflowGraph,
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
        let view = lash::typescript::workflow_graph::workflow_graph_from_artifact(&linked.artifact);
        Ok(Self {
            linked,
            view,
            entry,
        })
    }

    pub(crate) fn view(&self) -> &WorkflowGraph {
        &self.view
    }
}

pub(crate) struct PreparedRun {
    admitted: AdmittedWorkflow,
    workflow_version: u64,
    definition: String,
    process_name: String,
    root_node: String,
    nodes: BTreeSet<String>,
}

impl PreparedRun {
    pub(crate) fn new(
        graph: &WorkflowGraph,
        admitted: &AdmittedWorkflow,
        workflow_version: u64,
    ) -> Result<Self, RunError> {
        let definition = admitted.linked.artifact.source_identity();
        if graph.source_identity.as_deref() != Some(definition.as_str()) {
            return Err(RunError::Invalid(
                "the run overlay's graph is not the admitted artifact's".into(),
            ));
        }
        let ids = |graph: &WorkflowGraph| {
            graph
                .nodes()
                .map(|node| node.id.clone())
                .collect::<BTreeSet<_>>()
        };
        if let Some(node) = ids(graph)
            .symmetric_difference(&ids(admitted.view()))
            .next()
        {
            return Err(RunError::Invalid(format!(
                "workflow node `{node}` is not shared by the run overlay's graph and its admitted artifact"
            )));
        }
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
        let map = trace_lashlang_process_map(graph, &process_name)
            .ok_or_else(|| RunError::Invalid("saved process has no execution map".into()))?;
        let root_node = graph
            .process(&process_name)
            .ok_or_else(|| RunError::Invalid("saved process has no graph".into()))?
            .id
            .to_string();
        Ok(Self {
            admitted: admitted.clone(),
            workflow_version,
            definition,
            process_name,
            root_node,
            nodes: map.nodes.into_iter().map(|node| node.id).collect(),
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
                ),
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
            .with_env_ref(env_ref)
            .with_extra_event_types([crate::display::event_type()]))
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
    ) -> Result<(), RunError> {
        let mut overlay = Overlay {
            prepared: self,
            process: process.clone(),
            sequence: 0,
            display: DisplayState::default(),
            bindings: BTreeMap::new(),
            pending: BTreeMap::new(),
            observed: BTreeSet::new(),
        };
        let mut live = core
            .processes()
            .subscribe_observation(&process, None)
            .await?;
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
                let terminal = matches!(
                    event.event_type.as_str(),
                    "process.completed"
                        | "process.failed"
                        | "process.cancelled"
                        | "process.abandoned"
                );
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
                if let Some(projected) = overlay.durable(&event)?
                    && sender.send(Ok(projected)).await.is_err()
                {
                    return Ok(());
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
    prepared: PreparedRun,
    process: lash::ProcessId,
    sequence: u64,
    display: DisplayState,
    bindings: BTreeMap<String, String>,
    pending: BTreeMap<String, DisplayDelta>,
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
            workflow_version: self.prepared.workflow_version,
            definition: self.prepared.definition.clone(),
            sequence: self.sequence,
            node_id,
            status,
            display_delta,
            display: self.display.clone(),
            error,
            waiting_signal: None,
        }
    }

    fn durable(&mut self, event: &ObservedProcessEvent) -> Result<Option<RunEvent>, RunError> {
        if event.event_type == crate::display::EVENT_TYPE {
            let operation: crate::display::DisplayEvent =
                serde_json::from_value(event.payload.clone())?;
            let (_, delta) = crate::display::apply_tool(
                &mut self.display,
                &operation.operation,
                &[lash::rlm::lang::from_json(operation.args)],
            )?;
            let node = self.bindings.get(&operation.call_id).cloned();
            let (node, status) = match node {
                Some(node) => (node, RunStatus::Succeeded),
                None => {
                    self.pending.insert(operation.call_id, delta.clone());
                    (self.prepared.root_node.clone(), RunStatus::Started)
                }
            };
            return Ok(Some(self.event(node, status, delta, None)));
        }
        if event.event_type == PROCESS_EFFECT_OUTCOME_EVENT_TYPE {
            let occurrence: ProcessEffectOccurrence =
                serde_json::from_value(event.payload.clone())?;
            if !self.prepared.nodes.contains(&occurrence.node_id) {
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
            return Ok(Some(self.event(
                occurrence.node_id,
                status,
                DisplayDelta::default(),
                occurrence.code.map(|code| code.to_string()),
            )));
        }
        if event.event_type == "process.waiting" {
            #[derive(serde::Deserialize)]
            struct Waiting {
                wait: WaitState,
            }
            let Waiting { wait } = serde_json::from_value(event.payload.clone())?;
            let WaitKind::Signal { name, .. } = wait.kind;
            let mut event = self.event(
                self.prepared.root_node.clone(),
                RunStatus::Waiting,
                DisplayDelta::default(),
                None,
            );
            event.waiting_signal = Some(name);
            return Ok(Some(event));
        }
        let status = match event.event_type.as_str() {
            "process.completed" => RunStatus::Succeeded,
            "process.failed" | "process.cancelled" | "process.abandoned" => RunStatus::Failed,
            _ => return Ok(None),
        };
        Ok(Some(self.event(
            self.prepared.root_node.clone(),
            status,
            DisplayDelta::default(),
            (status == RunStatus::Failed).then(|| event.payload.to_string()),
        )))
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
            if observed.identity.source_identity != self.prepared.definition
                || !self.observed.insert(observed.event_key.clone())
            {
                continue;
            }
            if let Some(event) = self.language(observed.payload) {
                events.push(event);
            }
        }
        let ready = self
            .pending
            .keys()
            .filter(|call| self.bindings.contains_key(*call))
            .cloned()
            .collect::<Vec<_>>();
        for call in ready {
            if let (Some(delta), Some(node)) = (
                self.pending.remove(&call),
                self.bindings.get(&call).cloned(),
            ) {
                events.push(self.event(node, RunStatus::Succeeded, delta, None));
            }
        }
        events
    }

    fn language(&mut self, payload: TraceLanguageExecutionPayload) -> Option<RunEvent> {
        match &payload {
            TraceLanguageExecutionPayload::NodeStarted {
                node_id,
                call_id: Some(call),
                ..
            }
            | TraceLanguageExecutionPayload::NodeCompleted {
                node_id,
                call_id: Some(call),
                ..
            }
            | TraceLanguageExecutionPayload::NodeFailed {
                node_id,
                call_id: Some(call),
                ..
            } if self.prepared.nodes.contains(node_id) => {
                self.bindings.insert(call.to_string(), node_id.clone());
            }
            _ => {}
        }
        let (node, status, error) = match payload {
            TraceLanguageExecutionPayload::NodeStarted { node_id, .. }
            | TraceLanguageExecutionPayload::NodeResumed { node_id, .. } => {
                (node_id, RunStatus::Started, None)
            }
            TraceLanguageExecutionPayload::NodeWaiting {
                node_id, awaited, ..
            } => {
                if !self.prepared.nodes.contains(&node_id) {
                    return None;
                }
                let mut event =
                    self.event(node_id, RunStatus::Waiting, DisplayDelta::default(), None);
                if let lash::tracing::TraceNodeAwaited::Signal { name, .. } = awaited {
                    event.waiting_signal = Some(name);
                }
                return Some(event);
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
        self.prepared
            .nodes
            .contains(&node)
            .then(|| self.event(node, status, DisplayDelta::default(), error))
    }
}

pub(crate) fn host_environment() -> lash::rlm::lang::LashlangHostEnvironment {
    crate::operations::host_environment()
}

pub fn core(backend: lash::Backend) -> lash::Result<LashCore> {
    let mut config = lash::rlm::RlmProtocolPluginConfig::builder()
        .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
        .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
        .channel(lash::rlm::RlmChannel::Cell)
        .build();
    config.lashlang_abilities = lash::rlm::lang::LashlangAbilities::all().into();
    config.lashlang_language_features = lash::rlm::lang::LashlangLanguageFeatures::default()
        .with_label_annotations()
        .into();
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        config,
        Arc::new(lash::rlm::TypescriptDialect),
        &backend,
    );
    LashCore::rlm_builder(backend, factory)
        .tools(Arc::new(lash::tools::StaticToolProvider::new(
            crate::operations::tool_definitions(),
            crate::display::DisplayTools,
        )))
        .trace_level(lash::tracing::TraceLevel::Extended)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "workflow-graph",
            uuid::Uuid::new_v4().to_string(),
        ))
}

/// Bind the host's process admissions beside the engine's workers. The HTTP
/// host sends a command; only this journaled handler issues its effects.
pub fn bind_commands(
    builder: restate_sdk::endpoint::Builder,
    core: LashCore,
    engine: &lash::restate::RestateEngine,
) -> restate_sdk::endpoint::Builder {
    builder.bind(WorkflowGraphCommand {
        authority: engine.restate_effect_host().authority_id().clone(),
        namespace: engine.namespace().clone(),
        core,
    })
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "request", rename_all = "snake_case")]
enum HostCommand {
    Start(Box<ProcessStartRequest>),
    Signal(ProcessSignal),
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "receipt", rename_all = "snake_case")]
enum HostReceipt {
    Started(ProcessStartReceipt),
    Signalled,
}

struct WorkflowGraphCommand {
    core: LashCore,
    authority: lash::restate::RestateAuthorityId,
    namespace: lash::restate::RestateNamespace,
}

#[restate_sdk::workflow]
impl WorkflowGraphCommand {
    #[handler]
    async fn run(
        &self,
        context: restate_sdk::context::WorkflowContext<'_>,
        restate_sdk::serde::Json(command): restate_sdk::serde::Json<HostCommand>,
    ) -> restate_sdk::errors::HandlerResult<
        restate_sdk::serde::Json<Result<HostReceipt, lash::plugins::PluginError>>,
    > {
        let scope = lash::runtime::AdmittedScope::runtime_operation(format!(
            "workflow-command:{}",
            context.key()
        ));
        let controller = lash::restate::RestateRuntimeEffectController::new(
            context,
            self.authority.clone(),
            self.core.build_generation().clone(),
        )
        .in_namespace(self.namespace.clone());
        let scoped = controller
            .scoped_effect_controller(scope)
            .map_err(restate_sdk::errors::TerminalError::from_error)?;
        let result = match command {
            HostCommand::Start(request) => self
                .core
                .processes()
                .start(*request, scoped)
                .await
                .map(HostReceipt::Started),
            HostCommand::Signal(signal) => self
                .core
                .processes()
                .signal(signal, scoped)
                .await
                .map(|_| HostReceipt::Signalled),
        };
        let result = match result {
            Ok(receipt) => Ok(receipt),
            Err(error) if error.is_retryable() => {
                return Err(restate_sdk::errors::HandlerError::from(error));
            }
            Err(lash::EmbedError::Plugin(error)) => Err(error),
            Err(lash::EmbedError::Runtime(error)) => {
                Err(lash::plugins::PluginError::Runtime(error))
            }
            Err(lash::EmbedError::Store(error)) => Err(lash::plugins::PluginError::from(error)),
            Err(error) => return Err(restate_sdk::errors::HandlerError::from(error)),
        };
        Ok(restate_sdk::serde::Json(result))
    }
}

#[derive(Clone)]
pub(crate) struct CommandClient(lash::restate::RestateIngressClient);

impl CommandClient {
    pub(crate) fn new(connection: lash::restate::RestateConnection) -> Self {
        Self(lash::restate::RestateIngressClient::new(connection))
    }

    async fn send(&self, key: &str, command: HostCommand) -> Result<HostReceipt, RunError> {
        let result: Result<HostReceipt, lash::plugins::PluginError> = self
            .0
            .call_workflow_json("WorkflowGraphCommand", key, "run", &command)
            .await?;
        result.map_err(|error| RunError::Lash(lash::EmbedError::Plugin(error)))
    }

    pub(crate) async fn start(
        &self,
        key: &str,
        request: ProcessStartRequest,
    ) -> Result<ProcessStartReceipt, RunError> {
        match self
            .send(key, HostCommand::Start(Box::new(request)))
            .await?
        {
            HostReceipt::Started(receipt) => Ok(receipt),
            HostReceipt::Signalled => {
                Err(RunError::Invalid("start returned a signal receipt".into()))
            }
        }
    }

    pub(crate) async fn signal(&self, key: &str, signal: ProcessSignal) -> Result<(), RunError> {
        match self.send(key, HostCommand::Signal(signal)).await? {
            HostReceipt::Signalled => Ok(()),
            HostReceipt::Started(_) => {
                Err(RunError::Invalid("signal returned a start receipt".into()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admitted() -> AdmittedWorkflow {
        AdmittedWorkflow::admit(crate::DEFAULT_WORKFLOW).expect("the default workflow admits")
    }

    #[test]
    fn a_run_binds_to_the_admitted_view_and_its_recorded_entry() {
        let admitted = admitted();
        assert!(
            admitted.entry.is_some(),
            "the version records its entry by ref"
        );
        PreparedRun::new(admitted.view(), &admitted, 1).expect("the admitted view runs");
    }

    #[test]
    fn a_run_refuses_a_graph_that_is_not_its_admitted_view() {
        let admitted = admitted();
        let refused = |graph: &WorkflowGraph, what: &str| {
            let Err(error) = PreparedRun::new(graph, &admitted, 1) else {
                panic!("{what} must be refused");
            };
            error.to_string()
        };

        // Same shape, another definition: node ids hash only owner and path,
        // so a foreign graph of the same shape shares every id.
        let mut foreign = admitted.view().clone();
        foreign.source_identity = Some("another-definition".to_string());
        assert!(refused(&foreign, "a foreign definition").contains("not the admitted artifact's"));

        // A draft claims no definition.
        let mut draft = admitted.view().clone();
        draft.source_identity = None;
        refused(&draft, "a draft");

        // Fewer nodes than the artifact's view.
        let mut fewer = admitted.view().clone();
        fewer.main.nodes.pop();
        assert!(refused(&fewer, "a graph missing a node").contains("not shared"));

        // More nodes than the artifact's view.
        let mut more = admitted.view().clone();
        let mut extra = more.main.nodes[0].clone();
        extra.id = lash::rlm::lang::WorkflowNodeId::new("node:foreign".to_string());
        more.main.nodes.push(extra);
        assert!(refused(&more, "a graph with a foreign node").contains("node:foreign"));
    }
}
