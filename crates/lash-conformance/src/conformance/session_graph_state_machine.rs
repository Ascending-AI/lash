//! Model-based property laws for the durable session graph.
//!
//! The operation language and reference model live in `lash-core` so every
//! backend executes the same cases. Backend tests provide only a fresh
//! [`ConformanceDeployment`](crate::store::ConformanceDeployment) for each
//! case: a deployment store with the test seams that corrupt its accelerators.

use crate::facade_support::SessionGraphFacadeOps;
use lash_core::plugin::PluginSessionRequest;
use lash_core::testing::RuntimeStoreTestShiftExt as _;
use lash_sansio::SessionId;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestError, TestRunner};

use super::run_shape::Counter;
use super::*;

const SESSION_COUNT: u8 = 3;
const DEFAULT_CASES: u32 = 24;
const DEFAULT_RUNNER_SEED: u64 = 856;
const MAX_OPS: usize = 40;
const GENERATED_PREFIX_OPS: usize = 19;
const DEDICATED_LAW_SEED: u64 = 0x856d_ed1c_a7ed;
const TRAVERSAL_WATCHDOG: Duration = Duration::from_secs(2);

/// Operations generated against the session graph and its store contract.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SessionGraphContractOp {
    Append {
        session: u8,
        node_count: u8,
        requirement: u8,
    },
    /// Fork `source` at one of its retained revisions (the selector picks
    /// the head, an earlier leaf, the oldest retained, or one not retained).
    Fork {
        source: u8,
        target: u8,
        revision: u8,
    },
    Pin {
        session: u8,
        revision: u8,
    },
    Unpin {
        session: u8,
        revision: u8,
    },
    Delete {
        session: u8,
    },
    TruncateRewind {
        session: u8,
        revision: u8,
    },
    ReachabilitySweep,
    /// Raise one live session's fork ceiling on an ancestor owner to a node
    /// that owner holds off the session's path, check that readability and
    /// the active-ancestor predicate still follow the session's parent edges
    /// exactly, then restore the honest ceiling (ADR 0057, edge authority).
    InflateCeiling {
        session: u8,
        node: u8,
    },
    TombstoneVacuum,
    CheckpointCommit {
        session: u8,
    },
    ColdReload {
        session: u8,
    },
    Malformed {
        session: u8,
        shape: u8,
    },
    StaleHeadCas {
        session: u8,
    },
}

#[derive(Clone, Debug, serde::Deserialize)]
struct GeneratedCase {
    seed: u64,
    operations: Vec<SessionGraphContractOp>,
}

#[derive(Clone, Debug)]
struct ModelNode {
    parent_node_id: Option<lash_core::NodeId>,
    owner_session_id: SessionId,
}

#[derive(Clone, Debug)]
struct ModelSession {
    physical_id: String,
    path: Vec<lash_core::NodeId>,
    head_revision: u64,
    /// The revisions the store still retains, each with the leaf it
    /// published. A sweep drops every one that is neither head nor pinned.
    revisions: BTreeMap<u64, Option<lash_core::NodeId>>,
    pins: BTreeSet<u64>,
}

impl ModelSession {
    fn created(physical_id: String, path: Vec<lash_core::NodeId>) -> Self {
        let revisions = BTreeMap::from([(0, path.last().cloned())]);
        Self {
            physical_id,
            path,
            head_revision: 0,
            revisions,
            pins: BTreeSet::new(),
        }
    }

    fn sweep(&mut self) {
        let head = self.head_revision;
        let pins = &self.pins;
        self.revisions
            .retain(|revision, _| *revision == head || pins.contains(revision));
    }
}

/// What a revision selector names in one session.
enum SelectedRevision {
    Retained(u64, Option<lash_core::NodeId>),
    /// Published once and collected since.
    Pruned(u64),
    /// Past the head: nothing has published it yet.
    Pending(u64),
}

impl SelectedRevision {
    fn revision(&self) -> u64 {
        match self {
            Self::Retained(revision, _) | Self::Pruned(revision) | Self::Pending(revision) => {
                *revision
            }
        }
    }

    fn refused_as(&self, error: &crate::StoreError) -> bool {
        match self {
            Self::Retained(..) => false,
            Self::Pruned(_) => matches!(error, crate::StoreError::ForkTargetPruned { .. }),
            Self::Pending(_) => matches!(error, crate::StoreError::ForkTargetPending { .. }),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct ReferenceModel {
    sessions: BTreeMap<u8, ModelSession>,
    nodes: BTreeMap<lash_core::NodeId, ModelNode>,
    next_session_generation: u64,
    next_operation: u64,
}

struct LiveSession {
    request: crate::SessionStoreCreateRequest,
    store: crate::store::SessionStore,
}

struct SessionGraphScenario {
    seed: u64,
    factory: Arc<dyn crate::store::ConformanceDeployment>,
    live: BTreeMap<u8, LiveSession>,
    handles_by_physical_id: BTreeMap<String, crate::store::SessionStore>,
    model: ReferenceModel,
    shape: RunShape,
}

/// The run-shape counter alphabet. `RunShape`, `RunShapeTotals`, the
/// required-shape table, and the report all derive from this one enum, so a
/// new counter cannot be counted without being reported.
#[derive(Clone, Copy, Debug)]
enum RunShapeCounter {
    AppendsCommitted,
    AncestorAppendsCommitted,
    ForksCommitted,
    RewindsCommitted,
    PinsCommitted,
    UnpinsCommitted,
    DeletesCommitted,
    CheckpointCommits,
    ColdReloads,
    ReachabilitySweeps,
    InflatedCeilings,
    VacuumRuns,
    TypedRejections,
    BoundedTraversals,
}

impl Counter for RunShapeCounter {
    const ALL: &'static [Self] = &[
        Self::AppendsCommitted,
        Self::AncestorAppendsCommitted,
        Self::ForksCommitted,
        Self::RewindsCommitted,
        Self::PinsCommitted,
        Self::UnpinsCommitted,
        Self::DeletesCommitted,
        Self::CheckpointCommits,
        Self::ColdReloads,
        Self::ReachabilitySweeps,
        Self::InflatedCeilings,
        Self::VacuumRuns,
        Self::TypedRejections,
        Self::BoundedTraversals,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::AppendsCommitted => "appends_committed",
            Self::AncestorAppendsCommitted => "ancestor_appends_committed",
            Self::ForksCommitted => "forks_committed",
            Self::RewindsCommitted => "rewinds_committed",
            Self::PinsCommitted => "pins_committed",
            Self::UnpinsCommitted => "unpins_committed",
            Self::DeletesCommitted => "deletes_committed",
            Self::CheckpointCommits => "checkpoint_commits",
            Self::ColdReloads => "cold_reloads",
            Self::ReachabilitySweeps => "reachability_sweeps",
            Self::InflatedCeilings => "inflated_ceilings",
            Self::VacuumRuns => "vacuum_runs",
            Self::TypedRejections => "typed_rejections",
            Self::BoundedTraversals => "bounded_traversals",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

type RunShape = run_shape::RunShape<RunShapeCounter>;
type RunShapeTotals = run_shape::RunShapeTotals<RunShapeCounter>;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_graph_state_machine<F, Fut>(backend: &'static str, make: F)
where
    F: Fn(u64) -> Fut + Send + Sync + Clone + 'static,
    Fut: Future<Output = Arc<dyn crate::store::ConformanceDeployment>> + Send + 'static,
{
    let first = make(u64::MAX - 1).await;
    let second = make(u64::MAX - 1).await;
    assert!(
        !Arc::ptr_eq(&first, &second),
        "session_graph_state_machine factory reused one Arc"
    );
    drop((first, second));
    let cases = std::env::var("LASH_SESSION_GRAPH_PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_CASES);
    let runner_seed = std::env::var("LASH_SESSION_GRAPH_PROPTEST_SEED")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_RUNNER_SEED);
    let config = Config {
        cases,
        max_shrink_iters: 8_192,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(runner_seed),
        ..Config::default()
    };

    assert_dedicated_laws(&make, DEDICATED_LAW_SEED)
        .await
        .unwrap_or_else(|reason| panic!("{backend} dedicated session-graph law failed: {reason}"));

    let runtime = tokio::runtime::Handle::current();
    let totals = Arc::new(RunShapeTotals::default());
    let runner_totals = Arc::clone(&totals);
    let result = tokio::task::spawn_blocking(move || {
        let mut runner = TestRunner::new(config);
        runner.run(&generated_case(), |case| {
            runtime.block_on(async {
                let factory = make(case.seed).await;
                let shape = Box::pin(replay_case(case.seed, factory, &case.operations)).await?;
                prop_assert!(
                    shape[RunShapeCounter::AncestorAppendsCommitted] > 0,
                    "generated alphabet starvation: no ancestor-based append committed"
                );
                prop_assert!(
                    shape[RunShapeCounter::ForksCommitted] > 0
                        && shape[RunShapeCounter::RewindsCommitted] > 0,
                    "generated alphabet starvation: fork/rewind lifecycle was not reached"
                );
                prop_assert!(
                    shape[RunShapeCounter::InflatedCeilings] > 0,
                    "generated alphabet starvation: no fork ceiling was inflated"
                );
                prop_assert!(
                    shape[RunShapeCounter::TypedRejections] >= 5,
                    "generated alphabet starvation: malformed and stale paths were not rejected"
                );
                prop_assert!(
                    shape[RunShapeCounter::BoundedTraversals] >= 4,
                    "generated alphabet starvation: malformed traversal shapes were not exercised"
                );
                runner_totals.add(&shape);
                Ok(())
            })
        })
    })
    .await
    .expect("session-graph property runner task");

    if let Err(error) = result {
        persist_counterexample(backend, runner_seed, &error);
        panic!(
            "{backend} session-graph property law failed with runner seed {runner_seed}; replay with LASH_SESSION_GRAPH_PROPTEST_SEED={runner_seed}: {error}"
        );
    }

    eprintln!(
        "session-graph run shape ({backend}, cases={cases}): {}",
        totals.report()
    );
}

fn generated_case() -> impl Strategy<Value = GeneratedCase> {
    (
        any::<u64>(),
        prop::collection::vec(operation(), 1..=(MAX_OPS - GENERATED_PREFIX_OPS)),
    )
        .prop_map(|(seed, random_operations)| {
            let mut operations = generated_prefix();
            operations.extend(random_operations);
            GeneratedCase { seed, operations }
        })
}

fn generated_prefix() -> Vec<SessionGraphContractOp> {
    vec![
        SessionGraphContractOp::Append {
            session: 0,
            node_count: 1,
            requirement: 0,
        },
        SessionGraphContractOp::Pin {
            session: 0,
            revision: 0,
        },
        SessionGraphContractOp::Append {
            session: 0,
            node_count: 1,
            requirement: 1,
        },
        SessionGraphContractOp::CheckpointCommit { session: 0 },
        SessionGraphContractOp::Fork {
            source: 0,
            target: 1,
            revision: 1,
        },
        SessionGraphContractOp::Append {
            session: 1,
            node_count: 1,
            requirement: 2,
        },
        SessionGraphContractOp::InflateCeiling {
            session: 1,
            node: 0,
        },
        SessionGraphContractOp::Pin {
            session: 1,
            revision: 0,
        },
        SessionGraphContractOp::Append {
            session: 1,
            node_count: 1,
            requirement: 1,
        },
        SessionGraphContractOp::TruncateRewind {
            session: 1,
            revision: 1,
        },
        SessionGraphContractOp::Unpin {
            session: 1,
            revision: 0,
        },
        SessionGraphContractOp::ColdReload { session: 0 },
        SessionGraphContractOp::ReachabilitySweep,
        SessionGraphContractOp::Malformed {
            session: 0,
            shape: 0,
        },
        SessionGraphContractOp::Malformed {
            session: 0,
            shape: 1,
        },
        SessionGraphContractOp::Malformed {
            session: 0,
            shape: 2,
        },
        SessionGraphContractOp::Malformed {
            session: 0,
            shape: 3,
        },
        SessionGraphContractOp::StaleHeadCas { session: 0 },
        SessionGraphContractOp::Delete { session: 1 },
        SessionGraphContractOp::TombstoneVacuum,
    ]
}

fn operation() -> impl Strategy<Value = SessionGraphContractOp> {
    prop_oneof![
        8 => (0..SESSION_COUNT, 1_u8..=3, 0_u8..4).prop_map(
            |(session, node_count, requirement)| SessionGraphContractOp::Append {
                session,
                node_count,
                requirement,
            },
        ),
        3 => (0..SESSION_COUNT, 0..SESSION_COUNT, 0_u8..4).prop_map(
            |(source, target, revision)| SessionGraphContractOp::Fork { source, target, revision },
        ),
        2 => (0..SESSION_COUNT, 0_u8..4)
            .prop_map(|(session, revision)| SessionGraphContractOp::Pin { session, revision }),
        2 => (0..SESSION_COUNT, 0_u8..4)
            .prop_map(|(session, revision)| SessionGraphContractOp::Unpin { session, revision }),
        1 => (0..SESSION_COUNT).prop_map(|session| SessionGraphContractOp::Delete { session }),
        2 => (0..SESSION_COUNT, 0_u8..4).prop_map(|(session, revision)| {
            SessionGraphContractOp::TruncateRewind { session, revision }
        }),
        2 => Just(SessionGraphContractOp::ReachabilitySweep),
        2 => (0..SESSION_COUNT, 0_u8..8)
            .prop_map(|(session, node)| SessionGraphContractOp::InflateCeiling { session, node }),
        2 => Just(SessionGraphContractOp::TombstoneVacuum),
        3 => (0..SESSION_COUNT)
            .prop_map(|session| SessionGraphContractOp::CheckpointCommit { session }),
        2 => (0..SESSION_COUNT)
            .prop_map(|session| SessionGraphContractOp::ColdReload { session }),
        4 => (0..SESSION_COUNT, 0_u8..4)
            .prop_map(|(session, shape)| SessionGraphContractOp::Malformed { session, shape }),
        2 => (0..SESSION_COUNT)
            .prop_map(|session| SessionGraphContractOp::StaleHeadCas { session }),
    ]
}

async fn replay_case(
    seed: u64,
    factory: Arc<dyn crate::store::ConformanceDeployment>,
    operations: &[SessionGraphContractOp],
) -> Result<RunShape, TestCaseError> {
    let mut scenario = SessionGraphScenario::new(seed, factory);
    for (step, operation) in operations.iter().enumerate() {
        Box::pin(scenario.apply(operation))
            .await
            .map_err(|reason| {
                TestCaseError::fail(format!("step {step} {operation:?}: {reason}"))
            })?;
        scenario.assert_model_agreement().await.map_err(|reason| {
            TestCaseError::fail(format!(
                "model agreement at step {step} {operation:?}: {reason}"
            ))
        })?;
    }
    Ok(scenario.shape)
}

impl SessionGraphScenario {
    fn new(seed: u64, factory: Arc<dyn crate::store::ConformanceDeployment>) -> Self {
        Self {
            seed,
            factory,
            live: BTreeMap::new(),
            handles_by_physical_id: BTreeMap::new(),
            model: ReferenceModel::default(),
            shape: RunShape::default(),
        }
    }

    async fn apply(&mut self, operation: &SessionGraphContractOp) -> Result<(), String> {
        match operation {
            SessionGraphContractOp::Append {
                session,
                node_count,
                requirement,
            } => Box::pin(self.append(*session, *node_count, *requirement)).await,
            SessionGraphContractOp::Fork {
                source,
                target,
                revision,
            } => self.fork(*source, *target, *revision).await,
            SessionGraphContractOp::Pin { session, revision } => {
                self.pin(*session, *revision).await
            }
            SessionGraphContractOp::Unpin { session, revision } => {
                self.unpin(*session, *revision).await
            }
            SessionGraphContractOp::Delete { session } => self.delete(*session).await,
            SessionGraphContractOp::TruncateRewind { session, revision } => {
                self.truncate_rewind(*session, *revision).await
            }
            SessionGraphContractOp::ReachabilitySweep => self.reachability_sweep().await,
            SessionGraphContractOp::InflateCeiling { session, node } => {
                self.inflate_ceiling(*session, *node).await
            }
            SessionGraphContractOp::TombstoneVacuum => self.tombstone_vacuum().await,
            SessionGraphContractOp::CheckpointCommit { session } => {
                self.checkpoint_commit(*session).await
            }
            SessionGraphContractOp::ColdReload { session } => self.cold_reload(*session).await,
            SessionGraphContractOp::Malformed { session, shape } => {
                Box::pin(self.malformed(*session, *shape)).await
            }
            SessionGraphContractOp::StaleHeadCas { session } => {
                Box::pin(self.stale_head_cas(*session)).await
            }
        }
    }

    async fn ensure_session(&mut self, slot: u8) -> Result<(), String> {
        let slot = slot % SESSION_COUNT;
        if self.live.contains_key(&slot) {
            return Ok(());
        }
        let physical_id = self.next_session_id(slot);
        let request = session_store_request(
            &SessionId::fixture(physical_id.clone()),
            "session-graph-property-model",
            crate::SessionRelation::Root,
        );
        let store = self
            .factory
            .admit_view(&request)
            .await
            .map_err(|error| error.to_string())?;
        self.handles_by_physical_id
            .insert(physical_id.clone(), store.clone());
        self.live.insert(slot, LiveSession { request, store });
        self.model
            .sessions
            .insert(slot, ModelSession::created(physical_id, Vec::new()));
        Ok(())
    }

    fn next_session_id(&mut self, slot: u8) -> String {
        let generation = self.model.next_session_generation;
        self.model.next_session_generation += 1;
        format!("sg-prop-{}-{slot}-{generation}", self.seed)
    }

    fn next_operation_id(&mut self, kind: &str) -> String {
        let operation = self.model.next_operation;
        self.model.next_operation += 1;
        format!("sg-prop-{}-{kind}-{operation}", self.seed)
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn append(&mut self, slot: u8, node_count: u8, requirement: u8) -> Result<(), String> {
        let slot = slot % SESSION_COUNT;
        self.ensure_session(slot).await?;
        let before = self.session_snapshot(slot).await?;
        let old_path = self
            .model
            .sessions
            .get(&slot)
            .expect("ensured model session")
            .path
            .clone();
        let required = match requirement % 4 {
            0 => None,
            1 => old_path.first().cloned(),
            2 => old_path.last().cloned(),
            _ => Some(lash_core::NodeId::fixture(format!(
                "missing-required-{}",
                self.model.next_operation
            ))),
        };
        let operation_id = self.next_operation_id("append");
        let live = self.live.get(&slot).expect("ensured live session");
        let runtime = property_runtime(live.store.store(), &live.request).await?;
        // The plugin-facing service writes straight through the store's
        // append path: nothing works this session, so nothing owns its head.
        let service = runtime
            .session_graph_service()
            .map_err(|error| error.to_string())?;
        let nodes = (0..usize::from(node_count.max(1)))
            .map(|ordinal| {
                crate::SessionAppendNode::plugin(
                    "session-graph-property",
                    serde_json::json!({"operation": operation_id, "ordinal": ordinal}),
                )
            })
            .collect();
        let result = Box::pin(service.append_session_nodes(
            &live.request.session_id,
            crate::AppendSessionNodesRequest {
                operation_id,
                nodes,
                requires_ancestor_node_id: required.clone(),
            },
        ))
        .await
        .map_err(|error| error.to_string())?;

        let required_is_live = required
            .as_ref()
            .is_none_or(|node_id| old_path.contains(node_id));
        if !required_is_live {
            if !matches!(
                result,
                crate::AppendSessionNodesOutcome::StaleBranch { ref required_node_id }
                    if Some(required_node_id) == required.as_ref()
            ) {
                return Err(format!(
                    "branch-liveness: stale base was not rejected with its typed identity: {result:?}"
                ));
            }
            let after = self.session_snapshot(slot).await?;
            if before != after {
                return Err("branch-liveness: stale-base rejection mutated the session".to_string());
            }
            self.shape[RunShapeCounter::TypedRejections] += 1;
            return Ok(());
        }

        let crate::AppendSessionNodesOutcome::Appended { node_ids, .. } = result else {
            return Err("branch-liveness: active ancestor append was rejected".to_string());
        };
        if node_ids.len() != usize::from(node_count.max(1)) {
            return Err(format!(
                "append returned {} ids for {} requested nodes",
                node_ids.len(),
                node_count.max(1)
            ));
        }
        let read = self.read_live(slot).await?;
        let actual_path = graph_path_ids(&read.window)?;
        if !actual_path.starts_with(&old_path) {
            return Err(format!(
                "append-only history: prior path {old_path:?} is not a prefix of {actual_path:?}"
            ));
        }
        if !old_path.is_empty()
            && actual_path
                .get(old_path.len())
                .and_then(|node_id| read.window.find_node(node_id))
                .and_then(|node| node.parent_node_id.as_ref())
                != old_path.last()
        {
            return Err(
                "first-parent-equals-leaf: append did not parent on the current leaf".to_string(),
            );
        }
        self.record_read(slot, &read)?;
        self.shape[RunShapeCounter::AppendsCommitted] += 1;
        if required.is_some() && required.as_ref() != old_path.last() {
            self.shape[RunShapeCounter::AncestorAppendsCommitted] += 1;
        }
        Ok(())
    }

    /// A pin is a name, written whether or not its revision exists: a pin of
    /// a revision the session has not published holds it once it is, and a
    /// pin of a collected one holds nothing.
    async fn pin(&mut self, slot: u8, selector: u8) -> Result<(), String> {
        let slot = slot % SESSION_COUNT;
        let (Some(selected), Some(live)) =
            (self.selected_revision(slot, selector), self.live.get(&slot))
        else {
            return Ok(());
        };
        let revision = selected.revision();
        // A second pin of the same target is the first one.
        for _ in 0..2 {
            live.store
                .pin(&crate::Target::Revision(revision))
                .await
                .map_err(|error| format!("pin of revision {revision}: {error}"))?;
        }
        self.model
            .sessions
            .get_mut(&slot)
            .ok_or("pinned session is not modeled")?
            .pins
            .insert(revision);
        self.shape[RunShapeCounter::PinsCommitted] += 1;
        Ok(())
    }

    async fn unpin(&mut self, slot: u8, selector: u8) -> Result<(), String> {
        let slot = slot % SESSION_COUNT;
        let (Some(selected), Some(live)) =
            (self.selected_revision(slot, selector), self.live.get(&slot))
        else {
            return Ok(());
        };
        let revision = selected.revision();
        live.store
            .unpin(&crate::Target::Revision(revision))
            .await
            .map_err(|error| error.to_string())?;
        if let Some(session) = self.model.sessions.get_mut(&slot) {
            session.pins.remove(&revision);
        }
        self.shape[RunShapeCounter::UnpinsCommitted] += 1;
        Ok(())
    }

    /// Fork `source_slot`'s `selected` revision into a new physical session.
    /// Returns the fork's request and store, or `None` after the store
    /// refused a revision it no longer (or does not yet) hold with the typed
    /// cause the model expects.
    async fn fork_revision(
        &mut self,
        source_slot: u8,
        selected: &SelectedRevision,
        physical_id: &str,
    ) -> Result<Option<(LiveSession, Vec<lash_core::NodeId>)>, String> {
        let source_session_id = SessionId::fixture(
            self.model
                .sessions
                .get(&source_slot)
                .ok_or("fork source is not modeled")?
                .physical_id
                .clone(),
        );
        let leaf = match selected {
            SelectedRevision::Retained(_, leaf) => leaf.clone(),
            SelectedRevision::Pruned(_) | SelectedRevision::Pending(_) => None,
        };
        let relation = crate::SessionRelation::Fork {
            source_session_id: source_session_id.clone(),
            source_node_id: leaf.clone(),
        };
        let request = session_store_request(
            &SessionId::fixture(physical_id.to_string()),
            "session-graph-property-model",
            relation.clone(),
        );
        let result = self
            .factory
            .fork_session(&crate::ForkSessionRequest {
                pending_observer_intents: Vec::new(),
                session_id: SessionId::fixture(physical_id.to_string()),
                source_session_id,
                head_revision: selected.revision(),
                relation,
                config: request.config.session_policy().into(),
            })
            .await;
        let receipt = match result {
            Ok(receipt) if matches!(selected, SelectedRevision::Retained(..)) => receipt,
            Err(error) if selected.refused_as(&error) => {
                self.shape[RunShapeCounter::TypedRejections] += 1;
                return Ok(None);
            }
            other => {
                return Err(format!(
                    "fork isolation: revision {} answered the wrong way: {other:?}",
                    selected.revision()
                ));
            }
        };
        if receipt.leaf_node_id != leaf {
            return Err(format!(
                "fork isolation: revision {} forked at leaf {:?}, the revision published {leaf:?}",
                selected.revision(),
                receipt.leaf_node_id
            ));
        }
        let store = self
            .factory
            .live_view_for(&request)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "fork created no reopenable store".to_string())?;
        let path = self.path_to(leaf.as_ref());
        Ok(Some((LiveSession { request, store }, path)))
    }

    async fn fork(&mut self, source: u8, target: u8, selector: u8) -> Result<(), String> {
        let source = source % SESSION_COUNT;
        let target = target % SESSION_COUNT;
        let Some(selected) = self.selected_revision(source, selector) else {
            return Ok(());
        };
        if self.live.contains_key(&target) {
            return Ok(());
        }
        let physical_id = self.next_session_id(target);
        let Some((live, path)) = self.fork_revision(source, &selected, &physical_id).await? else {
            return Ok(());
        };
        self.handles_by_physical_id
            .insert(physical_id.clone(), live.store.clone());
        self.live.insert(target, live);
        self.model
            .sessions
            .insert(target, ModelSession::created(physical_id, path));
        self.shape[RunShapeCounter::ForksCommitted] += 1;
        Ok(())
    }

    /// Rewind is fork-then-delete: any revision the session still retains is
    /// a rewind target, with no pin taken beforehand.
    async fn truncate_rewind(&mut self, slot: u8, selector: u8) -> Result<(), String> {
        let slot = slot % SESSION_COUNT;
        let Some(selected) = self.selected_revision(slot, selector) else {
            return Ok(());
        };
        let physical_id = self.next_session_id(slot);
        let Some((live, path)) = self.fork_revision(slot, &selected, &physical_id).await? else {
            return Ok(());
        };
        let old = self
            .live
            .insert(slot, live)
            .ok_or("rewound session was not live")?;
        self.factory
            .delete_session(&old.request.session_id)
            .await
            .map_err(|error| error.to_string())?;
        self.handles_by_physical_id.insert(
            physical_id.clone(),
            self.live
                .get(&slot)
                .ok_or("rewind fork is not live")?
                .store
                .clone(),
        );
        self.model
            .sessions
            .insert(slot, ModelSession::created(physical_id, path));
        self.shape[RunShapeCounter::RewindsCommitted] += 1;
        self.shape[RunShapeCounter::DeletesCommitted] += 1;
        Ok(())
    }

    async fn delete(&mut self, slot: u8) -> Result<(), String> {
        let slot = slot % SESSION_COUNT;
        let Some(live) = self.live.remove(&slot) else {
            return Ok(());
        };
        self.factory
            .delete_session(&live.request.session_id)
            .await
            .map_err(|error| error.to_string())?;
        self.model.sessions.remove(&slot);
        self.shape[RunShapeCounter::DeletesCommitted] += 1;
        Ok(())
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn checkpoint_commit(&mut self, slot: u8) -> Result<(), String> {
        let slot = slot % SESSION_COUNT;
        self.ensure_session(slot).await?;
        if self
            .model
            .sessions
            .get(&slot)
            .is_none_or(|session| session.path.is_empty())
        {
            return Ok(());
        }
        let operation = self.next_operation_id("checkpoint");
        let live = self.live.get(&slot).expect("ensured live session");
        let mut state = crate::conformance::helpers::load_window_state(
            live.store.store(),
            live.store.session_id(),
        )
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "checkpoint subject has no persisted state".to_string())?;
        state.turn_index += 1;
        let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state);
        commit.turn_commit = crate::RuntimeTurnCommitStamp::new(crate::OperationId::turn(
            &state.session_id,
            crate::TurnId::fixture(operation),
            "checkpoint",
        ));
        commit_runtime_state_for_property(live.store.store(), commit, "checkpoint")
            .await
            .map_err(|error| error.to_string())?;
        let model = self
            .model
            .sessions
            .get_mut(&slot)
            .expect("checkpoint model");
        model.head_revision += 1;
        model
            .revisions
            .insert(model.head_revision, model.path.last().cloned());
        self.shape[RunShapeCounter::CheckpointCommits] += 1;
        Ok(())
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn cold_reload(&mut self, slot: u8) -> Result<(), String> {
        let slot = slot % SESSION_COUNT;
        let Some(live) = self.live.get(&slot) else {
            return Ok(());
        };
        let before = persisted_projection(&live.store).await?;
        let reopened = self
            .factory
            .live_view_for(&live.request)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "cold reload lost a live session".to_string())?;
        let after = persisted_projection(&reopened).await?;
        if before != after {
            return Err("cold-reload projection equality: reopened state differs".to_string());
        }
        self.handles_by_physical_id.insert(
            live.request.session_id.clone().to_string(),
            reopened.clone(),
        );
        self.live.get_mut(&slot).expect("live slot").store = reopened;
        self.shape[RunShapeCounter::ColdReloads] += 1;
        Ok(())
    }

    async fn reachability_sweep(&mut self) -> Result<(), String> {
        if let Some(store) = self.live.values().next().map(|live| &live.store) {
            store
                .store()
                .gc_unreachable()
                .await
                .map_err(|error| error.to_string())?;
            // Host GC is the one place the default policy releases: every
            // revision that is neither a head nor pinned goes.
            for session in self.model.sessions.values_mut() {
                session.sweep();
            }
        }
        self.assert_reachability().await?;
        self.shape[RunShapeCounter::ReachabilitySweeps] += 1;
        Ok(())
    }

    async fn inflate_ceiling(&mut self, slot: u8, selector: u8) -> Result<(), String> {
        let slot = slot % SESSION_COUNT;
        let (Some(session), Some(live)) = (self.model.sessions.get(&slot), self.live.get(&slot))
        else {
            return Ok(());
        };
        let session_id = SessionId::fixture(session.physical_id.clone());
        // The honest ceilings: the highest node each ancestor owner holds on
        // this session's path (the ForkPlan of ADR 0057).
        let mut honest = BTreeMap::<SessionId, lash_core::NodeId>::new();
        for node_id in &session.path {
            let owner = &self
                .model
                .nodes
                .get(node_id)
                .ok_or_else(|| format!("path node `{node_id}` is not modeled"))?
                .owner_session_id;
            if *owner != session_id {
                honest.insert(owner.clone(), node_id.clone());
            }
        }
        let on_path = session.path.iter().cloned().collect::<BTreeSet<_>>();
        // A live node of an ancestor owner off this session's path: one the
        // owner appended after the fork.
        let candidates = self
            .reachable_nodes()
            .into_iter()
            .filter(|node_id| {
                !on_path.contains(node_id)
                    && self
                        .model
                        .nodes
                        .get(node_id)
                        .is_some_and(|node| honest.contains_key(&node.owner_session_id))
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Ok(());
        }
        let inflated = candidates[usize::from(selector) % candidates.len()].clone();
        let owner = self
            .model
            .nodes
            .get(&inflated)
            .map(|node| node.owner_session_id.clone())
            .ok_or_else(|| format!("candidate `{inflated}` is not modeled"))?;
        let restored = honest
            .get(&owner)
            .cloned()
            .ok_or_else(|| format!("owner `{owner}` has no honest ceiling"))?;
        let store = live.store.clone();
        self.factory
            .force_fork_lineage_for_testing(&session_id, &inflated)
            .await
            .map_err(|error| error.to_string())?;

        for node_id in self.model.nodes.keys() {
            let expected = on_path.contains(node_id);
            match crate::conformance::helpers::node_readable(&store, node_id).await {
                Ok(readable) if readable == expected => {}
                // A corrupt ceiling may deny a node the edges reach: two rows
                // then share a generation, and the page refuses as corrupt.
                Err(crate::StoreError::StoredDataCorrupt { .. }) if expected => {}
                other => {
                    return Err(format!(
                        "edge authority: with `{session_id}`'s ceiling on `{owner}` raised to `{inflated}`, reading `{node_id}` (on path: {expected}) gave {other:?}"
                    ));
                }
            }
            let active = self
                .factory
                .contains_active_ancestor(&session_id, node_id)
                .await
                .map_err(|error| error.to_string())?;
            if active != expected {
                return Err(format!(
                    "edge authority: with `{session_id}`'s ceiling on `{owner}` raised to `{inflated}`, `{node_id}` active={active}, on path={expected}"
                ));
            }
        }

        self.factory
            .force_fork_lineage_for_testing(&session_id, &restored)
            .await
            .map_err(|error| error.to_string())?;
        self.shape[RunShapeCounter::InflatedCeilings] += 1;
        Ok(())
    }

    async fn tombstone_vacuum(&mut self) -> Result<(), String> {
        for store in self.handles_by_physical_id.values() {
            store.vacuum().await.map_err(|error| error.to_string())?;
        }
        self.assert_reachability().await?;
        self.shape[RunShapeCounter::VacuumRuns] += 1;
        Ok(())
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn malformed(&mut self, slot: u8, shape: u8) -> Result<(), String> {
        let slot = slot % SESSION_COUNT;
        self.ensure_session(slot).await?;
        if self
            .model
            .sessions
            .get(&slot)
            .is_none_or(|session| session.path.is_empty())
        {
            Box::pin(self.append(slot, 1, 0)).await?;
        }
        assert_bounded_resident_rejection(shape % 4)?;
        self.shape[RunShapeCounter::BoundedTraversals] += 1;

        let before = self.session_snapshot(slot).await?;
        let operation_key = self.next_operation_id("malformed");
        let live = self.live.get(&slot).expect("malformed live session");
        let state = crate::conformance::helpers::load_window_state(
            live.store.store(),
            live.store.session_id(),
        )
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "malformed subject has no persisted state".to_string())?;
        let operation = crate::OperationId::turn(
            &state.session_id,
            crate::TurnId::fixture(operation_key),
            "malformed",
        );
        let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state);
        commit.turn_commit = crate::RuntimeTurnCommitStamp::new(operation.clone());
        commit.graph = malformed_graph_append(&state, &operation, shape % 4)?;
        let error = commit_runtime_state_for_property(live.store.store(), commit, "malformed")
            .await
            .expect_err("malformed session graph commit must be rejected");
        let typed = match shape % 4 {
            0 | 2 => matches!(error, crate::StoreError::NodeIdCollision { .. }),
            _ => matches!(error, crate::StoreError::InvalidGraphParent { .. }),
        };
        if !typed {
            return Err(format!(
                "malformed graph returned the wrong typed rejection: {error:?}"
            ));
        }
        let after = self.session_snapshot(slot).await?;
        if before != after {
            return Err("malformed graph rejection mutated durable state".to_string());
        }
        self.shape[RunShapeCounter::TypedRejections] += 1;
        Ok(())
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn stale_head_cas(&mut self, slot: u8) -> Result<(), String> {
        let slot = slot % SESSION_COUNT;
        self.ensure_session(slot).await?;
        if self
            .model
            .sessions
            .get(&slot)
            .is_none_or(|session| session.path.is_empty())
        {
            Box::pin(self.append(slot, 1, 0)).await?;
        }
        if self
            .model
            .sessions
            .get(&slot)
            .is_some_and(|session| session.head_revision == 0)
        {
            self.checkpoint_commit(slot).await?;
        }
        let before = self.session_snapshot(slot).await?;
        let operation_key = self.next_operation_id("stale-cas");
        let live = self.live.get(&slot).expect("stale-CAS live session");
        let state = crate::conformance::helpers::load_window_state(
            live.store.store(),
            live.store.session_id(),
        )
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "stale-CAS subject has no persisted state".to_string())?;
        let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state);
        commit.expected_head_revision = state.head_revision - 1;
        commit.turn_commit = crate::RuntimeTurnCommitStamp::new(crate::OperationId::turn(
            &state.session_id,
            crate::TurnId::fixture(operation_key),
            "stale-cas",
        ));
        let error = commit_runtime_state_for_property(live.store.store(), commit, "stale-cas")
            .await
            .expect_err("stale head revision must be rejected");
        if !matches!(error, crate::StoreError::HeadRevisionConflict { .. }) {
            return Err(format!(
                "head-revision CAS returned the wrong rejection: {error:?}"
            ));
        }
        if before != self.session_snapshot(slot).await? {
            return Err("head-revision CAS rejection mutated durable state".to_string());
        }
        self.shape[RunShapeCounter::TypedRejections] += 1;
        Ok(())
    }

    /// Selector 0 names the head, 1 the newest retained revision whose leaf
    /// is not the head's, 2 the oldest retained revision, and 3 a revision
    /// the session does not retain: a collected one when there is one, else
    /// one past the head.
    fn selected_revision(&self, slot: u8, selector: u8) -> Option<SelectedRevision> {
        let session = self.model.sessions.get(&(slot % SESSION_COUNT))?;
        let retained = |(revision, leaf): (&u64, &Option<lash_core::NodeId>)| {
            SelectedRevision::Retained(*revision, leaf.clone())
        };
        match selector % 4 {
            0 => session
                .revisions
                .get_key_value(&session.head_revision)
                .map(retained),
            1 => session
                .revisions
                .iter()
                .rev()
                .find(|(_, leaf)| leaf.as_ref() != session.path.last())
                .map(retained),
            2 => session.revisions.iter().next().map(retained),
            _ => Some(
                (0..session.head_revision)
                    .rev()
                    .find(|revision| !session.revisions.contains_key(revision))
                    .map_or(
                        SelectedRevision::Pending(session.head_revision + 7),
                        SelectedRevision::Pruned,
                    ),
            ),
        }
    }

    /// The path from the root to `leaf`, by the modeled parent edges.
    fn path_to(&self, leaf: Option<&lash_core::NodeId>) -> Vec<lash_core::NodeId> {
        let mut path = Vec::new();
        let mut cursor = leaf.cloned();
        while let Some(node_id) = cursor {
            cursor = self
                .model
                .nodes
                .get(&node_id)
                .and_then(|node| node.parent_node_id.clone());
            path.push(node_id);
        }
        path.reverse();
        path
    }

    async fn read_live(&self, slot: u8) -> Result<crate::store::SessionWindowRead, String> {
        self.live
            .get(&(slot % SESSION_COUNT))
            .ok_or_else(|| format!("session slot {slot} is not live"))?
            .store
            .load_session_window(crate::store::WindowSelector::Current)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("session slot {slot} has no persisted head"))
    }

    async fn session_snapshot(&self, slot: u8) -> Result<serde_json::Value, String> {
        let live = self
            .live
            .get(&(slot % SESSION_COUNT))
            .ok_or_else(|| format!("session slot {slot} is not live"))?;
        persisted_projection(&live.store).await
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    fn record_read(
        &mut self,
        slot: u8,
        read: &crate::store::SessionWindowRead,
    ) -> Result<(), String> {
        let path = graph_path_ids(&read.window)?;
        for node_id in &path {
            let node = read
                .window
                .find_node(node_id)
                .ok_or_else(|| format!("active node `{node_id}` does not resolve"))?;
            match self.model.nodes.get(node_id) {
                Some(expected)
                    if expected.parent_node_id != node.parent_node_id
                        || expected.owner_session_id
                            != self
                                .model
                                .sessions
                                .get(&(slot % SESSION_COUNT))
                                .expect("recorded session")
                                .physical_id =>
                {
                    // Shared fork prefixes retain their original owner, so only compare the
                    // immutable parent here. Ownership is installed on first observation.
                    if expected.parent_node_id != node.parent_node_id {
                        return Err(format!(
                            "append-only history: node `{node_id}` changed parent"
                        ));
                    }
                }
                Some(_) => {}
                None => {
                    self.model.nodes.insert(
                        node_id.clone(),
                        ModelNode {
                            parent_node_id: node.parent_node_id.clone(),
                            owner_session_id: read.session_id.clone(),
                        },
                    );
                }
            }
        }
        let model = self
            .model
            .sessions
            .get_mut(&(slot % SESSION_COUNT))
            .expect("recorded session");
        model.path = path;
        model.head_revision = read.head_revision;
        model
            .revisions
            .insert(read.head_revision, model.path.last().cloned());
        Ok(())
    }

    async fn assert_model_agreement(&self) -> Result<(), String> {
        for (slot, expected) in &self.model.sessions {
            let live = self
                .live
                .get(slot)
                .ok_or_else(|| format!("modeled session slot {slot} has no live handle"))?;
            // Admission writes the created head (FIG-4553): every live
            // session answers a window, and a session nothing committed yet
            // answers its created head at revision 0 with an empty path.
            let read = live
                .store
                .load_session_window(crate::store::WindowSelector::Current)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| {
                    format!(
                        "admitted session `{}` lost its created head",
                        expected.physical_id
                    )
                })?;
            if read.session_id != expected.physical_id {
                return Err(format!(
                    "session identity differs: actual={}, expected={}",
                    read.session_id, expected.physical_id
                ));
            }
            if read.head_revision != expected.head_revision {
                return Err(format!(
                    "head-revision CAS: session `{}` has revision {}, expected {}",
                    expected.physical_id, read.head_revision, expected.head_revision
                ));
            }
            let actual_path = graph_path_ids(&read.window)?;
            if actual_path != expected.path {
                return Err(format!(
                    "active-path integrity: session `{}` actual={actual_path:?}, expected={:?}",
                    expected.physical_id, expected.path
                ));
            }
            for (index, node_id) in actual_path.iter().enumerate() {
                let node = read
                    .window
                    .find_node(node_id)
                    .ok_or_else(|| format!("active leaf/path node `{node_id}` does not resolve"))?;
                let expected_parent = index
                    .checked_sub(1)
                    .and_then(|parent| actual_path.get(parent));
                if node.parent_node_id.as_ref() != expected_parent {
                    return Err(format!(
                        "active-path integrity: node `{node_id}` parent {:?}, expected {expected_parent:?}",
                        node.parent_node_id
                    ));
                }
                if self
                    .model
                    .nodes
                    .get(node_id)
                    .is_some_and(|modeled| modeled.parent_node_id != node.parent_node_id)
                {
                    return Err(format!(
                        "append-only history: node `{node_id}` changed parent"
                    ));
                }
            }
        }
        self.assert_fork_runs().await
    }

    /// Every live session lists exactly the revisions the model retains, with
    /// the leaf each published, its head flagged and its pins named.
    async fn assert_fork_runs(&self) -> Result<(), String> {
        for (slot, expected) in &self.model.sessions {
            let live = self
                .live
                .get(slot)
                .ok_or_else(|| format!("modeled session slot {slot} has no live handle"))?;
            let actual = live
                .store
                .revisions()
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(|revision| {
                    (
                        revision.head_revision,
                        (
                            revision.leaf_node_id,
                            revision.head,
                            !revision.pinned_by.is_empty(),
                        ),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            let modeled = expected
                .revisions
                .iter()
                .map(|(revision, leaf)| {
                    (
                        *revision,
                        (
                            leaf.clone(),
                            *revision == expected.head_revision,
                            expected.pins.contains(revision),
                        ),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            if actual != modeled {
                return Err(format!(
                    "retained revisions of `{}` differ: actual={actual:?}, expected={modeled:?}",
                    expected.physical_id
                ));
            }
        }
        Ok(())
    }

    async fn assert_reachability(&mut self) -> Result<(), String> {
        self.assert_fork_runs().await?;
        let reachable = self.reachable_nodes();
        for node_id in &reachable {
            if !self.model.nodes.contains_key(node_id) {
                return Err(format!(
                    "reachability model lost reachable node `{node_id}`"
                ));
            }
            let mut loadable = false;
            for handle in self.handles_by_physical_id.values() {
                if handle_reads(handle, node_id).await? {
                    loadable = true;
                    break;
                }
            }
            if !loadable && let Some((slot, revision, leaf)) = self.retaining_revision_for(node_id)
            {
                self.probe_retained_node(slot, revision, leaf, node_id)
                    .await?;
                loadable = true;
            }
            if !loadable {
                return Err(format!(
                    "reachability equals retention: reachable node `{node_id}` is not loadable"
                ));
            }
        }
        for node_id in self.model.nodes.keys() {
            if reachable.contains(node_id) {
                continue;
            }
            for handle in self.handles_by_physical_id.values() {
                if handle_reads(handle, node_id).await? {
                    return Err(format!(
                        "reachability equals retention: unreachable node `{node_id}` remains loadable"
                    ));
                }
            }
        }
        Ok(())
    }

    /// A retained revision whose history holds `node_id`.
    fn retaining_revision_for(
        &self,
        node_id: &lash_core::NodeId,
    ) -> Option<(u8, u64, Option<lash_core::NodeId>)> {
        self.model.sessions.iter().find_map(|(slot, session)| {
            session.revisions.iter().find_map(|(revision, leaf)| {
                self.path_to(leaf.as_ref())
                    .contains(node_id)
                    .then(|| (*slot, *revision, leaf.clone()))
            })
        })
    }

    async fn probe_retained_node(
        &mut self,
        slot: u8,
        revision: u64,
        leaf: Option<lash_core::NodeId>,
        expected_node_id: &str,
    ) -> Result<(), String> {
        let probe_id = self.next_operation_id("retention-probe");
        let Some((probe, _)) = self
            .fork_revision(
                slot,
                &SelectedRevision::Retained(revision, leaf.clone()),
                &probe_id,
            )
            .await
            .map_err(|error| {
                format!(
                    "reachability equals retention: retained revision {revision} was not forkable: {error}"
                )
            })?
        else {
            return Err(format!(
                "reachability equals retention: retained revision {revision} was refused"
            ));
        };
        let read = probe
            .store
            .load_session_window(crate::store::WindowSelector::Current)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                format!("reachability equals retention: revision {revision} produced no probe head")
            })?;
        if read.window.leaf_node_id != leaf
            || !crate::conformance::helpers::node_readable(&probe.store, expected_node_id)
                .await
                .map_err(|error| error.to_string())?
        {
            return Err(format!(
                "reachability equals retention: node `{expected_node_id}` retained by revision {revision} did not survive in its probe fork"
            ));
        }
        self.factory
            .delete_session(&SessionId::fixture(probe_id))
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// The nodes some retained revision of a live session still reaches.
    fn reachable_nodes(&self) -> BTreeSet<lash_core::NodeId> {
        let roots = self
            .model
            .sessions
            .values()
            .flat_map(|session| session.revisions.values().flatten().cloned());
        let mut reachable = BTreeSet::new();
        let mut pending = roots.collect::<Vec<_>>();
        while let Some(node_id) = pending.pop() {
            if !reachable.insert(node_id.clone()) {
                continue;
            }
            if let Some(parent) = self
                .model
                .nodes
                .get(&node_id)
                .and_then(|node| node.parent_node_id.clone())
            {
                pending.push(parent);
            }
        }
        reachable
    }
}

async fn property_runtime(
    store: &Arc<dyn crate::RuntimeStore>,
    request: &crate::SessionStoreCreateRequest,
) -> Result<crate::LashRuntime, String> {
    let state = crate::conformance::helpers::load_window_state(store, &request.session_id)
        .await
        .map_err(|error| error.to_string())?
        .unwrap_or_else(|| crate::RuntimeSessionState {
            session_id: request.session_id.clone(),
            policy: request.config.session_policy(),
            ..crate::RuntimeSessionState::new(request.config.session_policy())
        });
    let host = crate::PluginHost::new(crate::testing::test_standard_protocol_factories());
    let plugins = match state.plugin_state() {
        Some(snapshot) => host.build_session(PluginSessionRequest::rematerialization(
            request.session_id.clone(),
            snapshot,
            crate::plugin::SessionAuthorityContext {
                plugin_config: state.admitted_plugin_config(),
                ..Default::default()
            },
        )),
        None => host.build_session(PluginSessionRequest::creation(
            request.session_id.clone(),
            Default::default(),
        )),
    }
    .map_err(|error| error.to_string())?;
    let runtime_host = crate::EmbeddedRuntimeHost::new(crate::StoreLawBackend::new().host_config(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    ));
    let runtime_services = crate::PersistentRuntimeServices::new(
        plugins,
        crate::conformance::helpers::session_view(store, request.session_id.clone()),
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    crate::LashRuntime::from_persistent_embedded_state(
        request.config.session_policy(),
        runtime_host,
        runtime_services,
        state,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .map_err(|error| error.to_string())
}

async fn commit_runtime_state_for_property(
    store: &Arc<dyn crate::RuntimeStore>,
    commit: crate::RuntimeCommit,
    owner_suffix: &str,
) -> Result<crate::RuntimeCommitReceipt, crate::StoreError> {
    let session_id = commit.session_id.clone();
    let owner = crate::LeaseOwnerIdentity::opaque(
        format!("session-graph-property-{owner_suffix}"),
        format!("session-graph-property-{owner_suffix}-incarnation"),
    );
    let lease = store
        .seal_shift_epoch_for_test(
            &session_id,
            &owner,
            "commit-runtime-state-for-property-executor",
            60_000,
        )
        .await?
        .acquired()
        .ok_or(crate::StoreError::Contended)?;
    let result = store.commit_runtime_state(commit).await;
    if result.is_err() {
        store.supersede_shift_epoch_for_test(&lease).await?;
    }
    result
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn malformed_graph_append(
    state: &crate::RuntimeSessionState,
    operation: &crate::OperationId,
    shape: u8,
) -> Result<crate::GraphAppend, String> {
    let old_leaf = state.session_graph.leaf_node_id.clone();
    let plugin_node =
        |ordinal: u64, parent_node_id: Option<lash_core::NodeId>| crate::SessionNodeRecord {
            node_id: crate::store::derive_history_node_id(&state.session_id, operation, ordinal)
                .expect("property operation id is valid"),
            parent_node_id,
            timestamp: "1970-01-01T00:00:00.000000000Z"
                .parse()
                .expect("canonical node timestamp"),
            payload: crate::SessionNodePayload::Plugin {
                plugin_type: "session-graph-malformed".to_string(),
                body: crate::session_graph::SharedJsonValue::new(
                    serde_json::json!({"shape": shape}),
                ),
            },
        };
    Ok(match shape {
        0 => {
            let frame_key = crate::FrameKey::from_caller_material(&format!(
                "malformed-duplicate-{}",
                state.head_revision
            ))
            .expect("non-empty frame material");
            let node_id = crate::frame_node_id(&state.session_id, frame_key.as_str()).into_inner();
            let frame = |parent_node_id: Option<lash_core::NodeId>| crate::SessionNodeRecord {
                node_id: lash_core::NodeId::fixture(node_id.clone()),
                parent_node_id,
                timestamp: "1970-01-01T00:00:00.000000000Z"
                    .parse()
                    .expect("canonical node timestamp"),
                payload: crate::SessionNodePayload::FrameOpen {
                    frame_key: frame_key.clone(),
                    reason: crate::AgentFrameReason::initial(),
                    assignment: crate::AgentFrameAssignment::unconfigured(state.policy.clone()),
                },
            };
            crate::GraphAppend::Extend {
                nodes: vec![
                    frame(old_leaf),
                    frame(Some(lash_core::NodeId::fixture(node_id.clone()))),
                ],
            }
        }
        1 => {
            let node = plugin_node(0, Some("missing-parent".into()));
            crate::GraphAppend::Extend { nodes: vec![node] }
        }
        2 => {
            let resident_frame_key = state
                .session_graph
                .nodes
                .iter()
                .find_map(|node| match &node.payload {
                    crate::SessionNodePayload::FrameOpen { frame_key, .. } => {
                        Some(frame_key.clone())
                    }
                    _ => None,
                })
                .ok_or_else(|| "malformed subject has no resident frame".to_string())?;
            let node_id =
                crate::frame_node_id(&state.session_id, resident_frame_key.as_str()).into_inner();
            crate::GraphAppend::Extend {
                nodes: vec![crate::SessionNodeRecord {
                    node_id: lash_core::NodeId::fixture(node_id.clone()),
                    parent_node_id: old_leaf.clone(),
                    timestamp: "1970-01-01T00:00:00.000000000Z"
                        .parse()
                        .expect("canonical node timestamp"),
                    payload: crate::SessionNodePayload::FrameOpen {
                        frame_key: resident_frame_key,
                        reason: crate::AgentFrameReason::initial(),
                        assignment: crate::AgentFrameAssignment::unconfigured(state.policy.clone()),
                    },
                }],
            }
        }
        _ => {
            let frame_key = crate::FrameKey::from_caller_material(&format!(
                "malformed-cycle-{}",
                state.head_revision
            ))
            .expect("non-empty frame material");
            let node_id = crate::frame_node_id(&state.session_id, frame_key.as_str()).into_inner();
            crate::GraphAppend::Extend {
                nodes: vec![crate::SessionNodeRecord {
                    node_id: lash_core::NodeId::fixture(node_id.clone()),
                    parent_node_id: Some(lash_core::NodeId::fixture(node_id.clone())),
                    timestamp: "1970-01-01T00:00:00.000000000Z"
                        .parse()
                        .expect("canonical node timestamp"),
                    payload: crate::SessionNodePayload::FrameOpen {
                        frame_key,
                        reason: crate::AgentFrameReason::initial(),
                        assignment: crate::AgentFrameAssignment::unconfigured(state.policy.clone()),
                    },
                }],
            }
        }
    })
}

fn assert_bounded_resident_rejection(shape: u8) -> Result<(), String> {
    let graph = malformed_resident_graph(shape);
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = graph.validate_resident_integrity();
        let _ = send.send(result);
    });
    let result = receive.recv_timeout(TRAVERSAL_WATCHDOG).map_err(|_| {
        format!(
            "bounded traversal: malformed resident shape {shape} exceeded {:?}",
            TRAVERSAL_WATCHDOG
        )
    })?;
    let typed = match shape {
        0 => matches!(result, Err(crate::StoreError::NodeIdCollision { .. })),
        1 | 3 => matches!(result, Err(crate::StoreError::InvalidGraphParent { .. })),
        _ => matches!(result, Err(crate::StoreError::InvalidGraphLeaf { .. })),
    };
    if !typed {
        return Err(format!(
            "bounded traversal: malformed resident shape {shape} was not typed: {result:?}"
        ));
    }
    Ok(())
}

fn malformed_resident_graph(shape: u8) -> crate::SessionGraph {
    let node = |id: &str, parent: Option<&str>| crate::SessionNodeRecord {
        node_id: lash_core::NodeId::fixture(id.to_string()),
        parent_node_id: parent.map(lash_core::NodeId::fixture),
        timestamp: "1970-01-01T00:00:00.000000000Z"
            .parse()
            .unwrap_or_else(|error| panic!("invalid fixture timestamp: {error}")),
        payload: crate::SessionNodePayload::Plugin {
            plugin_type: "session-graph-bounded".to_string(),
            body: crate::session_graph::SharedJsonValue::new(serde_json::json!({"id": id})),
        },
    };
    match shape {
        0 => crate::SessionGraph::from_unchecked_nodes_for_testing(
            vec![node("duplicate", None), node("duplicate", None)],
            Some("duplicate".into()),
        ),
        1 => crate::SessionGraph::from_unchecked_nodes_for_testing(
            vec![node("dangling", Some("missing"))],
            Some("dangling".into()),
        ),
        2 => crate::SessionGraph::from_unchecked_nodes_for_testing(
            vec![node("present", None)],
            Some("missing-leaf".into()),
        ),
        _ => crate::SessionGraph::from_unchecked_nodes_for_testing(
            vec![
                node("cycle-a", Some("cycle-b")),
                node("cycle-b", Some("cycle-a")),
            ],
            Some("cycle-b".into()),
        ),
    }
}

fn graph_path_ids(graph: &crate::SessionGraph) -> Result<Vec<lash_core::NodeId>, String> {
    graph
        .validate_resident_integrity()
        .map_err(|error| error.to_string())?;
    Ok(graph
        .active_path_nodes()
        .into_iter()
        .map(|node| node.node_id.clone())
        .collect())
}

/// Whether `handle`'s session reads `node_id`. A handle whose session a
/// rewind deleted reads nothing.
async fn handle_reads(handle: &crate::store::SessionStore, node_id: &str) -> Result<bool, String> {
    match crate::conformance::helpers::node_readable(handle, node_id).await {
        Ok(readable) => Ok(readable),
        Err(crate::StoreError::SessionDeleted { .. }) => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

async fn persisted_projection(
    store: &crate::store::SessionStore,
) -> Result<serde_json::Value, String> {
    let read = store
        .load_session_window(crate::store::WindowSelector::Current)
        .await
        .map_err(|error| error.to_string())?;
    Ok(read.map_or(serde_json::Value::Null, |read| {
        serde_json::json!({
            "session_id": read.session_id,
            "head_revision": read.head_revision,
            "config": read.config,
            "current_frame_node_id": read.current_frame_node_id,
            "window": read.window,
            "checkpoint_ref": read.checkpoint_ref,
            "checkpoint": read.checkpoint,
        })
    }))
}

async fn assert_dedicated_laws<F, Fut>(make: &F, seed: u64) -> Result<(), TestCaseError>
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = Arc<dyn crate::store::ConformanceDeployment>>,
{
    assert_on_fresh_factory(make, seed, |factory| async move {
        let operations = generated_prefix();
        Box::pin(replay_case(seed, factory, &operations))
            .await
            .map(|_| ())
    })
    .await?;
    assert_on_fresh_factory(make, seed.wrapping_add(1), |factory| async move {
        let operations = vec![
            SessionGraphContractOp::Append {
                session: 0,
                node_count: 2,
                requirement: 0,
            },
            SessionGraphContractOp::CheckpointCommit { session: 0 },
            SessionGraphContractOp::ColdReload { session: 0 },
            SessionGraphContractOp::ReachabilitySweep,
        ];
        Box::pin(replay_case(seed.wrapping_add(1), factory, &operations))
            .await
            .map(|_| ())
    })
    .await?;
    // FIG-1174: the rewound session's head stays rewindable after the first
    // rewind replaces and deletes the session it was forked from.
    assert_on_fresh_factory(make, seed.wrapping_add(2), |factory| async move {
        let operations = vec![
            SessionGraphContractOp::Append {
                session: 0,
                node_count: 1,
                requirement: 0,
            },
            SessionGraphContractOp::TruncateRewind {
                session: 0,
                revision: 0,
            },
            SessionGraphContractOp::TruncateRewind {
                session: 0,
                revision: 0,
            },
        ];
        Box::pin(replay_case(seed.wrapping_add(2), factory, &operations))
            .await
            .map(|_| ())
    })
    .await
}

async fn assert_on_fresh_factory<F, Fut, Law, LawFut>(
    make: &F,
    seed: u64,
    law: Law,
) -> Result<(), TestCaseError>
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = Arc<dyn crate::store::ConformanceDeployment>>,
    Law: FnOnce(Arc<dyn crate::store::ConformanceDeployment>) -> LawFut,
    LawFut: Future<Output = Result<(), TestCaseError>>,
{
    law(make(seed).await).await
}

fn counterexample_path(backend: &str) -> PathBuf {
    let root = std::env::var_os("LASH_SESSION_GRAPH_COUNTEREXAMPLE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("LASH_CONFIDENCE_OUT_DIR")
                .map(PathBuf::from)
                .map(|path| path.join("session-graph-counterexamples"))
        })
        .or_else(|| {
            std::env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .map(|path| path.join("session-graph-counterexamples"))
        })
        .unwrap_or_else(|| std::env::temp_dir().join("lash-session-graph-counterexamples"));
    root.join(format!("{backend}.txt"))
}

fn persist_counterexample(backend: &str, runner_seed: u64, error: &TestError<GeneratedCase>) {
    let path = counterexample_path(backend);
    if let Some(parent) = path.parent()
        && let Err(write_error) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "could not create session-graph counterexample directory {}: {write_error}",
            parent.display()
        );
        return;
    }
    let (case_seed, operations) = match error {
        TestError::Fail(_, case) => (Some(case.seed), Some(&case.operations)),
        TestError::Abort(_) => (None, None),
    };
    let body = format!(
        "backend: {backend}\nproptest_runner_seed: {runner_seed}\ncase_seed: {case_seed:?}\nminimal_operations: {operations:#?}\nfailure: {error}\n"
    );
    match std::fs::write(&path, body) {
        Ok(()) => eprintln!(
            "persisted minimized session-graph counterexample to {}",
            path.display()
        ),
        Err(write_error) => eprintln!(
            "could not persist session-graph counterexample to {}: {write_error}",
            path.display()
        ),
    }
}
