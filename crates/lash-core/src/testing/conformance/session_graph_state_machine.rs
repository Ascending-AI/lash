//! Model-based property laws for the durable session graph.
//!
//! The operation language and reference model live in `lash-core` so every
//! backend executes the same cases. Backend tests provide only a fresh
//! [`SessionStoreFactory`](crate::SessionStoreFactory) for each case.

use crate::facade_support::SessionGraphFacadeOps;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestError, TestRunner};

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
    Fork {
        source: u8,
        target: u8,
        node: u8,
    },
    Pin {
        session: u8,
        node: u8,
    },
    Unpin {
        session: u8,
        node: u8,
    },
    Delete {
        session: u8,
    },
    TruncateRewind {
        session: u8,
        node: u8,
    },
    ReachabilitySweep,
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
    parent_node_id: Option<String>,
    owner_session_id: String,
}

#[derive(Clone, Debug)]
struct ModelSession {
    physical_id: String,
    path: Vec<String>,
    head_revision: u64,
}

#[derive(Clone, Debug, Default)]
struct ReferenceModel {
    sessions: BTreeMap<u8, ModelSession>,
    nodes: BTreeMap<String, ModelNode>,
    pins: BTreeSet<String>,
    next_session_generation: u64,
    next_operation: u64,
}

struct LiveSession {
    request: crate::SessionStoreCreateRequest,
    store: Arc<dyn crate::RuntimePersistence>,
}

struct SessionGraphScenario {
    seed: u64,
    factory: Arc<dyn crate::SessionStoreFactory>,
    live: BTreeMap<u8, LiveSession>,
    handles_by_physical_id: BTreeMap<String, Arc<dyn crate::RuntimePersistence>>,
    model: ReferenceModel,
    shape: RunShape,
}

#[derive(Clone, Copy, Debug, Default)]
struct RunShape {
    appends_committed: u64,
    ancestor_appends_committed: u64,
    forks_committed: u64,
    rewinds_committed: u64,
    pins_committed: u64,
    unpins_committed: u64,
    deletes_committed: u64,
    checkpoint_commits: u64,
    cold_reloads: u64,
    reachability_sweeps: u64,
    vacuum_runs: u64,
    typed_rejections: u64,
    bounded_traversals: u64,
}

#[derive(Debug, Default)]
struct RunShapeTotals {
    appends_committed: AtomicU64,
    ancestor_appends_committed: AtomicU64,
    forks_committed: AtomicU64,
    rewinds_committed: AtomicU64,
    pins_committed: AtomicU64,
    unpins_committed: AtomicU64,
    deletes_committed: AtomicU64,
    checkpoint_commits: AtomicU64,
    cold_reloads: AtomicU64,
    reachability_sweeps: AtomicU64,
    vacuum_runs: AtomicU64,
    typed_rejections: AtomicU64,
    bounded_traversals: AtomicU64,
}

impl RunShapeTotals {
    fn add(&self, shape: RunShape) {
        self.appends_committed
            .fetch_add(shape.appends_committed, Ordering::Relaxed);
        self.ancestor_appends_committed
            .fetch_add(shape.ancestor_appends_committed, Ordering::Relaxed);
        self.forks_committed
            .fetch_add(shape.forks_committed, Ordering::Relaxed);
        self.rewinds_committed
            .fetch_add(shape.rewinds_committed, Ordering::Relaxed);
        self.pins_committed
            .fetch_add(shape.pins_committed, Ordering::Relaxed);
        self.unpins_committed
            .fetch_add(shape.unpins_committed, Ordering::Relaxed);
        self.deletes_committed
            .fetch_add(shape.deletes_committed, Ordering::Relaxed);
        self.checkpoint_commits
            .fetch_add(shape.checkpoint_commits, Ordering::Relaxed);
        self.cold_reloads
            .fetch_add(shape.cold_reloads, Ordering::Relaxed);
        self.reachability_sweeps
            .fetch_add(shape.reachability_sweeps, Ordering::Relaxed);
        self.vacuum_runs
            .fetch_add(shape.vacuum_runs, Ordering::Relaxed);
        self.typed_rejections
            .fetch_add(shape.typed_rejections, Ordering::Relaxed);
        self.bounded_traversals
            .fetch_add(shape.bounded_traversals, Ordering::Relaxed);
    }
}

/// Run generated session-graph laws with shrinking and counterexample capture.
pub async fn session_graph_state_machine<F, Fut>(backend: &'static str, make: F)
where
    F: Fn(u64) -> Fut + Send + Sync + Clone + 'static,
    Fut: Future<Output = Arc<dyn crate::SessionStoreFactory>> + Send + 'static,
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
                let shape = replay_case(case.seed, factory, &case.operations).await?;
                prop_assert!(
                    shape.ancestor_appends_committed > 0,
                    "generated alphabet starvation: no ancestor-based append committed"
                );
                prop_assert!(
                    shape.forks_committed > 0 && shape.rewinds_committed > 0,
                    "generated alphabet starvation: fork/rewind lifecycle was not reached"
                );
                prop_assert!(
                    shape.typed_rejections >= 5,
                    "generated alphabet starvation: malformed and stale paths were not rejected"
                );
                prop_assert!(
                    shape.bounded_traversals >= 4,
                    "generated alphabet starvation: malformed traversal shapes were not exercised"
                );
                runner_totals.add(shape);
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
        "session-graph run shape ({backend}, cases={cases}): appends_committed={} ancestor_appends_committed={} forks_committed={} rewinds_committed={} pins_committed={} unpins_committed={} deletes_committed={} checkpoint_commits={} cold_reloads={} reachability_sweeps={} vacuum_runs={} typed_rejections={} bounded_traversals={}",
        totals.appends_committed.load(Ordering::Relaxed),
        totals.ancestor_appends_committed.load(Ordering::Relaxed),
        totals.forks_committed.load(Ordering::Relaxed),
        totals.rewinds_committed.load(Ordering::Relaxed),
        totals.pins_committed.load(Ordering::Relaxed),
        totals.unpins_committed.load(Ordering::Relaxed),
        totals.deletes_committed.load(Ordering::Relaxed),
        totals.checkpoint_commits.load(Ordering::Relaxed),
        totals.cold_reloads.load(Ordering::Relaxed),
        totals.reachability_sweeps.load(Ordering::Relaxed),
        totals.vacuum_runs.load(Ordering::Relaxed),
        totals.typed_rejections.load(Ordering::Relaxed),
        totals.bounded_traversals.load(Ordering::Relaxed),
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
            node: 0,
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
            node: 1,
        },
        SessionGraphContractOp::Append {
            session: 1,
            node_count: 1,
            requirement: 2,
        },
        SessionGraphContractOp::Pin {
            session: 1,
            node: 0,
        },
        SessionGraphContractOp::Append {
            session: 1,
            node_count: 1,
            requirement: 1,
        },
        SessionGraphContractOp::TruncateRewind {
            session: 1,
            node: 1,
        },
        SessionGraphContractOp::Unpin {
            session: 1,
            node: 0,
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
            |(source, target, node)| SessionGraphContractOp::Fork { source, target, node },
        ),
        2 => (0..SESSION_COUNT, 0_u8..4)
            .prop_map(|(session, node)| SessionGraphContractOp::Pin { session, node }),
        2 => (0..SESSION_COUNT, 0_u8..4)
            .prop_map(|(session, node)| SessionGraphContractOp::Unpin { session, node }),
        1 => (0..SESSION_COUNT).prop_map(|session| SessionGraphContractOp::Delete { session }),
        2 => (0..SESSION_COUNT, 0_u8..4).prop_map(|(session, node)| {
            SessionGraphContractOp::TruncateRewind { session, node }
        }),
        2 => Just(SessionGraphContractOp::ReachabilitySweep),
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
    factory: Arc<dyn crate::SessionStoreFactory>,
    operations: &[SessionGraphContractOp],
) -> Result<RunShape, TestCaseError> {
    let mut scenario = SessionGraphScenario::new(seed, factory);
    for (step, operation) in operations.iter().enumerate() {
        scenario.apply(operation).await.map_err(|reason| {
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
    fn new(seed: u64, factory: Arc<dyn crate::SessionStoreFactory>) -> Self {
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
            } => self.append(*session, *node_count, *requirement).await,
            SessionGraphContractOp::Fork {
                source,
                target,
                node,
            } => self.fork(*source, *target, *node).await,
            SessionGraphContractOp::Pin { session, node } => self.pin(*session, *node).await,
            SessionGraphContractOp::Unpin { session, node } => self.unpin(*session, *node).await,
            SessionGraphContractOp::Delete { session } => self.delete(*session).await,
            SessionGraphContractOp::TruncateRewind { session, node } => {
                self.truncate_rewind(*session, *node).await
            }
            SessionGraphContractOp::ReachabilitySweep => self.reachability_sweep().await,
            SessionGraphContractOp::TombstoneVacuum => self.tombstone_vacuum().await,
            SessionGraphContractOp::CheckpointCommit { session } => {
                self.checkpoint_commit(*session).await
            }
            SessionGraphContractOp::ColdReload { session } => self.cold_reload(*session).await,
            SessionGraphContractOp::Malformed { session, shape } => {
                self.malformed(*session, *shape).await
            }
            SessionGraphContractOp::StaleHeadCas { session } => self.stale_head_cas(*session).await,
        }
    }

    async fn ensure_session(&mut self, slot: u8) -> Result<(), String> {
        let slot = slot % SESSION_COUNT;
        if self.live.contains_key(&slot) {
            return Ok(());
        }
        let physical_id = self.next_session_id(slot);
        let request = session_store_request(
            &physical_id,
            "session-graph-property-model",
            crate::SessionRelation::Root,
        );
        let store = self
            .factory
            .create_store(&request)
            .await
            .map_err(|error| error.to_string())?;
        self.handles_by_physical_id
            .insert(physical_id.clone(), Arc::clone(&store));
        self.live.insert(slot, LiveSession { request, store });
        self.model.sessions.insert(
            slot,
            ModelSession {
                physical_id,
                path: Vec::new(),
                head_revision: 0,
            },
        );
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
            _ => Some(format!("missing-required-{}", self.model.next_operation)),
        };
        let operation_id = self.next_operation_id("append");
        let live = self.live.get(&slot).expect("ensured live session");
        let mut runtime = property_runtime(&live.store, &live.request).await?;
        let nodes = (0..usize::from(node_count.max(1)))
            .map(|ordinal| {
                crate::SessionAppendNode::plugin(
                    "session-graph-property",
                    serde_json::json!({"operation": operation_id, "ordinal": ordinal}),
                )
            })
            .collect();
        let result = runtime
            .append_session_nodes(crate::AppendSessionNodesRequest {
                operation_id,
                nodes,
                requires_ancestor_node_id: required.clone(),
            })
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
            self.shape.typed_rejections += 1;
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
        let actual_path = graph_path_ids(&read.graph)?;
        if !actual_path.starts_with(&old_path) {
            return Err(format!(
                "append-only history: prior path {old_path:?} is not a prefix of {actual_path:?}"
            ));
        }
        if !old_path.is_empty()
            && actual_path
                .get(old_path.len())
                .and_then(|node_id| read.graph.find_node(node_id))
                .and_then(|node| node.parent_node_id.as_ref())
                != old_path.last()
        {
            return Err(
                "first-parent-equals-leaf: append did not parent on the current leaf".to_string(),
            );
        }
        self.record_read(slot, &read)?;
        self.shape.appends_committed += 1;
        if required.is_some() && required.as_ref() != old_path.last() {
            self.shape.ancestor_appends_committed += 1;
        }
        Ok(())
    }

    async fn pin(&mut self, slot: u8, selector: u8) -> Result<(), String> {
        let slot = slot % SESSION_COUNT;
        let Some(node_id) = self.selected_node(slot, selector) else {
            return Ok(());
        };
        let is_retainable = self
            .model
            .sessions
            .get(&slot)
            .and_then(|session| session.path.last())
            == Some(&node_id)
            || self.model.pins.contains(&node_id);
        let result = self.factory.pin(&node_id).await;
        if is_retainable {
            let point = result.map_err(|error| error.to_string())?;
            if point.node_id != node_id || !point.pinned {
                return Err("pin/refcount: successful pin returned the wrong root".to_string());
            }
            self.model.pins.insert(node_id);
            self.shape.pins_committed += 1;
        } else {
            if !matches!(result, Err(crate::StoreError::ForkPointNotRetained { .. })) {
                return Err(format!(
                    "pin/refcount: unretained past node was accepted: {result:?}"
                ));
            }
            self.shape.typed_rejections += 1;
        }
        Ok(())
    }

    async fn unpin(&mut self, slot: u8, selector: u8) -> Result<(), String> {
        let slot = slot % SESSION_COUNT;
        let Some(node_id) = self.selected_node(slot, selector) else {
            return Ok(());
        };
        self.factory
            .unpin(&node_id)
            .await
            .map_err(|error| error.to_string())?;
        self.model.pins.remove(&node_id);
        self.shape.unpins_committed += 1;
        Ok(())
    }

    async fn fork(&mut self, source: u8, target: u8, selector: u8) -> Result<(), String> {
        let source = source % SESSION_COUNT;
        let target = target % SESSION_COUNT;
        let Some(node_id) = self.selected_node(source, selector) else {
            return Ok(());
        };
        if self.live.contains_key(&target) {
            return Ok(());
        }
        let retained = self.model.pins.contains(&node_id)
            || self
                .model
                .sessions
                .get(&source)
                .and_then(|session| session.path.last())
                == Some(&node_id);
        let physical_id = self.next_session_id(target);
        let source_session_id = self
            .model
            .sessions
            .get(&source)
            .expect("selected source is live")
            .physical_id
            .clone();
        let relation = crate::SessionRelation::Fork {
            source_session_id,
            source_node_id: node_id.clone(),
            observer_inheritance: crate::ObserverInheritance::default(),
            pending_observer_process_ids: Vec::new(),
        };
        let request = session_store_request(
            &physical_id,
            "session-graph-property-model",
            relation.clone(),
        );
        let result = self
            .factory
            .fork_at(&crate::ForkSessionRequest {
                session_id: physical_id.clone(),
                node_id: node_id.clone(),
                relation,
                policy: request.policy.clone(),
            })
            .await;
        if !retained {
            if !matches!(result, Err(crate::StoreError::ForkPointNotRetained { .. })) {
                return Err(format!(
                    "fork isolation: unretained node was forked: {result:?}"
                ));
            }
            self.shape.typed_rejections += 1;
            return Ok(());
        }
        result.map_err(|error| error.to_string())?;
        let store = self
            .factory
            .open_existing_store(&request)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "fork created no reopenable store".to_string())?;
        let source_path = self
            .model
            .sessions
            .get(&source)
            .expect("source model")
            .path
            .clone();
        let target_index = source_path
            .iter()
            .position(|candidate| candidate == &node_id)
            .ok_or_else(|| "fork target left the modeled source path".to_string())?;
        self.handles_by_physical_id
            .insert(physical_id.clone(), Arc::clone(&store));
        self.live.insert(target, LiveSession { request, store });
        self.model.sessions.insert(
            target,
            ModelSession {
                physical_id,
                path: source_path[..=target_index].to_vec(),
                head_revision: 0,
            },
        );
        self.shape.forks_committed += 1;
        Ok(())
    }

    async fn truncate_rewind(&mut self, slot: u8, selector: u8) -> Result<(), String> {
        let slot = slot % SESSION_COUNT;
        let Some(node_id) = self.selected_node(slot, selector) else {
            return Ok(());
        };
        if !self.model.pins.contains(&node_id) {
            let result = self.factory.pin(&node_id).await;
            let is_leaf = self
                .model
                .sessions
                .get(&slot)
                .and_then(|session| session.path.last())
                == Some(&node_id);
            if !is_leaf {
                if !matches!(result, Err(crate::StoreError::ForkPointNotRetained { .. })) {
                    return Err(format!(
                        "rewind: unretained ancestor pin was not typed: {result:?}"
                    ));
                }
                self.shape.typed_rejections += 1;
                return Ok(());
            }
            result.map_err(|error| error.to_string())?;
            self.model.pins.insert(node_id.clone());
            self.shape.pins_committed += 1;
        }

        let old = self.live.remove(&slot).expect("selected session is live");
        let old_model = self.model.sessions.remove(&slot).expect("selected model");
        let physical_id = self.next_session_id(slot);
        let relation = crate::SessionRelation::Fork {
            source_session_id: old_model.physical_id.clone(),
            source_node_id: node_id.clone(),
            observer_inheritance: crate::ObserverInheritance::default(),
            pending_observer_process_ids: Vec::new(),
        };
        let request = session_store_request(
            &physical_id,
            "session-graph-property-model",
            relation.clone(),
        );
        self.factory
            .fork_at(&crate::ForkSessionRequest {
                session_id: physical_id.clone(),
                node_id: node_id.clone(),
                relation,
                policy: request.policy.clone(),
            })
            .await
            .map_err(|error| error.to_string())?;
        let store = self
            .factory
            .open_existing_store(&request)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "rewind fork created no reopenable store".to_string())?;
        self.factory
            .delete_session(&old.request.session_id)
            .await
            .map_err(|error| error.to_string())?;
        let index = old_model
            .path
            .iter()
            .position(|candidate| candidate == &node_id)
            .expect("rewind node belongs to old path");
        self.handles_by_physical_id
            .insert(physical_id.clone(), Arc::clone(&store));
        self.live.insert(slot, LiveSession { request, store });
        self.model.sessions.insert(
            slot,
            ModelSession {
                physical_id,
                path: old_model.path[..=index].to_vec(),
                head_revision: 0,
            },
        );
        self.shape.rewinds_committed += 1;
        self.shape.deletes_committed += 1;
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
        self.shape.deletes_committed += 1;
        Ok(())
    }

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
        let mut state = crate::store::load_persisted_session_state(live.store.as_ref())
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "checkpoint subject has no persisted state".to_string())?;
        state.turn_index += 1;
        let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state, &[]);
        commit.turn_commit = crate::RuntimeTurnCommitStamp::new(crate::OperationId::turn(
            &state.session_id,
            operation,
            "checkpoint",
        ));
        commit_runtime_state_for_property(&live.store, commit, "checkpoint")
            .await
            .map_err(|error| error.to_string())?;
        self.model
            .sessions
            .get_mut(&slot)
            .expect("checkpoint model")
            .head_revision += 1;
        self.shape.checkpoint_commits += 1;
        Ok(())
    }

    async fn cold_reload(&mut self, slot: u8) -> Result<(), String> {
        let slot = slot % SESSION_COUNT;
        let Some(live) = self.live.get(&slot) else {
            return Ok(());
        };
        let before = persisted_projection(live.store.as_ref()).await?;
        let reopened = self
            .factory
            .open_existing_store(&live.request)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "cold reload lost a live session".to_string())?;
        let after = persisted_projection(reopened.as_ref()).await?;
        if before != after {
            return Err("cold-reload projection equality: reopened state differs".to_string());
        }
        self.handles_by_physical_id
            .insert(live.request.session_id.clone(), Arc::clone(&reopened));
        self.live.get_mut(&slot).expect("live slot").store = reopened;
        self.shape.cold_reloads += 1;
        Ok(())
    }

    async fn reachability_sweep(&mut self) -> Result<(), String> {
        if let Some(store) = self.live.values().next().map(|live| &live.store) {
            store
                .gc_unreachable()
                .await
                .map_err(|error| error.to_string())?;
        }
        self.assert_reachability().await?;
        self.shape.reachability_sweeps += 1;
        Ok(())
    }

    async fn tombstone_vacuum(&mut self) -> Result<(), String> {
        for store in self.handles_by_physical_id.values() {
            store.vacuum().await.map_err(|error| error.to_string())?;
        }
        self.assert_reachability().await?;
        self.shape.vacuum_runs += 1;
        Ok(())
    }

    async fn malformed(&mut self, slot: u8, shape: u8) -> Result<(), String> {
        let slot = slot % SESSION_COUNT;
        self.ensure_session(slot).await?;
        if self
            .model
            .sessions
            .get(&slot)
            .is_none_or(|session| session.path.is_empty())
        {
            self.append(slot, 1, 0).await?;
        }
        assert_bounded_resident_rejection(shape % 4)?;
        self.shape.bounded_traversals += 1;

        let before = self.session_snapshot(slot).await?;
        let operation_key = self.next_operation_id("malformed");
        let live = self.live.get(&slot).expect("malformed live session");
        let state = crate::store::load_persisted_session_state(live.store.as_ref())
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "malformed subject has no persisted state".to_string())?;
        let operation = crate::OperationId::turn(&state.session_id, operation_key, "malformed");
        let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state, &[]);
        commit.turn_commit = crate::RuntimeTurnCommitStamp::new(operation.clone());
        commit.graph = malformed_graph_append(&state, &operation, shape % 4)?;
        if matches!(shape % 4, 0 | 3) {
            commit.current_frame_node_id = commit.graph.leaf_node_id.clone();
        }
        let error = commit_runtime_state_for_property(&live.store, commit, "malformed")
            .await
            .expect_err("malformed session graph commit must be rejected");
        let typed = match shape % 4 {
            0 => matches!(error, crate::StoreError::NodeIdCollision { .. }),
            1 | 3 => matches!(error, crate::StoreError::InvalidGraphParent { .. }),
            _ => matches!(error, crate::StoreError::InvalidGraphLeaf { .. }),
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
        self.shape.typed_rejections += 1;
        Ok(())
    }

    async fn stale_head_cas(&mut self, slot: u8) -> Result<(), String> {
        let slot = slot % SESSION_COUNT;
        self.ensure_session(slot).await?;
        if self
            .model
            .sessions
            .get(&slot)
            .is_none_or(|session| session.path.is_empty())
        {
            self.append(slot, 1, 0).await?;
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
        let state = crate::store::load_persisted_session_state(live.store.as_ref())
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "stale-CAS subject has no persisted state".to_string())?;
        let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state, &[]);
        commit.expected_head_revision = state.head_revision - 1;
        commit.turn_commit = crate::RuntimeTurnCommitStamp::new(crate::OperationId::turn(
            &state.session_id,
            operation_key,
            "stale-cas",
        ));
        let error = commit_runtime_state_for_property(&live.store, commit, "stale-cas")
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
        self.shape.typed_rejections += 1;
        Ok(())
    }

    fn selected_node(&self, slot: u8, selector: u8) -> Option<String> {
        let path = &self.model.sessions.get(&(slot % SESSION_COUNT))?.path;
        match selector % 4 {
            0 => path.last().cloned(),
            1 => path.iter().rev().nth(1).cloned(),
            2 => path.first().cloned(),
            _ => Some(format!("missing-node-{slot}-{}", self.model.next_operation)),
        }
    }

    async fn read_live(&self, slot: u8) -> Result<crate::PersistedSessionRead, String> {
        self.live
            .get(&(slot % SESSION_COUNT))
            .ok_or_else(|| format!("session slot {slot} is not live"))?
            .store
            .load_session()
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("session slot {slot} has no persisted head"))
    }

    async fn session_snapshot(&self, slot: u8) -> Result<serde_json::Value, String> {
        let live = self
            .live
            .get(&(slot % SESSION_COUNT))
            .ok_or_else(|| format!("session slot {slot} is not live"))?;
        persisted_projection(live.store.as_ref()).await
    }

    fn record_read(&mut self, slot: u8, read: &crate::PersistedSessionRead) -> Result<(), String> {
        let path = graph_path_ids(&read.graph)?;
        for node_id in &path {
            let node = read
                .graph
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
        Ok(())
    }

    async fn assert_model_agreement(&self) -> Result<(), String> {
        for (slot, expected) in &self.model.sessions {
            let live = self
                .live
                .get(slot)
                .ok_or_else(|| format!("modeled session slot {slot} has no live handle"))?;
            let read = live
                .store
                .load_session()
                .await
                .map_err(|error| error.to_string())?;
            if expected.path.is_empty() {
                if read.is_some() {
                    return Err(format!(
                        "uncommitted session `{}` unexpectedly has a durable head",
                        expected.physical_id
                    ));
                }
                continue;
            }
            let read = read
                .ok_or_else(|| format!("modeled session `{}` disappeared", expected.physical_id))?;
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
            let actual_path = graph_path_ids(&read.graph)?;
            if actual_path != expected.path {
                return Err(format!(
                    "active-path integrity: session `{}` actual={actual_path:?}, expected={:?}",
                    expected.physical_id, expected.path
                ));
            }
            for (index, node_id) in actual_path.iter().enumerate() {
                let node = read
                    .graph
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
        self.assert_fork_roots().await
    }

    async fn assert_fork_roots(&self) -> Result<(), String> {
        let actual = self
            .factory
            .fork_points()
            .await
            .map_err(|error| error.to_string())?;
        let actual = actual
            .into_iter()
            .map(|point| (point.node_id, point.pinned))
            .collect::<BTreeMap<_, _>>();
        let mut expected = self
            .model
            .pins
            .iter()
            .cloned()
            .map(|node_id| (node_id, true))
            .collect::<BTreeMap<_, _>>();
        for session in self.model.sessions.values() {
            if let Some(leaf) = session.path.last() {
                expected.entry(leaf.clone()).or_insert(false);
            }
        }
        if actual != expected {
            return Err(format!(
                "fork isolation/refcount roots differ: actual={actual:?}, expected={expected:?}"
            ));
        }
        Ok(())
    }

    async fn assert_reachability(&mut self) -> Result<(), String> {
        self.assert_fork_roots().await?;
        let reachable = self.reachable_nodes();
        for node_id in &reachable {
            if !self.model.nodes.contains_key(node_id) {
                return Err(format!(
                    "reachability model lost reachable node `{node_id}`"
                ));
            }
            let mut loadable = false;
            for handle in self.handles_by_physical_id.values() {
                if handle
                    .load_node(node_id)
                    .await
                    .map_err(|error| error.to_string())?
                    .is_some()
                {
                    loadable = true;
                    break;
                }
            }
            if !loadable && let Some(pin) = self.retaining_pin_for(node_id) {
                self.probe_retained_node(&pin, node_id).await?;
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
                if handle
                    .load_node(node_id)
                    .await
                    .map_err(|error| error.to_string())?
                    .is_some()
                {
                    return Err(format!(
                        "reachability equals retention: unreachable node `{node_id}` remains loadable"
                    ));
                }
            }
        }
        Ok(())
    }

    fn retaining_pin_for(&self, node_id: &str) -> Option<String> {
        for pin in &self.model.pins {
            let mut cursor = Some(pin.as_str());
            let mut visited = BTreeSet::new();
            while let Some(candidate) = cursor {
                if candidate == node_id {
                    return Some(pin.clone());
                }
                if !visited.insert(candidate) {
                    break;
                }
                cursor = self
                    .model
                    .nodes
                    .get(candidate)
                    .and_then(|node| node.parent_node_id.as_deref());
            }
        }
        None
    }

    async fn probe_retained_node(
        &mut self,
        pinned_node_id: &str,
        expected_node_id: &str,
    ) -> Result<(), String> {
        let probe_id = self.next_operation_id("retention-probe");
        let request = session_store_request(
            &probe_id,
            "session-graph-property-retention-probe",
            crate::SessionRelation::Root,
        );
        self.factory
            .fork_at(&crate::ForkSessionRequest {
                session_id: probe_id.clone(),
                node_id: pinned_node_id.to_string(),
                relation: request.relation.clone(),
                policy: request.policy.clone(),
            })
            .await
            .map_err(|error| {
                format!(
                    "reachability equals retention: pinned node `{pinned_node_id}` was not forkable: {error}"
                )
            })?;
        let probe = self
            .factory
            .open_existing_store(&request)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                format!(
                    "reachability equals retention: pinned node `{pinned_node_id}` produced no probe store"
                )
            })?;
        let read = probe
            .load_session()
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                format!(
                    "reachability equals retention: pinned node `{pinned_node_id}` produced no probe head"
                )
            })?;
        if read.graph.leaf_node_id.as_deref() != Some(pinned_node_id)
            || probe
                .load_node(expected_node_id)
                .await
                .map_err(|error| error.to_string())?
                .is_none()
        {
            return Err(format!(
                "reachability equals retention: node `{expected_node_id}` retained by pin `{pinned_node_id}` did not survive in its probe fork"
            ));
        }
        self.factory
            .delete_session(&probe_id)
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn reachable_nodes(&self) -> BTreeSet<String> {
        let mut roots = self.model.pins.clone();
        roots.extend(
            self.model
                .sessions
                .values()
                .filter_map(|session| session.path.last().cloned()),
        );
        let mut reachable = BTreeSet::new();
        let mut pending = roots.into_iter().collect::<Vec<_>>();
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
    store: &Arc<dyn crate::RuntimePersistence>,
    request: &crate::SessionStoreCreateRequest,
) -> Result<crate::LashRuntime, String> {
    let state = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .map_err(|error| error.to_string())?
        .unwrap_or_else(|| crate::RuntimeSessionState {
            session_id: request.session_id.clone(),
            policy: request.policy.clone(),
            ..crate::RuntimeSessionState::new(request.policy.clone())
        });
    let plugins = crate::PluginHost::new(crate::testing::test_standard_protocol_factories())
        .build_session(request.session_id.clone(), state.plugin_snapshot())
        .map_err(|error| error.to_string())?;
    crate::LashRuntime::from_persistent_embedded_state(
        request.policy.clone(),
        crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        )),
        crate::PersistentRuntimeServices::new(plugins, Arc::clone(store)),
        state,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .map_err(|error| error.to_string())
}

async fn commit_runtime_state_for_property(
    store: &Arc<dyn crate::RuntimePersistence>,
    commit: crate::RuntimeCommit,
    owner_suffix: &str,
) -> Result<crate::RuntimeCommitReceipt, crate::StoreError> {
    let session_id = commit.session_id.clone();
    let owner = crate::LeaseOwnerIdentity::opaque(
        format!("session-graph-property-{owner_suffix}"),
        format!("session-graph-property-{owner_suffix}-incarnation"),
    );
    let lease = store
        .try_claim_session_execution_lease(
            &session_id,
            &owner,
            "commit-runtime-state-for-property-executor",
            60_000,
        )
        .await?
        .acquired()
        .ok_or(crate::StoreError::Contended)?;
    let result = store
        .commit_runtime_state(commit.releasing_session_execution_lease(lease.completion()))
        .await;
    if result.is_err() {
        store
            .release_session_execution_lease(&lease.completion())
            .await?;
    }
    result
}

fn malformed_graph_append(
    state: &crate::RuntimeSessionState,
    operation: &crate::OperationId,
    shape: u8,
) -> Result<crate::GraphAppend, String> {
    let old_leaf = state.session_graph.leaf_node_id.clone();
    let plugin_node = |ordinal: u64, parent_node_id: Option<String>| crate::SessionNodeRecord {
        node_id: crate::store::derive_history_node_id(&state.session_id, operation, ordinal)
            .expect("property operation id is valid"),
        parent_node_id,
        timestamp: "1970-01-01T00:00:00Z".to_string(),
        payload: crate::SessionNodePayload::Plugin {
            plugin_type: "session-graph-malformed".to_string(),
            body: crate::session_graph::SharedJsonValue::new(serde_json::json!({"shape": shape})),
        },
    };
    Ok(match shape {
        0 => {
            let frame_key = format!("malformed-duplicate-{}", state.head_revision);
            let node_id = crate::frame_node_id(&state.session_id, &frame_key);
            let frame = |parent_node_id: Option<String>| crate::SessionNodeRecord {
                node_id: node_id.clone(),
                parent_node_id,
                timestamp: "1970-01-01T00:00:00Z".to_string(),
                payload: crate::SessionNodePayload::FrameOpen {
                    frame_key: frame_key.clone(),
                    reason: crate::AgentFrameReason::initial(),
                    assignment: crate::AgentFrameAssignment::from_policy(state.policy.clone()),
                    protocol_turn_options: crate::ProtocolTurnOptions::default(),
                },
            };
            crate::GraphAppend {
                nodes: vec![frame(old_leaf), frame(Some(node_id.clone()))],
                leaf_node_id: Some(node_id),
            }
        }
        1 => {
            let node = plugin_node(0, Some("missing-parent".to_string()));
            crate::GraphAppend {
                leaf_node_id: Some(node.node_id.clone()),
                nodes: vec![node],
            }
        }
        2 => crate::GraphAppend {
            nodes: vec![plugin_node(0, old_leaf)],
            leaf_node_id: Some("missing-leaf".to_string()),
        },
        _ => {
            let frame_key = format!("malformed-cycle-{}", state.head_revision);
            let node_id = crate::frame_node_id(&state.session_id, &frame_key);
            crate::GraphAppend {
                nodes: vec![crate::SessionNodeRecord {
                    node_id: node_id.clone(),
                    parent_node_id: Some(node_id.clone()),
                    timestamp: "1970-01-01T00:00:00Z".to_string(),
                    payload: crate::SessionNodePayload::FrameOpen {
                        frame_key,
                        reason: crate::AgentFrameReason::initial(),
                        assignment: crate::AgentFrameAssignment::from_policy(state.policy.clone()),
                        protocol_turn_options: crate::ProtocolTurnOptions::default(),
                    },
                }],
                leaf_node_id: Some(node_id),
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
        node_id: id.to_string(),
        parent_node_id: parent.map(str::to_string),
        timestamp: "1970-01-01T00:00:00Z".to_string(),
        payload: crate::SessionNodePayload::Plugin {
            plugin_type: "session-graph-bounded".to_string(),
            body: crate::session_graph::SharedJsonValue::new(serde_json::json!({"id": id})),
        },
    };
    match shape {
        0 => crate::SessionGraph::from_unchecked_nodes_for_testing(
            vec![node("duplicate", None), node("duplicate", None)],
            Some("duplicate".to_string()),
        ),
        1 => crate::SessionGraph::from_unchecked_nodes_for_testing(
            vec![node("dangling", Some("missing"))],
            Some("dangling".to_string()),
        ),
        2 => crate::SessionGraph::from_unchecked_nodes_for_testing(
            vec![node("present", None)],
            Some("missing-leaf".to_string()),
        ),
        _ => crate::SessionGraph::from_unchecked_nodes_for_testing(
            vec![
                node("cycle-a", Some("cycle-b")),
                node("cycle-b", Some("cycle-a")),
            ],
            Some("cycle-b".to_string()),
        ),
    }
}

fn graph_path_ids(graph: &crate::SessionGraph) -> Result<Vec<String>, String> {
    graph
        .validate_resident_integrity()
        .map_err(|error| error.to_string())?;
    Ok(graph
        .active_path_nodes()
        .into_iter()
        .map(|node| node.node_id.clone())
        .collect())
}

async fn persisted_projection(
    store: &dyn crate::RuntimePersistence,
) -> Result<serde_json::Value, String> {
    let read = store
        .load_session()
        .await
        .map_err(|error| error.to_string())?;
    Ok(read.map_or(serde_json::Value::Null, |read| {
        serde_json::json!({
            "session_id": read.session_id,
            "head_revision": read.head_revision,
            "config": read.config,
            "current_frame_node_id": read.current_frame_node_id,
            "graph": read.graph,
            "checkpoint_ref": read.checkpoint_ref,
            "checkpoint": read.checkpoint,
            "token_ledger": read.token_ledger,
        })
    }))
}

async fn assert_dedicated_laws<F, Fut>(make: &F, seed: u64) -> Result<(), TestCaseError>
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = Arc<dyn crate::SessionStoreFactory>>,
{
    assert_on_fresh_factory(make, seed, |factory| async move {
        let operations = generated_prefix();
        replay_case(seed, factory, &operations).await.map(|_| ())
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
        replay_case(seed.wrapping_add(1), factory, &operations)
            .await
            .map(|_| ())
    })
    .await?;
    // FIG-1174: a retained leaf remains rewindable after the first rewind
    // replaces and deletes the session that pinned it.
    assert_on_fresh_factory(make, seed.wrapping_add(2), |factory| async move {
        let operations = vec![
            SessionGraphContractOp::Append {
                session: 0,
                node_count: 1,
                requirement: 0,
            },
            SessionGraphContractOp::TruncateRewind {
                session: 0,
                node: 0,
            },
            SessionGraphContractOp::TruncateRewind {
                session: 0,
                node: 0,
            },
        ];
        replay_case(seed.wrapping_add(2), factory, &operations)
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
    Fut: Future<Output = Arc<dyn crate::SessionStoreFactory>>,
    Law: FnOnce(Arc<dyn crate::SessionStoreFactory>) -> LawFut,
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
