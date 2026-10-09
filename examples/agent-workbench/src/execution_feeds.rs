//! The workbench's execution graphs: a bounded cache of execution overlays,
//! fed from lash's session and process feeds and drawn over the workflow
//! document each execution names.
//!
//! A session's cells publish their language observations on the session's
//! feed; a process publishes its own on the process feed, with the body
//! starts of its admitted steps and the committed facts that settle it. The
//! workbench follows both and folds what they deliver into one
//! [`WorkflowExecutionOverlayAccumulator`] per execution key. An execution's
//! start, or its process's snapshot, names its document; the workbench reads
//! that document from lash once and draws the overlay over it. The
//! cache is a projection the host may lose at any time: it is never a
//! recovery authority. A feed gap discards the provisional history of the
//! graphs it covers, and what the feed replays next rebuilds what is still
//! retained.
//!
//! Everything here is bounded. The cache keeps the most recently fed
//! [`MAX_GRAPHS`] graphs; the workbench follows at most [`MAX_SESSIONS`]
//! sessions and [`MAX_PROCESSES`] processes, releasing the oldest follower
//! for a new one.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt as _;
use lash::observe::{SessionObservationEventPayload, SessionObservationStreamItem};
use lash::process::{
    LanguageExecutionObservation, ProcessDocumentIdentity, ProcessLifecycleFact,
    ProcessObservationEventPayload, ProcessObservationStreamItem, ProcessReadView, ProcessStatus,
    StepBodyStartedObservation,
};
use lash::sync::MutexExt;
use lash::tracing::{TraceLanguageExecutionIdentity, TraceLanguageExecutionPayload};
use lash::workflow::{
    WorkflowDocumentRead, WorkflowDocumentRef, WorkflowExecutionDocument,
    WorkflowExecutionOverlayAccumulator, WorkflowOverlaySettlement, WorkflowOverlayTerminal,
};
use lash::{ProcessId, SessionId};

use crate::AppState;
use crate::execution_view::ExecutionGraph;

/// Graphs the cache keeps.
const MAX_GRAPHS: usize = 256;
/// Sessions the workbench follows at once.
const MAX_SESSIONS: usize = 16;
/// Processes the workbench follows at once.
const MAX_PROCESSES: usize = 64;
/// How long a follower of an ended process keeps reading a quiet feed for
/// retained node evidence before it lets the process go.
const ENDED_PROCESS_DRAIN: Duration = Duration::from_secs(2);

/// What a graph's observations came from: the feed whose gap resets it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    Session(SessionId),
    Process(ProcessId),
}

struct CachedGraph {
    source: Source,
    accumulator: WorkflowExecutionOverlayAccumulator,
    /// The execution's identity as its language observations state it;
    /// `None` while only step facts of its process have arrived.
    identity: Option<TraceLanguageExecutionIdentity>,
    /// The document the execution names, and the document once lash
    /// answered it.
    wants: Option<WorkflowDocumentRef>,
    document: Option<Arc<WorkflowExecutionDocument>>,
    /// The cache's feed count when this graph was last fed.
    fed: u64,
}

/// Metadata lives exactly while a follower or a retained graph needs it.
#[derive(Default)]
struct CachedProcess {
    following: bool,
    settlement: Option<WorkflowOverlaySettlement>,
    document: Option<WorkflowDocumentRef>,
}

#[derive(Default)]
struct Cache {
    fed: u64,
    graphs: BTreeMap<String, CachedGraph>,
    /// Each followed process or process with a retained graph owns its
    /// committed settlement and the document its snapshot names together.
    processes: BTreeMap<ProcessId, CachedProcess>,
    /// The documents lash answered, by the reference that names each.
    documents: BTreeMap<WorkflowDocumentRef, Arc<WorkflowExecutionDocument>>,
}

impl Cache {
    /// The graph `key` names, fed now; a new one starts from what the cache
    /// already knows of `source`.
    fn fed_graph(&mut self, source: Source, key: String) -> &mut CachedGraph {
        self.fed += 1;
        let fed = self.fed;
        let (settlement, wants) = match &source {
            Source::Process(process_id) => {
                let process = self.processes.entry(process_id.clone()).or_default();
                (process.settlement, process.document.clone())
            }
            Source::Session(_) => (None, None),
        };
        let graph = self.graphs.entry(key).or_insert_with(|| {
            let mut accumulator = WorkflowExecutionOverlayAccumulator::default();
            if let Some(settlement) = settlement {
                accumulator.settle(settlement);
            }
            CachedGraph {
                source,
                accumulator,
                identity: None,
                wants,
                document: None,
                fed,
            }
        });
        graph.fed = fed;
        graph
    }

    fn evict(&mut self) {
        while self.graphs.len() > MAX_GRAPHS {
            let Some(oldest) = self
                .graphs
                .iter()
                .min_by_key(|(_, graph)| graph.fed)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.graphs.remove(&oldest);
        }
        let retained_processes = self
            .graphs
            .values()
            .filter_map(|graph| match &graph.source {
                Source::Process(id) => Some(id.clone()),
                Source::Session(_) => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        self.processes
            .retain(|id, process| process.following || retained_processes.contains(id));
        let wanted = self
            .graphs
            .values()
            .filter_map(|graph| graph.wants.as_ref())
            .chain(
                self.processes
                    .values()
                    .filter_map(|process| process.document.as_ref()),
            )
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        self.documents
            .retain(|reference, _| wanted.contains(reference));
    }

    /// Fold `observation`. Answers the document its graph names when the
    /// cache does not hold that document yet.
    fn observe(
        &mut self,
        source: Source,
        observation: &LanguageExecutionObservation,
    ) -> Option<WorkflowDocumentRef> {
        let key = observation.execution.identity.graph_key();
        // Steps that arrived before any language observation opened the
        // graph under the process's own key: it is this execution's.
        if !self.graphs.contains_key(&key)
            && let Some(opened) = self
                .graphs
                .iter()
                .find(|(_, graph)| graph.source == source && graph.identity.is_none())
                .map(|(opened, _)| opened.clone())
            && let Some(graph) = self.graphs.remove(&opened)
        {
            self.graphs.insert(key.clone(), graph);
        }
        let graph = self.fed_graph(source, key);
        graph.identity = Some(observation.execution.identity.clone());
        if let TraceLanguageExecutionPayload::ExecutionStarted { document } =
            &observation.execution.payload
        {
            graph.wants = Some(document.clone());
        }
        if let Err(error) = graph.accumulator.observe(observation) {
            eprintln!("warning: workbench execution graph refused an observation: {error}");
        }
        self.evict();
        self.attach_documents()
    }

    /// Fold the start of an admitted step body into its process's graph.
    fn step_body_started(
        &mut self,
        observation: &StepBodyStartedObservation,
    ) -> Option<WorkflowDocumentRef> {
        let process_id = observation.step.process_id.clone();
        let source = Source::Process(process_id.clone());
        // A process runs one execution: its steps belong to the graph its
        // language observations opened, or open it under the process's key.
        let key = self
            .graphs
            .iter()
            .find(|(_, graph)| graph.source == source)
            .map(|(key, _)| key.clone())
            .unwrap_or_else(|| format!("process:{process_id}"));
        let graph = self.fed_graph(source, key);
        if let Err(error) = graph.accumulator.step_body_started(observation) {
            eprintln!("warning: workbench execution graph refused a step body start: {error}");
        }
        self.evict();
        self.attach_documents()
    }

    /// Give every graph the document it names, when the cache holds it.
    /// Answers one document some graph names that the cache does not hold.
    fn attach_documents(&mut self) -> Option<WorkflowDocumentRef> {
        let mut missing = None;
        for graph in self.graphs.values_mut() {
            if graph.document.is_some() {
                continue;
            }
            let Some(wants) = &graph.wants else {
                continue;
            };
            match self.documents.get(wants) {
                Some(document) => {
                    graph.accumulator.set_document(document.overlay_document());
                    graph.document = Some(Arc::clone(document));
                }
                None => missing = Some(wants.clone()),
            }
        }
        missing
    }

    /// Lash answered the document `reference` names.
    fn loaded(&mut self, document: WorkflowExecutionDocument) {
        self.documents
            .insert(document.reference.clone(), Arc::new(document));
        self.attach_documents();
        // A read can answer after its graph or follower was released.
        self.evict();
    }

    fn follow_process(&mut self, process_id: &ProcessId) {
        self.processes
            .entry(process_id.clone())
            .or_default()
            .following = true;
    }

    fn release_process(&mut self, process_id: &ProcessId) {
        if let Some(process) = self.processes.get_mut(process_id) {
            process.following = false;
        }
        self.evict();
    }

    /// The snapshot of `process_id` names the document it runs.
    fn names_document(&mut self, process_id: &ProcessId, reference: WorkflowDocumentRef) {
        let source = Source::Process(process_id.clone());
        for graph in self.graphs.values_mut() {
            if graph.source == source && graph.wants.is_none() {
                graph.wants = Some(reference.clone());
            }
        }
        self.processes
            .entry(process_id.clone())
            .or_default()
            .document = Some(reference);
    }

    fn settle(&mut self, process_id: &ProcessId, settlement: WorkflowOverlaySettlement) {
        self.processes
            .entry(process_id.clone())
            .or_default()
            .settlement = Some(settlement);
        let source = Source::Process(process_id.clone());
        for graph in self.graphs.values_mut() {
            if graph.source == source {
                graph.accumulator.settle(settlement);
            }
        }
    }

    /// Discard the provisional history of `source`'s graphs: its feed lost
    /// continuity.
    fn reset(&mut self, source: &Source) {
        for graph in self.graphs.values_mut() {
            if graph.source == *source {
                graph.accumulator.reset_live();
            }
        }
    }

    /// Every graph a language observation has identified, drawn.
    fn drawn(&self) -> Vec<ExecutionGraph> {
        self.graphs
            .values()
            .filter_map(|graph| {
                Some(crate::execution_view::draw(
                    graph.identity.as_ref()?,
                    &graph.accumulator.snapshot()?,
                    graph.document.as_deref(),
                ))
            })
            .collect()
    }
}

#[derive(Default)]
struct Followers {
    sessions: VecDeque<(SessionId, tokio::task::AbortHandle)>,
    processes: VecDeque<(ProcessId, tokio::task::AbortHandle)>,
}

#[derive(Default)]
struct Inner {
    cache: Mutex<Cache>,
    followers: Mutex<Followers>,
}

/// The workbench's execution graph cache and the feed followers that fill it.
#[derive(Clone, Default)]
pub(crate) struct ExecutionGraphs {
    inner: Arc<Inner>,
}

impl ExecutionGraphs {
    /// Every cached graph, drawn, in graph-key order.
    pub(crate) fn graphs(&self) -> Vec<ExecutionGraph> {
        self.inner.cache.lock_recover().drawn()
    }

    /// Drop every graph and stop every follower.
    pub(crate) fn clear(&self) {
        let followers = std::mem::take(&mut *self.inner.followers.lock_recover());
        for (_, follower) in followers.sessions {
            follower.abort();
        }
        for (_, follower) in followers.processes {
            follower.abort();
        }
        *self.inner.cache.lock_recover() = Cache::default();
    }

    /// Follow `session_id`'s feed, and the feed of each process it may see.
    pub(crate) async fn follow(&self, state: &AppState, session_id: &SessionId) {
        self.follow_session(state, session_id);
        match state
            .process_observer
            .snapshot_for_session(session_id)
            .await
        {
            Ok(snapshot) => {
                for process_id in &snapshot.visible_processes {
                    self.follow_process(state, process_id);
                }
            }
            Err(error) => eprintln!(
                "warning: workbench could not list the processes of `{session_id}`: {error}"
            ),
        }
    }

    /// Follow `session_id`'s feed for the language observations of its
    /// cells and the processes it hears about. Following twice is once.
    pub(crate) fn follow_session(&self, state: &AppState, session_id: &SessionId) {
        let mut followers = self.inner.followers.lock_recover();
        if followers.sessions.iter().any(|(id, _)| id == session_id) {
            return;
        }
        let follower = tokio::spawn(follow_session(
            self.clone(),
            state.clone(),
            session_id.clone(),
        ))
        .abort_handle();
        followers.sessions.push_back((session_id.clone(), follower));
        while followers.sessions.len() > MAX_SESSIONS {
            if let Some((_, oldest)) = followers.sessions.pop_front() {
                oldest.abort();
            }
        }
    }

    /// Follow `process_id`'s feed for its language observations and the
    /// committed fact that ends it. Following twice is once.
    pub(crate) fn follow_process(&self, state: &AppState, process_id: &ProcessId) {
        let mut followers = self.inner.followers.lock_recover();
        if followers.processes.iter().any(|(id, _)| id == process_id) {
            return;
        }
        self.inner.cache.lock_recover().follow_process(process_id);
        let follower = tokio::spawn(follow_process(
            self.clone(),
            state.clone(),
            process_id.clone(),
        ))
        .abort_handle();
        followers
            .processes
            .push_back((process_id.clone(), follower));
        while followers.processes.len() > MAX_PROCESSES {
            if let Some((released, oldest)) = followers.processes.pop_front() {
                oldest.abort();
                self.inner.cache.lock_recover().release_process(&released);
            }
        }
    }

    fn session_follower_ended(&self, session_id: &SessionId) {
        self.inner
            .followers
            .lock_recover()
            .sessions
            .retain(|(id, _)| id != session_id);
    }

    fn process_follower_ended(&self, process_id: &ProcessId) {
        let mut followers = self.inner.followers.lock_recover();
        followers.processes.retain(|(id, _)| id != process_id);
        self.inner.cache.lock_recover().release_process(process_id);
    }

    async fn observe(
        &self,
        state: &AppState,
        source: Source,
        observation: &LanguageExecutionObservation,
    ) {
        let missing = self.inner.cache.lock_recover().observe(source, observation);
        self.load(state, missing).await;
    }

    /// Read the document `reference` names from lash and hand it to the
    /// graphs that name it. A document lash cannot answer leaves those
    /// graphs drawn from their observations alone.
    async fn load(&self, state: &AppState, reference: Option<WorkflowDocumentRef>) {
        let Some(reference) = reference else {
            return;
        };
        match state
            .core
            .host_artifacts()
            .execution_document(&reference)
            .await
        {
            Ok(WorkflowDocumentRead::Read(document)) => {
                self.inner.cache.lock_recover().loaded(*document);
            }
            Ok(unreadable) => {
                eprintln!("warning: workbench cannot read a workflow document: {unreadable:?}");
            }
            Err(error) => {
                eprintln!("warning: workbench could not read a workflow document: {error}");
            }
        }
    }
}

async fn follow_session(graphs: ExecutionGraphs, state: AppState, session_id: SessionId) {
    let source = Source::Session(session_id.clone());
    let followed = async {
        let session = Box::pin(state.open_session_for_observation(&session_id)).await?;
        let observed = session.observe();
        // The earliest retained cursor: the feed replays what the session's
        // cells published before the workbench attached.
        let mut feed = observed.subscribe_and_recover(observed.attach().await?.cursor);
        while let Some(item) = feed.next().await {
            match item? {
                SessionObservationStreamItem::Event(event) => match &event.payload {
                    SessionObservationEventPayload::LanguageExecution(observation) => {
                        graphs.observe(&state, source.clone(), observation).await;
                    }
                    SessionObservationEventPayload::ProcessChanged { process_ids, .. } => {
                        for process_id in process_ids {
                            graphs.follow_process(&state, process_id);
                        }
                    }
                    _ => {}
                },
                SessionObservationStreamItem::Gap { .. } => {
                    graphs.inner.cache.lock_recover().reset(&source);
                }
            }
        }
        Ok::<(), lash::EmbedError>(())
    };
    if let Err(error) = followed.await {
        eprintln!("warning: workbench stopped following session `{session_id}`: {error}");
    }
    graphs.session_follower_ended(&session_id);
}

/// The settlement a process's durable end states, from its terminal status
/// and the time of the committed fact that ended it.
fn settlement(status: ProcessStatus, occurred_at_ms: u64) -> Option<WorkflowOverlaySettlement> {
    let terminal = match status {
        ProcessStatus::Completed => WorkflowOverlayTerminal::Completed,
        ProcessStatus::Failed => WorkflowOverlayTerminal::Failed,
        ProcessStatus::Cancelled => WorkflowOverlayTerminal::Cancelled,
        ProcessStatus::Abandoned => WorkflowOverlayTerminal::Abandoned,
        _ => return None,
    };
    Some(WorkflowOverlaySettlement {
        terminal,
        occurred_at: i64::try_from(occurred_at_ms)
            .ok()
            .and_then(chrono::DateTime::from_timestamp_millis),
    })
}

async fn follow_process(graphs: ExecutionGraphs, state: AppState, process_id: ProcessId) {
    let source = Source::Process(process_id.clone());
    // The durable read view replaces what the workbench holds of the
    // process: its end settles its graphs, and the session a child turn runs
    // in is followed for that session's cells. `false` when no process is
    // retained under the id.
    let apply = |read_view: &ProcessReadView| -> Option<bool> {
        let ProcessReadView::Retained(view) = read_view else {
            return None;
        };
        if let Some(child_session_id) = &view.process.child_session_id {
            graphs.follow_session(&state, child_session_id);
        }
        if let ProcessDocumentIdentity::Available(reference) = &view.document {
            graphs
                .inner
                .cache
                .lock_recover()
                .names_document(&process_id, reference.clone());
        }
        let ended = view
            .process
            .lifecycle
            .terminal_at_ms()
            .and_then(|at_ms| settlement(view.process.status(), at_ms));
        if let Some(settlement) = ended {
            graphs
                .inner
                .cache
                .lock_recover()
                .settle(&process_id, settlement);
        }
        Some(ended.is_some())
    };
    let followed = async {
        let observed = state.core.processes().observe(&process_id);
        let snapshot = observed.snapshot().await?;
        let Some(mut ended) = apply(&snapshot.read_view) else {
            return Ok(());
        };
        let mut feed = observed.subscribe_and_recover(snapshot.cursor);
        loop {
            let item = if ended {
                // The feed of an ended process only replays what it retains.
                match tokio::time::timeout(ENDED_PROCESS_DRAIN, feed.next()).await {
                    Ok(item) => item,
                    Err(_quiet) => break,
                }
            } else {
                feed.next().await
            };
            let Some(item) = item else {
                break;
            };
            match item? {
                ProcessObservationStreamItem::Event(event) => match &event.payload {
                    ProcessObservationEventPayload::LanguageExecution(observation) => {
                        if let TraceLanguageExecutionPayload::ChildStarted { child, .. } =
                            &observation.execution.payload
                        {
                            graphs.follow_process(&state, &child.process_id);
                        }
                        graphs.observe(&state, source.clone(), observation).await;
                    }
                    ProcessObservationEventPayload::StepBodyStarted(observation) => {
                        let missing = graphs
                            .inner
                            .cache
                            .lock_recover()
                            .step_body_started(observation);
                        graphs.load(&state, missing).await;
                    }
                    ProcessObservationEventPayload::Committed { event } => {
                        if let ProcessLifecycleFact::Terminal { outcome, .. } = &event.fact
                            && let Some(settlement) =
                                settlement(outcome.status().into(), event.occurred_at_ms)
                        {
                            graphs
                                .inner
                                .cache
                                .lock_recover()
                                .settle(&process_id, settlement);
                            ended = true;
                        }
                    }
                },
                ProcessObservationStreamItem::Gap { observation, .. } => {
                    graphs.inner.cache.lock_recover().reset(&source);
                    match apply(&observation.read_view) {
                        Some(now_ended) => ended = now_ended,
                        None => break,
                    }
                }
            }
        }
        Ok::<(), lash::EmbedError>(())
    };
    if let Err(error) = followed.await {
        eprintln!("warning: workbench stopped following process `{process_id}`: {error}");
    }
    graphs.process_follower_ended(&process_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash::tracing::{TraceLanguageExecution, TraceRuntimeScope, TraceRuntimeSubject};
    use lash::workflow::{WorkflowDocumentEntry, WorkflowOverlayOccurrence};

    fn observation(
        process_id: &ProcessId,
        key: &str,
        observed_at_ms: u64,
        payload: TraceLanguageExecutionPayload,
    ) -> LanguageExecutionObservation {
        LanguageExecutionObservation {
            language: "typescript".to_string(),
            execution: TraceLanguageExecution {
                event_key: key.to_string(),
                identity: TraceLanguageExecutionIdentity {
                    scope: TraceRuntimeScope::none(),
                    subject: TraceRuntimeSubject::Process {
                        process_id: process_id.clone(),
                    },
                    source_identity: "source".to_string(),
                    module_ref: "module".to_string(),
                    entry_kind: "process".to_string(),
                    entry_ref: Some("0:0".to_string()),
                    entry_name: "worker".to_string(),
                    engine_execution_id: Some(process_id.to_string()),
                    generation: None,
                },
                payload,
            },
            observed_at_ms,
        }
    }

    fn started(process_id: &ProcessId) -> LanguageExecutionObservation {
        observation(
            process_id,
            "started",
            1_000,
            TraceLanguageExecutionPayload::ExecutionStarted {
                document: WorkflowDocumentRef {
                    source_identity: "source".to_string(),
                    module_ref: "module".to_string(),
                    entry: WorkflowDocumentEntry::Process {
                        process_ref: "0:0".to_string(),
                    },
                    ir_version: 1,
                },
            },
        )
    }

    fn node_started(process_id: &ProcessId) -> LanguageExecutionObservation {
        observation(
            process_id,
            "node-started",
            1_100,
            TraceLanguageExecutionPayload::NodeStarted {
                node_id: "sleep".to_string(),
                occurrence: 1,
                call_id: None,
                context: Default::default(),
            },
        )
    }

    fn cancelled_at(ms: u64) -> WorkflowOverlaySettlement {
        settlement(ProcessStatus::Cancelled, ms).expect("a terminal status settles")
    }

    /// The committed end of a process settles its graph whichever arrives
    /// first, and node evidence replayed after it never reopens the graph.
    #[test]
    fn a_committed_end_settles_a_process_graph_before_or_after_its_observations() {
        let process_id = ProcessId::fixture("settled");
        let source = Source::Process(process_id.clone());
        for settle_first in [true, false] {
            let mut cache = Cache::default();
            if settle_first {
                cache.settle(&process_id, cancelled_at(2_000));
            }
            cache.observe(source.clone(), &started(&process_id));
            cache.observe(source.clone(), &node_started(&process_id));
            if !settle_first {
                cache.settle(&process_id, cancelled_at(2_000));
            }
            let graph = cache
                .graphs
                .values()
                .next()
                .and_then(|graph| graph.accumulator.snapshot())
                .expect("the process graph");
            assert_eq!(
                graph.settlement.map(|settlement| settlement.terminal),
                Some(WorkflowOverlayTerminal::Cancelled),
                "settle_first={settle_first}"
            );
            assert!(
                matches!(
                    graph.sites[0].occurrence,
                    WorkflowOverlayOccurrence::Cancelled { .. }
                ),
                "settle_first={settle_first}: {:?}",
                graph.sites[0].occurrence
            );
        }
    }

    /// A gap discards a feed's provisional node history and keeps the
    /// durable end; other feeds' graphs are untouched.
    #[test]
    fn a_gap_resets_only_the_graphs_of_its_own_feed() {
        let (gapped, other) = (ProcessId::fixture("gapped"), ProcessId::fixture("other"));
        let mut cache = Cache::default();
        for process_id in [&gapped, &other] {
            let source = Source::Process(process_id.clone());
            cache.observe(source.clone(), &started(process_id));
            cache.observe(source, &node_started(process_id));
        }
        cache.settle(&gapped, cancelled_at(2_000));
        cache.reset(&Source::Process(gapped.clone()));
        let history = |process_id: &ProcessId| {
            cache
                .graphs
                .values()
                .find(|graph| graph.source == Source::Process(process_id.clone()))
                .and_then(|graph| graph.accumulator.snapshot())
                .map(|graph| (graph.history.len(), graph.settlement.is_some()))
        };
        assert_eq!(history(&other), Some((2, false)));
        assert!(
            matches!(history(&gapped), None | Some((0, true))),
            "the gapped graph keeps no provisional history: {:?}",
            history(&gapped)
        );
    }

    /// FIG-5630: completing workflows serially must release their metadata
    /// and answered documents once neither a follower nor a graph needs them.
    #[test]
    fn serial_completions_release_entries_and_documents_with_their_last_graph() {
        let graphs = ExecutionGraphs::default();
        let document_graph =
            lash::typescript::workflow_graph::workflow_graph_from_source_with_facets(
                "finish(1);",
                None,
            )
            .expect("a workflow document");
        let mut released_documents = Vec::new();
        for index in 0..MAX_GRAPHS * 2 {
            let process_id = ProcessId::fixture(&format!("serial-{index}"));
            let mut start = started(&process_id);
            let TraceLanguageExecutionPayload::ExecutionStarted { document } =
                &mut start.execution.payload
            else {
                unreachable!()
            };
            document.source_identity = format!("document-{index}");
            let document = document.clone();
            {
                let mut cache = graphs.inner.cache.lock_recover();
                cache.follow_process(&process_id);
                cache.names_document(&process_id, document.clone());
                cache.settle(&process_id, cancelled_at(2_000));
                cache.observe(Source::Process(process_id.clone()), &start);
                cache.loaded(WorkflowExecutionDocument {
                    reference: document.clone(),
                    graph: document_graph.clone(),
                    entry: None,
                });
                released_documents.push(Arc::downgrade(
                    cache
                        .documents
                        .get(&document)
                        .expect("the answered document"),
                ));
            }
            graphs.process_follower_ended(&process_id);
        }
        let cache = graphs.inner.cache.lock_recover();
        assert_eq!(cache.graphs.len(), MAX_GRAPHS);
        assert_eq!(
            cache.processes.len(),
            MAX_GRAPHS,
            "only retained graphs own ended processes"
        );
        assert_eq!(cache.documents.len(), MAX_GRAPHS);
        assert!(
            released_documents[..MAX_GRAPHS]
                .iter()
                .all(|document| document.upgrade().is_none()),
            "evicted graphs release their answered documents"
        );
        assert!(
            cache.graphs.values().all(|graph| graph
                .accumulator
                .snapshot()
                .is_some_and(|overlay| overlay.settlement.is_some())),
            "retained graphs keep their committed settlement"
        );
    }

    /// FIG-5630: attaching after a cell completed rebuilds its graph from
    /// retained language evidence, without another turn or new observation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn late_session_attachment_rebuilds_a_completed_cells_retained_graph() {
        use crate::tests::{Workbench, run_turn};
        use lash::observe::LiveReplayStore as _;
        use lash::tracing::TraceLanguageExecutionStatus;
        let replay = Arc::new(lash::observe::InMemoryLiveReplayStore::new(
            lash::observe::InMemoryLiveReplayStoreConfig::standard(),
        ));
        let workbench = Workbench::builder(crate::tests::replying_provider(
            "<typescript>let answer = 42; finish(answer);</typescript>",
        ))
        .live_replay(replay.clone())
        .build()
        .await;
        let state = &workbench.state;
        state.execution_graphs.clear();
        let session_id = state.current_session_id();
        run_turn(state, "complete a cell before attachment").await;
        let session = state
            .open_session_for_observation(&session_id)
            .await
            .expect("the completed session");
        let snapshot = session
            .observe()
            .snapshot()
            .await
            .expect("the durable head");
        let revision = snapshot
            .cursor
            .parse_for_session(&session_id)
            .expect("the session cursor")
            .revision;
        let retained = replay
            .replay_after_cursor(&replay.earliest_cursor(&session_id, revision))
            .await
            .expect("retained replay");
        let lash::persistence::LiveReplayOutcome::Replayed(events) = retained else {
            panic!("the completed cell's evidence is retained");
        };
        assert!(events.iter().any(|event| matches!(&event.payload,
            SessionObservationEventPayload::LanguageExecution(observation)
            if matches!(observation.execution.payload, TraceLanguageExecutionPayload::ExecutionFinished { status: TraceLanguageExecutionStatus::Completed, .. }))),
            "the law attaches while the completed cell is still retained");
        state.execution_graphs.follow_session(state, &session_id);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if state.execution_graphs.graphs().iter().any(|graph| {
                    graph.status == TraceLanguageExecutionStatus::Completed
                        && !graph.nodes.is_empty()
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("late attachment rebuilds the completed cell's graph");
        state.execution_graphs.clear();
        workbench.shutdown().await;
    }

    /// The cache keeps the most recently fed graphs and no more.
    #[test]
    fn the_cache_keeps_the_most_recently_fed_graphs() {
        let mut cache = Cache::default();
        let first = ProcessId::fixture("graph-0");
        for index in 0..=MAX_GRAPHS {
            let process_id = ProcessId::fixture(&format!("graph-{index}"));
            cache.observe(Source::Process(process_id.clone()), &started(&process_id));
        }
        assert_eq!(cache.graphs.len(), MAX_GRAPHS);
        assert!(
            cache
                .graphs
                .values()
                .all(|graph| graph.source != Source::Process(first.clone())),
            "the least recently fed graph is the one released"
        );
    }
}
