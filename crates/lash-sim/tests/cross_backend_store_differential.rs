//! Store-level differential testing for `RuntimePersistence`.
//!
//! The generator is deliberately table-driven for its first landing. These
//! malformed shapes are individually named, reviewable, and shrink no further
//! than the short sequences below. Add a case by extending `generated_cases`;
//! the runner automatically applies every operation to in-memory, SQLite, and
//! Postgres and compares the observation after each step.
//!
//! Nodes are never observed through `load_session`: that constructs a
//! `SessionGraph` read model whose id indexes can hide duplicate durable rows.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use lash_core::store::GraphCommitDelta;
use lash_core::{
    InMemorySessionStore, LeaseOwnerIdentity, PendingTurnInputDraft, ProtocolEvent, Residency,
    RuntimeCommit, RuntimePersistence, RuntimeSessionState, RuntimeTurnCommitStamp, SessionGraph,
    SessionHistoryRecord, SessionNodePayload, SessionNodeRecord, StoreError, TurnInput,
    TurnInputClaim, TurnInputIngress,
};
use lash_postgres_store::PostgresStorage;
use rusqlite::OptionalExtension;
use sqlx::PgPool;

const SESSION_LEASE_TTL_MS: u64 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaseName {
    DuplicateWithinAppend,
    DuplicateAcrossCommits,
    AppendTombstoned,
    AppendTombstonedThenVacuumed,
    AppendOffActivePathUnderActivePathOnly,
    StaleExpectedHeadRevision,
    IdenticalAndMutatedTurnCommitReplay,
    SettleClaimAfterLeaseGenerationSuperseded,
}

impl CaseName {
    fn as_str(self) -> &'static str {
        match self {
            Self::DuplicateWithinAppend => "duplicate_node_id_within_one_append",
            Self::DuplicateAcrossCommits => "duplicate_node_id_across_two_commits",
            Self::AppendTombstoned => "append_onto_tombstoned_node_id",
            Self::AppendTombstonedThenVacuumed => "append_onto_tombstoned_then_vacuumed_node_id",
            Self::AppendOffActivePathUnderActivePathOnly => {
                "append_onto_off_active_path_node_id_under_active_path_only"
            }
            Self::StaleExpectedHeadRevision => "stale_expected_head_revision",
            Self::IdenticalAndMutatedTurnCommitReplay => "identical_and_mutated_turn_commit_replay",
            Self::SettleClaimAfterLeaseGenerationSuperseded => {
                "settle_claim_after_session_lease_generation_superseded_before_reclaim"
            }
        }
    }
}

#[derive(Clone, Debug)]
struct GeneratedCase {
    name: CaseName,
    residency: Residency,
    operations: Vec<StoreOperation>,
}

#[derive(Clone, Debug)]
enum StoreOperation {
    Commit {
        label: &'static str,
        expected_head_revision: Option<u64>,
        graph: GraphSpec,
        turn_commit: Option<TurnCommitSpec>,
    },
    Tombstone {
        node_ids: Vec<&'static str>,
    },
    Vacuum,
    EnqueueNextTurnInput,
    AcquireSessionLease {
        slot: LeaseSlot,
        owner: &'static str,
    },
    ClaimNextTurnInput {
        lease: LeaseSlot,
    },
    ReleaseSessionLease {
        lease: LeaseSlot,
    },
    CommitStaleTurnInputClaim {
        current_lease: LeaseSlot,
        expected_head_revision: Option<u64>,
    },
}

impl StoreOperation {
    fn label(&self) -> &'static str {
        match self {
            Self::Commit { label, .. } => label,
            Self::Tombstone { .. } => "tombstone",
            Self::Vacuum => "vacuum",
            Self::EnqueueNextTurnInput => "enqueue_next_turn_input",
            Self::AcquireSessionLease {
                slot: LeaseSlot::First,
                ..
            } => "acquire_first_session_lease_generation",
            Self::AcquireSessionLease {
                slot: LeaseSlot::Successor,
                ..
            } => "acquire_successor_session_lease_generation",
            Self::ClaimNextTurnInput { .. } => "claim_next_turn_input",
            Self::ReleaseSessionLease { .. } => "release_first_session_lease_generation",
            Self::CommitStaleTurnInputClaim { .. } => {
                "commit_stale_claim_before_successor_reclaims_row"
            }
        }
    }
}

#[derive(Clone, Debug)]
enum GraphSpec {
    Append {
        nodes: Vec<NodeSpec>,
        leaf_node_id: Option<&'static str>,
    },
    ReplaceFull {
        nodes: Vec<NodeSpec>,
        leaf_node_id: Option<&'static str>,
    },
}

#[derive(Clone, Copy, Debug)]
struct NodeSpec {
    node_id: &'static str,
    parent_node_id: Option<&'static str>,
    contents: &'static str,
}

impl NodeSpec {
    const fn new(
        node_id: &'static str,
        parent_node_id: Option<&'static str>,
        contents: &'static str,
    ) -> Self {
        Self {
            node_id,
            parent_node_id,
            contents,
        }
    }

    fn materialize(self) -> SessionNodeRecord {
        SessionNodeRecord {
            node_id: self.node_id.to_string(),
            parent_node_id: self.parent_node_id.map(str::to_string),
            caused_by: None,
            agent_frame_id: None,
            timestamp: "2026-07-26T00:00:00Z".to_string(),
            payload: SessionNodePayload::Event {
                event: SessionHistoryRecord::Protocol(
                    ProtocolEvent::typed(
                        "store-differential",
                        serde_json::json!({ "contents": self.contents }),
                    )
                    .expect("valid differential protocol event"),
                ),
            },
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct TurnCommitSpec {
    turn_id: &'static str,
    hash: &'static str,
}

#[derive(Clone, Copy, Debug)]
enum LeaseSlot {
    First,
    Successor,
}

fn append(nodes: Vec<NodeSpec>, leaf_node_id: Option<&'static str>) -> GraphSpec {
    GraphSpec::Append {
        nodes,
        leaf_node_id,
    }
}

fn replace_full(nodes: Vec<NodeSpec>, leaf_node_id: Option<&'static str>) -> GraphSpec {
    GraphSpec::ReplaceFull {
        nodes,
        leaf_node_id,
    }
}

fn commit(
    label: &'static str,
    expected_head_revision: Option<u64>,
    graph: GraphSpec,
) -> StoreOperation {
    StoreOperation::Commit {
        label,
        expected_head_revision,
        graph,
        turn_commit: None,
    }
}

fn generated_cases() -> Vec<GeneratedCase> {
    let original = || NodeSpec::new("collision", None, "original");
    let mutated = || NodeSpec::new("collision", None, "mutated");

    vec![
        GeneratedCase {
            name: CaseName::DuplicateWithinAppend,
            residency: Residency::KeepAll,
            operations: vec![commit(
                "append_duplicate_batch",
                None,
                append(vec![original(), mutated()], Some("collision")),
            )],
        },
        GeneratedCase {
            name: CaseName::DuplicateAcrossCommits,
            residency: Residency::KeepAll,
            operations: vec![
                commit(
                    "append_original",
                    None,
                    append(vec![original()], Some("collision")),
                ),
                commit(
                    "append_committed_id_again",
                    Some(1),
                    append(vec![mutated()], Some("collision")),
                ),
            ],
        },
        GeneratedCase {
            name: CaseName::AppendTombstoned,
            residency: Residency::KeepAll,
            operations: vec![
                commit(
                    "append_original",
                    None,
                    append(vec![original()], Some("collision")),
                ),
                StoreOperation::Tombstone {
                    node_ids: vec!["collision"],
                },
                commit(
                    "append_tombstoned_id",
                    Some(1),
                    append(vec![mutated()], Some("collision")),
                ),
            ],
        },
        GeneratedCase {
            name: CaseName::AppendTombstonedThenVacuumed,
            residency: Residency::KeepAll,
            operations: vec![
                commit(
                    "append_original",
                    None,
                    append(vec![original()], Some("collision")),
                ),
                StoreOperation::Tombstone {
                    node_ids: vec!["collision"],
                },
                StoreOperation::Vacuum,
                commit(
                    "append_vacuumed_id",
                    Some(1),
                    append(vec![mutated()], Some("collision")),
                ),
            ],
        },
        GeneratedCase {
            name: CaseName::AppendOffActivePathUnderActivePathOnly,
            residency: Residency::ActivePathOnly,
            operations: vec![
                commit(
                    "seed_forked_graph",
                    None,
                    replace_full(
                        vec![
                            NodeSpec::new("root", None, "root"),
                            NodeSpec::new("active-leaf", Some("root"), "active"),
                            NodeSpec::new("off-path", Some("root"), "off-path-original"),
                        ],
                        Some("active-leaf"),
                    ),
                ),
                commit(
                    "append_id_absent_from_active_path_resident_set",
                    Some(1),
                    append(
                        vec![NodeSpec::new("off-path", Some("root"), "off-path-mutated")],
                        Some("active-leaf"),
                    ),
                ),
            ],
        },
        GeneratedCase {
            name: CaseName::StaleExpectedHeadRevision,
            residency: Residency::KeepAll,
            operations: vec![
                commit(
                    "append_original",
                    None,
                    append(vec![original()], Some("collision")),
                ),
                commit(
                    "append_with_stale_head",
                    Some(0),
                    append(
                        vec![NodeSpec::new("fresh", Some("collision"), "fresh")],
                        Some("fresh"),
                    ),
                ),
            ],
        },
        GeneratedCase {
            name: CaseName::IdenticalAndMutatedTurnCommitReplay,
            residency: Residency::KeepAll,
            operations: vec![
                StoreOperation::Commit {
                    label: "first_turn_commit",
                    expected_head_revision: None,
                    graph: append(vec![original()], Some("collision")),
                    turn_commit: Some(TurnCommitSpec {
                        turn_id: "turn-1",
                        hash: "identical-hash",
                    }),
                },
                StoreOperation::Commit {
                    label: "resubmit_identical_turn_commit_hash",
                    expected_head_revision: None,
                    graph: append(vec![original()], Some("collision")),
                    turn_commit: Some(TurnCommitSpec {
                        turn_id: "turn-1",
                        hash: "identical-hash",
                    }),
                },
                StoreOperation::Commit {
                    label: "resubmit_mutated_turn_commit_hash",
                    expected_head_revision: Some(1),
                    graph: append(vec![mutated()], Some("collision")),
                    turn_commit: Some(TurnCommitSpec {
                        turn_id: "turn-1",
                        hash: "mutated-hash",
                    }),
                },
            ],
        },
        GeneratedCase {
            name: CaseName::SettleClaimAfterLeaseGenerationSuperseded,
            residency: Residency::KeepAll,
            operations: vec![
                StoreOperation::EnqueueNextTurnInput,
                StoreOperation::AcquireSessionLease {
                    slot: LeaseSlot::First,
                    owner: "first-owner",
                },
                StoreOperation::ClaimNextTurnInput {
                    lease: LeaseSlot::First,
                },
                StoreOperation::ReleaseSessionLease {
                    lease: LeaseSlot::First,
                },
                StoreOperation::AcquireSessionLease {
                    slot: LeaseSlot::Successor,
                    owner: "successor-owner",
                },
                StoreOperation::CommitStaleTurnInputClaim {
                    current_lease: LeaseSlot::Successor,
                    expected_head_revision: None,
                },
            ],
        },
    ]
}

fn materialize_graph(spec: &GraphSpec) -> GraphCommitDelta {
    match spec {
        GraphSpec::Append {
            nodes,
            leaf_node_id,
        } => GraphCommitDelta::Append {
            nodes: nodes.iter().copied().map(NodeSpec::materialize).collect(),
            leaf_node_id: leaf_node_id.map(str::to_string),
        },
        GraphSpec::ReplaceFull {
            nodes,
            leaf_node_id,
        } => GraphCommitDelta::ReplaceFull(SessionGraph::from_nodes(
            nodes.iter().copied().map(NodeSpec::materialize).collect(),
            leaf_node_id.map(str::to_string),
        )),
    }
}

fn runtime_commit(
    session_id: &str,
    expected_head_revision: Option<u64>,
    graph: &GraphSpec,
    turn_commit: Option<TurnCommitSpec>,
) -> RuntimeCommit {
    let state = RuntimeSessionState {
        session_id: session_id.to_string(),
        ..RuntimeSessionState::default()
    };
    let mut commit = RuntimeCommit::persisted_state(&state, &[]);
    commit.expected_head_revision = expected_head_revision;
    commit.graph = materialize_graph(graph);
    if let Some(turn_commit) = turn_commit {
        commit = commit.with_turn_commit(RuntimeTurnCommitStamp::new(
            session_id,
            turn_commit.turn_id,
            turn_commit.hash,
        ));
    }
    commit
}

#[derive(Clone, PartialEq, Eq)]
struct DurableNode {
    node_id: String,
    bytes: Vec<u8>,
}

impl std::fmt::Debug for DurableNode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DurableNode")
            .field("node_id", &self.node_id)
            .field("bytes", &String::from_utf8_lossy(&self.bytes))
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StepObservation {
    store_error_variant: Option<&'static str>,
    head_revision: Option<u64>,
    durable_nodes: Vec<DurableNode>,
}

enum RawDurableReader {
    InMemory(Arc<InMemorySessionStore>),
    Sqlite(PathBuf),
    Postgres { pool: PgPool, session_id: String },
}

impl RawDurableReader {
    async fn observe(&self) -> (Option<u64>, Vec<DurableNode>) {
        let (head_revision, mut nodes) = match self {
            Self::InMemory(store) => {
                let nodes = store
                    .raw_graph_nodes_for_testing()
                    .into_iter()
                    .map(|node| DurableNode {
                        node_id: node.node_id.clone(),
                        bytes: serde_json::to_vec(&node).expect("encode in-memory durable node"),
                    })
                    .collect();
                (store.raw_head_revision_for_testing(), nodes)
            }
            Self::Sqlite(path) => read_sqlite_durable_state(path),
            Self::Postgres { pool, session_id } => {
                let head_revision: Option<i64> = sqlx::query_scalar(
                    "SELECT head_revision FROM lash_sessions WHERE session_id = $1",
                )
                .bind(session_id)
                .fetch_optional(pool)
                .await
                .expect("read Postgres durable head");
                let rows: Vec<(String, String)> = sqlx::query_as(
                    "SELECT node_id, node_json
                     FROM lash_graph_nodes
                     WHERE session_id = $1 AND tombstoned = FALSE",
                )
                .bind(session_id)
                .fetch_all(pool)
                .await
                .expect("read Postgres durable nodes");
                (
                    head_revision.map(|revision| revision as u64),
                    rows.into_iter()
                        .map(|(node_id, node_json)| DurableNode {
                            node_id,
                            bytes: node_json.into_bytes(),
                        })
                        .collect(),
                )
            }
        };
        nodes.sort_by(|left, right| {
            (&left.node_id, &left.bytes).cmp(&(&right.node_id, &right.bytes))
        });
        (head_revision, nodes)
    }
}

fn read_sqlite_durable_state(path: &Path) -> (Option<u64>, Vec<DurableNode>) {
    let connection = rusqlite::Connection::open(path).expect("open SQLite durable reader");
    let head_revision = connection
        .query_row(
            "SELECT head_revision FROM session_head WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .expect("read SQLite durable head")
        .map(|revision| revision as u64);
    let mut statement = connection
        .prepare(
            "SELECT node_id, node_json
             FROM graph_nodes
             WHERE tombstoned = 0",
        )
        .expect("prepare SQLite durable node read");
    let nodes = statement
        .query_map([], |row| {
            Ok(DurableNode {
                node_id: row.get(0)?,
                bytes: row.get::<_, String>(1)?.into_bytes(),
            })
        })
        .expect("read SQLite durable nodes")
        .collect::<Result<Vec<_>, _>>()
        .expect("decode SQLite durable nodes");
    (head_revision, nodes)
}

struct BackendRunner {
    name: &'static str,
    session_id: String,
    store: Arc<dyn RuntimePersistence>,
    raw_reader: RawDurableReader,
    first_lease: Option<lash_core::SessionExecutionLease>,
    successor_lease: Option<lash_core::SessionExecutionLease>,
    stale_turn_input_claim: Option<TurnInputClaim>,
}

impl BackendRunner {
    fn lease(&self, slot: LeaseSlot) -> &lash_core::SessionExecutionLease {
        match slot {
            LeaseSlot::First => self.first_lease.as_ref(),
            LeaseSlot::Successor => self.successor_lease.as_ref(),
        }
        .expect("generated sequence acquired lease before use")
    }

    fn put_lease(&mut self, slot: LeaseSlot, lease: lash_core::SessionExecutionLease) {
        match slot {
            LeaseSlot::First => self.first_lease = Some(lease),
            LeaseSlot::Successor => self.successor_lease = Some(lease),
        }
    }

    async fn apply(&mut self, operation: &StoreOperation) -> Result<(), StoreError> {
        match operation {
            StoreOperation::Commit {
                expected_head_revision,
                graph,
                turn_commit,
                ..
            } => self
                .store
                .commit_runtime_state(runtime_commit(
                    &self.session_id,
                    *expected_head_revision,
                    graph,
                    *turn_commit,
                ))
                .await
                .map(|_| ()),
            StoreOperation::Tombstone { node_ids } => {
                let node_ids = node_ids.iter().map(ToString::to_string).collect::<Vec<_>>();
                self.store.tombstone_nodes(&node_ids).await
            }
            StoreOperation::Vacuum => self.store.vacuum().await.map(|_| ()),
            StoreOperation::EnqueueNextTurnInput => self
                .store
                .enqueue_pending_turn_input(
                    PendingTurnInputDraft::new(
                        &self.session_id,
                        TurnInputIngress::NextTurn,
                        TurnInput::text("generation-fenced input"),
                    )
                    .with_input_id(format!("{}:input", self.session_id)),
                )
                .await
                .map(|_| ()),
            StoreOperation::AcquireSessionLease { slot, owner } => {
                let owner = LeaseOwnerIdentity::opaque(*owner, format!("{owner}:incarnation"));
                let lease = self
                    .store
                    .try_claim_session_execution_lease(
                        &self.session_id,
                        &owner,
                        SESSION_LEASE_TTL_MS,
                    )
                    .await?
                    .acquired()
                    .ok_or_else(|| {
                        StoreError::Backend(format!(
                            "{} unexpectedly found the generated session lease busy",
                            self.name
                        ))
                    })?;
                self.put_lease(*slot, lease);
                Ok(())
            }
            StoreOperation::ClaimNextTurnInput { lease } => {
                let lease = self.lease(*lease);
                let owner = lease.owner.clone();
                let claim = self
                    .store
                    .claim_next_turn_inputs(&self.session_id, &lease.fence(), &owner, 1)
                    .await?
                    .ok_or_else(|| {
                        StoreError::Backend(format!(
                            "{} did not return the generated turn-input claim",
                            self.name
                        ))
                    })?;
                self.stale_turn_input_claim = Some(claim);
                Ok(())
            }
            StoreOperation::ReleaseSessionLease { lease } => {
                self.store
                    .release_session_execution_lease(&self.lease(*lease).completion())
                    .await
            }
            StoreOperation::CommitStaleTurnInputClaim {
                current_lease,
                expected_head_revision,
            } => {
                let lease = self.lease(*current_lease);
                let claim = self
                    .stale_turn_input_claim
                    .as_ref()
                    .expect("generated sequence claimed input before stale settlement");
                let graph = GraphSpec::Append {
                    nodes: vec![NodeSpec::new("stale-claim-node", None, "stale-claim")],
                    leaf_node_id: Some("stale-claim-node"),
                };
                self.store
                    .commit_runtime_state(
                        runtime_commit(&self.session_id, *expected_head_revision, &graph, None)
                            .with_session_execution_lease(lease.fence())
                            .completing_turn_input_claim(claim.completion()),
                    )
                    .await
                    .map(|_| ())
            }
        }
    }

    async fn observe(&self, error: Option<&StoreError>) -> StepObservation {
        let (head_revision, durable_nodes) = self.raw_reader.observe().await;
        StepObservation {
            store_error_variant: error.map(store_error_variant),
            head_revision,
            durable_nodes,
        }
    }
}

fn store_error_variant(error: &StoreError) -> &'static str {
    match error {
        StoreError::SessionBindingMismatch { .. } => "SessionBindingMismatch",
        StoreError::UnsupportedReadScope(_) => "UnsupportedReadScope",
        StoreError::HeadRevisionConflict { .. } => "HeadRevisionConflict",
        StoreError::RuntimeTurnCommitConflict { .. } => "RuntimeTurnCommitConflict",
        StoreError::QueuedWorkClaimSuperseded { .. } => "QueuedWorkClaimSuperseded",
        StoreError::TurnInputClaimSuperseded { .. } => "TurnInputClaimSuperseded",
        StoreError::UnsettledQueuedWorkClaim { .. } => "UnsettledQueuedWorkClaim",
        StoreError::UnsettledTurnInputClaim { .. } => "UnsettledTurnInputClaim",
        StoreError::PendingTurnInputSourceKeyConflict { .. } => "PendingTurnInputSourceKeyConflict",
        StoreError::SessionExecutionLeaseExpired { .. } => "SessionExecutionLeaseExpired",
        StoreError::UnsupportedRecordSchemaVersion { .. } => "UnsupportedRecordSchemaVersion",
        StoreError::MissingRecordSchemaVersion { .. } => "MissingRecordSchemaVersion",
        StoreError::InvalidRecordSchemaVersion { .. } => "InvalidRecordSchemaVersion",
        StoreError::Backend(_) => "Backend",
    }
}

async fn runners_for_case(
    case: CaseName,
    sqlite_root: &Path,
    postgres: &PostgresStorage,
    run_nonce: &str,
) -> Vec<BackendRunner> {
    let session_id = format!("fig-643-{run_nonce}-{}", case.as_str());

    let memory = Arc::new(InMemorySessionStore::new());
    let memory_store = Arc::clone(&memory) as Arc<dyn RuntimePersistence>;

    let sqlite_path = sqlite_root.join(format!("{}.db", case.as_str()));
    let sqlite = Arc::new(
        lash_sqlite_store::Store::open(&sqlite_path)
            .await
            .expect("open SQLite differential store"),
    );
    let sqlite_store = Arc::clone(&sqlite) as Arc<dyn RuntimePersistence>;

    let postgres_store =
        Arc::new(postgres.session_store(&session_id)) as Arc<dyn RuntimePersistence>;

    vec![
        BackendRunner {
            name: "in-memory",
            session_id: session_id.clone(),
            store: memory_store,
            raw_reader: RawDurableReader::InMemory(memory),
            first_lease: None,
            successor_lease: None,
            stale_turn_input_claim: None,
        },
        BackendRunner {
            name: "sqlite",
            session_id: session_id.clone(),
            store: sqlite_store,
            raw_reader: RawDurableReader::Sqlite(sqlite_path),
            first_lease: None,
            successor_lease: None,
            stale_turn_input_claim: None,
        },
        BackendRunner {
            name: "postgres",
            session_id: session_id.clone(),
            store: postgres_store,
            raw_reader: RawDurableReader::Postgres {
                pool: postgres.pool().clone(),
                session_id,
            },
            first_lease: None,
            successor_lease: None,
            stale_turn_input_claim: None,
        },
    ]
}

fn run_nonce() -> String {
    let epoch_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after Unix epoch")
        .as_nanos();
    format!("{}-{epoch_nanos}", std::process::id())
}

fn render_divergence(
    output: &mut String,
    case: &GeneratedCase,
    step_index: usize,
    operation: &StoreOperation,
    observations: &[(&str, StepObservation)],
) {
    let _ = writeln!(
        output,
        "\ncase={} residency={:?} step={} operation={}:",
        case.name.as_str(),
        case.residency,
        step_index + 1,
        operation.label()
    );
    for (backend, observation) in observations {
        let _ = writeln!(output, "  {backend}: {observation:#?}");
    }
}

#[test]
fn generated_catalog_covers_required_adversarial_shapes() {
    let cases = generated_cases();
    assert_eq!(cases.len(), 8);
    assert!(cases.iter().any(|case| {
        case.name == CaseName::AppendOffActivePathUnderActivePathOnly
            && matches!(case.residency, Residency::ActivePathOnly)
    }));
}

// FIG-637 owns the append-only contract change. Remove `ignore`, or run this
// test explicitly, to make every currently-known backend divergence red.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-637: preserves known RuntimePersistence backend divergences"]
async fn cross_backend_store_differential_reports_fig_637_divergences() {
    let database_url = std::env::var("LASH_POSTGRES_DATABASE_URL").expect(
        "LASH_POSTGRES_DATABASE_URL is required; this differential must run all three backends",
    );
    let postgres = PostgresStorage::connect(&database_url)
        .await
        .expect("connect required Postgres differential backend");
    let sqlite_root = tempfile::tempdir().expect("create SQLite differential root");
    let run_nonce = run_nonce();
    let mut divergences = String::new();

    for case in generated_cases() {
        let mut runners =
            runners_for_case(case.name, sqlite_root.path(), &postgres, &run_nonce).await;
        for (step_index, operation) in case.operations.iter().enumerate() {
            let mut observations = Vec::with_capacity(runners.len());
            for runner in &mut runners {
                let result = runner.apply(operation).await;
                let observation = runner.observe(result.as_ref().err()).await;
                observations.push((runner.name, observation));
            }
            let agrees = observations.windows(2).all(|pair| pair[0].1 == pair[1].1);
            if !agrees {
                render_divergence(
                    &mut divergences,
                    &case,
                    step_index,
                    operation,
                    &observations,
                );
                // Later mismatches in the same case are usually downstream
                // consequences of the first one, not independent signals.
                break;
            }
        }
    }

    assert!(
        divergences.is_empty(),
        "cross-backend durable state diverged:{divergences}"
    );
}
