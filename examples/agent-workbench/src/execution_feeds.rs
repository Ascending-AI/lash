//! The workbench's execution graphs: a bounded cache of pure graph folds, fed
//! from lash's session and process feeds.
//!
//! A session's cells publish their language observations on the session's
//! feed; a process publishes its own on the process feed, with the committed
//! facts that settle it. The workbench follows both and folds what they
//! deliver into one [`TraceLashlangGraphAccumulator`] per graph key. The
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
    LanguageExecutionObservation, ProcessLifecycleFact, ProcessObservationEventPayload,
    ProcessObservationStreamItem, ProcessReadView, ProcessStatus,
};
use lash::sync::MutexExt;
use lash::tracing::{
    TraceLanguageExecutionPayload, TraceLashlangGraph, TraceLashlangGraphAccumulator,
    TraceLashlangGraphSettlement, TraceLashlangGraphTerminal,
};
use lash::{ProcessId, SessionId};

use crate::AppState;

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
    accumulator: TraceLashlangGraphAccumulator,
    /// The cache's feed count when this graph was last fed.
    fed: u64,
}

#[derive(Default)]
struct Cache {
    fed: u64,
    graphs: BTreeMap<String, CachedGraph>,
    /// The committed end of each followed process, applied to its graphs
    /// whenever they appear.
    settlements: BTreeMap<ProcessId, TraceLashlangGraphSettlement>,
}

impl Cache {
    fn observe(&mut self, source: Source, observation: &LanguageExecutionObservation) {
        self.fed += 1;
        let fed = self.fed;
        let graph_key = observation.execution.identity.graph_key();
        let settlement = match &source {
            Source::Process(process_id) => self.settlements.get(process_id).copied(),
            Source::Session(_) => None,
        };
        let graph = self.graphs.entry(graph_key).or_insert_with(|| {
            let mut accumulator = TraceLashlangGraphAccumulator::default();
            if let Some(settlement) = settlement {
                accumulator.settle(settlement);
            }
            CachedGraph {
                source,
                accumulator,
                fed,
            }
        });
        graph.fed = fed;
        if let Err(error) = graph.accumulator.observe(observation) {
            eprintln!("warning: workbench execution graph refused an observation: {error}");
        }
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
    }

    fn settle(&mut self, process_id: &ProcessId, settlement: TraceLashlangGraphSettlement) {
        self.settlements.insert(process_id.clone(), settlement);
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
    /// Every cached graph, in graph-key order.
    pub(crate) fn graphs(&self) -> Vec<TraceLashlangGraph> {
        self.inner
            .cache
            .lock_recover()
            .graphs
            .values()
            .filter_map(|graph| graph.accumulator.snapshot())
            .collect()
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
                self.inner
                    .cache
                    .lock_recover()
                    .settlements
                    .remove(&released);
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
        self.inner
            .followers
            .lock_recover()
            .processes
            .retain(|(id, _)| id != process_id);
    }

    fn observe(&self, source: Source, observation: &LanguageExecutionObservation) {
        self.inner.cache.lock_recover().observe(source, observation);
    }
}

async fn follow_session(graphs: ExecutionGraphs, state: AppState, session_id: SessionId) {
    let source = Source::Session(session_id.clone());
    let followed = async {
        let session = Box::pin(state.open_session_for_observation(&session_id)).await?;
        let observed = session.observe();
        // The earliest retained cursor: the feed replays what the session's
        // cells published before the workbench attached.
        let mut feed = observed.subscribe_and_recover(observed.snapshot().await?.cursor);
        while let Some(item) = feed.next().await {
            match item? {
                SessionObservationStreamItem::Event(event) => match &event.payload {
                    SessionObservationEventPayload::LanguageExecution(observation) => {
                        graphs.observe(source.clone(), observation);
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
fn settlement(status: ProcessStatus, occurred_at_ms: u64) -> Option<TraceLashlangGraphSettlement> {
    let terminal = match status {
        ProcessStatus::Completed => TraceLashlangGraphTerminal::Completed,
        ProcessStatus::Failed => TraceLashlangGraphTerminal::Failed,
        ProcessStatus::Cancelled => TraceLashlangGraphTerminal::Cancelled,
        ProcessStatus::Abandoned => TraceLashlangGraphTerminal::Abandoned,
        _ => return None,
    };
    Some(TraceLashlangGraphSettlement {
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
                        graphs.observe(source.clone(), observation);
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
    use lash::tracing::{
        TraceLanguageExecution, TraceLanguageExecutionIdentity, TraceLanguageExecutionMap,
        TraceLanguageExecutionMapNode, TraceLashlangNodeObservation, TraceRuntimeScope,
        TraceRuntimeSubject,
    };

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
                execution_map: TraceLanguageExecutionMap {
                    nodes: vec![TraceLanguageExecutionMapNode {
                        id: "sleep".to_string(),
                        site: lash::vm::WorkflowExecutionSite::new(
                            "worker",
                            [0],
                            lash::tracing::ExecutionNodeKind::Call,
                            "sleep",
                        ),
                        kind: lash::tracing::ExecutionNodeKind::Call,
                        label: "sleep".to_string(),
                        branch_memberships: Vec::new(),
                        label_metadata: None,
                    }],
                    edges: Vec::new(),
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
                node_kind: lash::tracing::ExecutionNodeKind::Call,
                label: "sleep".to_string(),
                occurrence: 1,
                call_id: None,
            },
        )
    }

    fn cancelled_at(ms: u64) -> TraceLashlangGraphSettlement {
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
                Some(TraceLashlangGraphTerminal::Cancelled),
                "settle_first={settle_first}"
            );
            assert!(
                matches!(
                    graph.nodes[0].observation,
                    TraceLashlangNodeObservation::Cancelled { .. }
                ),
                "settle_first={settle_first}: {:?}",
                graph.nodes[0].observation
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
