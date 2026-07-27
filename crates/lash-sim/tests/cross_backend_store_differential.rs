//! Store-level differential testing for `RuntimePersistence`.
//!
//! The generator is deliberately table-driven for its first landing. These
//! malformed shapes are individually named, reviewable, and shrink no further
//! than the short sequences below. Add a case by extending `generated_cases`;
//! the runner automatically applies every operation to in-memory, SQLite, and
//! Postgres and compares the observation after each step.
//!
//! Agreement is not correctness: a differential cannot detect a defect shared
//! by all backends. The FIG-641 case below is the live example of that limit.
//!
//! Nodes are never observed through `load_session`: that constructs a
//! `SessionGraph` read model whose id indexes can hide duplicate durable rows.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lash_core::store::{GraphCommitDelta, RuntimeCommitResult, SessionHeadMeta};
use lash_core::{
    InMemorySessionStore, LeaseOwnerIdentity, PendingTurnInputDraft, ProtocolEvent, RuntimeCommit,
    RuntimePersistence, RuntimeSessionState, RuntimeTurnCommitStamp, SessionHistoryRecord,
    SessionNodePayload, SessionNodeRecord, StoreError, TurnInput, TurnInputApplication,
    TurnInputClaim, TurnInputIngress, TurnInputState,
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
    TombstonedLeaf,
    AppendDuplicateAfterAppendSeed,
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
            Self::TombstonedLeaf => "unchanged_commit_rejects_tombstoned_leaf",
            Self::AppendDuplicateAfterAppendSeed => "append_duplicate_node_id_after_append_seed",
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
    Unchanged {
        leaf_node_id: Option<&'static str>,
    },
    Append {
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

    fn materialize(self, session_id: &str) -> SessionNodeRecord {
        SessionNodeRecord {
            node_id: scoped_node_id(session_id, self.node_id),
            parent_node_id: self
                .parent_node_id
                .map(|node_id| scoped_node_id(session_id, node_id)),
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

fn scoped_node_id(session_id: &str, node_id: &str) -> String {
    format!("{session_id}:{node_id}")
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

fn unchanged(leaf_node_id: Option<&'static str>) -> GraphSpec {
    GraphSpec::Unchanged { leaf_node_id }
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
            operations: vec![commit(
                "append_duplicate_batch",
                None,
                append(vec![original(), mutated()], Some("collision")),
            )],
        },
        GeneratedCase {
            name: CaseName::DuplicateAcrossCommits,
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
            name: CaseName::TombstonedLeaf,
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
                    "unchanged_with_tombstoned_leaf",
                    Some(1),
                    unchanged(Some("collision")),
                ),
            ],
        },
        GeneratedCase {
            name: CaseName::AppendDuplicateAfterAppendSeed,
            // The store layer has no residency input. A host using
            // `ActivePathOnly` can produce this malformed Append because
            // `unique_message_node_id` de-duplicates only against its resident
            // set; the restored host-layer differential covers that path.
            operations: vec![
                commit(
                    "seed_forked_graph",
                    None,
                    append(
                        vec![
                            NodeSpec::new("root", None, "root"),
                            NodeSpec::new("active-leaf", Some("root"), "active"),
                            NodeSpec::new("off-path", Some("root"), "off-path-original"),
                        ],
                        Some("active-leaf"),
                    ),
                ),
                commit(
                    "append_duplicate_id_after_append_seed",
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
            // FIG-641: all three backends currently consume this superseded
            // claim, so this is an agreement test only. Its green result cannot
            // establish correctness because a differential cannot detect
            // uniform wrongness.
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

fn materialize_graph(session_id: &str, spec: &GraphSpec) -> GraphCommitDelta {
    match spec {
        GraphSpec::Unchanged { leaf_node_id } => GraphCommitDelta::Unchanged {
            leaf_node_id: leaf_node_id.map(str::to_string),
        },
        GraphSpec::Append {
            nodes,
            leaf_node_id,
        } => GraphCommitDelta::Append {
            nodes: nodes
                .iter()
                .copied()
                .map(|node| node.materialize(session_id))
                .collect(),
            leaf_node_id: leaf_node_id.map(|node_id| scoped_node_id(session_id, node_id)),
        },
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
    commit.graph = materialize_graph(session_id, graph);
    if let Some(turn_commit) = turn_commit {
        commit = commit.with_turn_commit(RuntimeTurnCommitStamp::new(
            session_id,
            lash_core::store::OperationId::turn(session_id, turn_commit.turn_id, "differential"),
            turn_commit.hash,
        ));
    }
    commit
}

#[derive(Clone, PartialEq, Eq)]
struct DurableNode {
    // SQL rows are read by `seq`; the in-memory vector uses its native index.
    // Comparing this normalized replay ordinal keeps transcript order
    // contract-visible without comparing backend-local sequence counters.
    ordinal: usize,
    node_id: String,
    // Both SQL backends currently store node_json as TEXT. A future jsonb
    // migration would reserialize values and make every byte comparison red
    // for a reason outside the persistence contract.
    bytes: Vec<u8>,
}

impl std::fmt::Debug for DurableNode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DurableNode")
            .field("ordinal", &self.ordinal)
            .field("node_id", &self.node_id)
            .field("bytes", &String::from_utf8_lossy(&self.bytes))
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingTurnInputObservation {
    input_id: String,
    state: TurnInputState,
    claim_session_lease_generation: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ComparableRuntimeCommitResult {
    head_revision: u64,
    turn_input_applications: Vec<TurnInputApplication>,
}

impl From<RuntimeCommitResult> for ComparableRuntimeCommitResult {
    fn from(result: RuntimeCommitResult) -> Self {
        Self {
            head_revision: result.head_revision,
            turn_input_applications: result.turn_input_applications,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RawDurableState {
    head_revision: Option<u64>,
    leaf_node_id: Option<String>,
    durable_nodes: Vec<DurableNode>,
    pending_turn_inputs: Vec<PendingTurnInputObservation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StepObservation {
    store_error: Option<String>,
    runtime_commit_result: Option<ComparableRuntimeCommitResult>,
    durable_state: RawDurableState,
}

enum RawDurableReader {
    InMemory(Arc<InMemorySessionStore>),
    Sqlite { path: PathBuf, session_id: String },
    Postgres { pool: PgPool, session_id: String },
}

impl RawDurableReader {
    async fn observe(&self) -> RawDurableState {
        match self {
            Self::InMemory(store) => {
                let durable_nodes = store
                    .raw_graph_nodes_for_testing()
                    .into_iter()
                    .enumerate()
                    .map(|(ordinal, node)| DurableNode {
                        ordinal,
                        node_id: node.node_id.clone(),
                        bytes: serde_json::to_vec(&node).expect("encode in-memory durable node"),
                    })
                    .collect();
                let pending_turn_inputs = store
                    .raw_pending_turn_inputs_for_testing()
                    .into_iter()
                    .map(|(input_id, state, claim_session_lease_generation)| {
                        PendingTurnInputObservation {
                            input_id,
                            state,
                            claim_session_lease_generation,
                        }
                    })
                    .collect();
                RawDurableState {
                    head_revision: store.raw_head_revision_for_testing(),
                    leaf_node_id: store.raw_leaf_node_id_for_testing(),
                    durable_nodes,
                    pending_turn_inputs,
                }
            }
            Self::Sqlite { path, session_id } => read_sqlite_durable_state(path, session_id),
            Self::Postgres { pool, session_id } => {
                let head: Option<(i64, String)> = sqlx::query_as(
                    "SELECT head_revision, head_json
                     FROM lash_sessions
                     WHERE session_id = $1",
                )
                .bind(session_id)
                .fetch_optional(pool)
                .await
                .expect("read Postgres durable head");
                let (head_revision, leaf_node_id) =
                    head.map_or((None, None), |(revision, json)| {
                        let meta: SessionHeadMeta =
                            serde_json::from_str(&json).expect("decode Postgres durable head");
                        (Some(revision as u64), meta.leaf_node_id)
                    });
                let rows: Vec<(i64, String, String)> = sqlx::query_as(
                    "SELECT seq, node_id, node_json
                     FROM lash_graph_nodes
                     WHERE session_id = $1 AND tombstoned = FALSE
                     ORDER BY seq ASC",
                )
                .bind(session_id)
                .fetch_all(pool)
                .await
                .expect("read Postgres durable nodes");
                let durable_nodes = rows
                    .into_iter()
                    .enumerate()
                    .map(|(ordinal, (_seq, node_id, node_json))| DurableNode {
                        ordinal,
                        node_id,
                        bytes: node_json.into_bytes(),
                    })
                    .collect();
                let pending_rows: Vec<(String, String, Option<i64>)> = sqlx::query_as(
                    "SELECT input_id, state,
                            CASE WHEN claim_token IS NULL
                                 THEN NULL
                                 ELSE claim_session_lease_generation
                            END
                     FROM lash_pending_turn_inputs
                     WHERE session_id = $1
                     ORDER BY enqueue_seq ASC",
                )
                .bind(session_id)
                .fetch_all(pool)
                .await
                .expect("read Postgres pending turn inputs");
                let pending_turn_inputs = pending_rows
                    .into_iter()
                    .map(|(input_id, state, claim_session_lease_generation)| {
                        PendingTurnInputObservation {
                            input_id,
                            state: TurnInputState::from_wire_str(&state)
                                .expect("decode Postgres pending-input state"),
                            claim_session_lease_generation: claim_session_lease_generation
                                .map(|generation| generation as u64),
                        }
                    })
                    .collect();
                RawDurableState {
                    head_revision,
                    leaf_node_id,
                    durable_nodes,
                    pending_turn_inputs,
                }
            }
        }
    }
}

fn read_sqlite_durable_state(path: &Path, session_id: &str) -> RawDurableState {
    let connection = rusqlite::Connection::open(path).expect("open SQLite durable reader");
    connection
        .busy_timeout(Duration::from_secs(15))
        .expect("configure SQLite durable reader busy timeout");
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .expect("configure SQLite durable reader WAL mode");
    connection
        .execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")
        .expect("configure SQLite durable reader pragmas");

    let head: Option<(i64, String)> = connection
        .query_row(
            "SELECT head_revision, head_json
             FROM session_head
             WHERE session_id = ?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .expect("read SQLite durable head");
    let (head_revision, leaf_node_id) = head.map_or((None, None), |(revision, json)| {
        let meta: SessionHeadMeta =
            serde_json::from_str(&json).expect("decode SQLite durable head");
        (Some(revision as u64), meta.leaf_node_id)
    });
    let durable_nodes = {
        let mut statement = connection
            .prepare(
                "SELECT seq, node_id, node_json
                 FROM graph_nodes
                 WHERE session_id = ?1 AND tombstoned = 0
                 ORDER BY seq ASC",
            )
            .expect("prepare SQLite durable node read");
        statement
            .query_map([session_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .expect("read SQLite durable nodes")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite durable nodes")
            .into_iter()
            .enumerate()
            .map(|(ordinal, (_seq, node_id, node_json))| DurableNode {
                ordinal,
                node_id,
                bytes: node_json.into_bytes(),
            })
            .collect()
    };
    let pending_turn_inputs = {
        let mut statement = connection
            .prepare(
                "SELECT input_id, state,
                        CASE WHEN claim_token IS NULL
                             THEN NULL
                             ELSE claim_session_lease_generation
                        END
                 FROM pending_turn_inputs
                 WHERE session_id = ?1
                 ORDER BY enqueue_seq ASC",
            )
            .expect("prepare SQLite pending-input read");
        statement
            .query_map([session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            })
            .expect("read SQLite pending turn inputs")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite pending turn inputs")
            .into_iter()
            .map(
                |(input_id, state, claim_session_lease_generation)| PendingTurnInputObservation {
                    input_id,
                    state: TurnInputState::from_wire_str(&state)
                        .expect("decode SQLite pending-input state"),
                    claim_session_lease_generation: claim_session_lease_generation
                        .map(|generation| generation as u64),
                },
            )
            .collect()
    };

    RawDurableState {
        head_revision,
        leaf_node_id,
        durable_nodes,
        pending_turn_inputs,
    }
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

    async fn apply(
        &mut self,
        operation: &StoreOperation,
    ) -> Result<Option<ComparableRuntimeCommitResult>, StoreError> {
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
                .map(|result| Some(result.into())),
            StoreOperation::Tombstone { node_ids } => {
                let node_ids = node_ids
                    .iter()
                    .map(|node_id| scoped_node_id(&self.session_id, node_id))
                    .collect::<Vec<_>>();
                self.store.tombstone_nodes(&node_ids).await.map(|_| None)
            }
            StoreOperation::Vacuum => self.store.vacuum().await.map(|_| None),
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
                .map(|_| None),
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
                if matches!(slot, LeaseSlot::Successor) {
                    let first = self.lease(LeaseSlot::First);
                    assert!(
                        lease.fencing_token > first.fencing_token,
                        "{} reused session-lease generation {} for the successor",
                        self.name,
                        lease.fencing_token
                    );
                }
                self.put_lease(*slot, lease);
                Ok(None)
            }
            StoreOperation::ClaimNextTurnInput { lease } => {
                let lease = self.lease(*lease);
                let owner = lease.owner.clone();
                let mut claim = self
                    .store
                    .claim_next_turn_inputs(&self.session_id, &lease.fence(), &owner, 1)
                    .await?
                    .ok_or_else(|| {
                        StoreError::Backend(format!(
                            "{} did not return the generated turn-input claim",
                            self.name
                        ))
                    })?;
                claim.record_initial_turn_application("claim-turn", "claim-message");
                self.stale_turn_input_claim = Some(claim);
                Ok(None)
            }
            StoreOperation::ReleaseSessionLease { lease } => self
                .store
                .release_session_execution_lease(&self.lease(*lease).completion())
                .await
                .map(|_| None),
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
                    .map(|result| Some(result.into()))
            }
        }
    }

    async fn observe(
        &self,
        result: Result<Option<ComparableRuntimeCommitResult>, StoreError>,
    ) -> StepObservation {
        let (store_error, runtime_commit_result) = match result {
            Ok(result) => (None, result),
            Err(error) => (Some(normalized_store_error(self.name, &error)), None),
        };
        StepObservation {
            store_error,
            runtime_commit_result,
            durable_state: self.raw_reader.observe().await,
        }
    }
}

fn normalized_store_error(backend: &str, error: &StoreError) -> String {
    match error {
        StoreError::Contended => "Contended".to_string(),
        StoreError::CommitNodeBudgetExceeded { .. } => "CommitNodeBudgetExceeded".to_string(),
        StoreError::CommitByteBudgetExceeded { .. } => "CommitByteBudgetExceeded".to_string(),
        StoreError::SessionBindingMismatch { .. } => "SessionBindingMismatch".to_string(),
        StoreError::UnsupportedReadScope(_) => "UnsupportedReadScope".to_string(),
        StoreError::HeadRevisionConflict { .. } => "HeadRevisionConflict".to_string(),
        StoreError::RuntimeTurnCommitConflict { .. } => "RuntimeTurnCommitConflict".to_string(),
        StoreError::QueuedWorkClaimSuperseded { .. } => "QueuedWorkClaimSuperseded".to_string(),
        StoreError::TurnInputClaimSuperseded { .. } => "TurnInputClaimSuperseded".to_string(),
        StoreError::UnsettledQueuedWorkClaim { .. } => "UnsettledQueuedWorkClaim".to_string(),
        StoreError::UnsettledTurnInputClaim { .. } => "UnsettledTurnInputClaim".to_string(),
        StoreError::PendingTurnInputSourceKeyConflict { .. } => {
            "PendingTurnInputSourceKeyConflict".to_string()
        }
        StoreError::SessionExecutionLeaseExpired { .. } => {
            "SessionExecutionLeaseExpired".to_string()
        }
        StoreError::UnsupportedRecordSchemaVersion { .. } => {
            "UnsupportedRecordSchemaVersion".to_string()
        }
        StoreError::MissingRecordSchemaVersion { .. } => "MissingRecordSchemaVersion".to_string(),
        StoreError::InvalidRecordSchemaVersion { .. } => "InvalidRecordSchemaVersion".to_string(),
        StoreError::NodeIdDerivationMismatch { .. } => "NodeIdDerivationMismatch".to_string(),
        StoreError::NodeIdCollision { .. } => "NodeIdCollision".to_string(),
        StoreError::InvalidGraphLeaf { .. } => "InvalidGraphLeaf".to_string(),
        StoreError::CommitRealizationMismatch { .. } => "CommitRealizationMismatch".to_string(),
        StoreError::CommitFrameRealizationMismatch { .. } => {
            "CommitFrameRealizationMismatch".to_string()
        }
        StoreError::Backend(message) => normalized_backend_error(backend, message),
    }
}

fn normalized_backend_error(backend: &str, message: &str) -> String {
    match backend {
        "sqlite" if message.contains("UNIQUE constraint failed") => {
            "Backend(sqlite:2067:SQLITE_CONSTRAINT_UNIQUE)".to_string()
        }
        "sqlite" if message.contains("database is locked") => {
            "Backend(sqlite:5:SQLITE_BUSY)".to_string()
        }
        "postgres" if message.contains("duplicate key value violates unique constraint") => {
            "Backend(postgres:23505:unique_violation)".to_string()
        }
        _ => format!("Backend({backend}:unclassified:{message})"),
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
            raw_reader: RawDurableReader::Sqlite {
                path: sqlite_path,
                session_id: session_id.clone(),
            },
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
        "\ncase={} step={} operation={}:",
        case.name.as_str(),
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
    assert_eq!(cases.len(), 9);
    assert!(cases.iter().all(|case| !case.operations.is_empty()));
    assert_eq!(
        cases
            .iter()
            .map(|case| case.name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "duplicate_node_id_within_one_append",
            "duplicate_node_id_across_two_commits",
            "append_onto_tombstoned_node_id",
            "append_onto_tombstoned_then_vacuumed_node_id",
            "unchanged_commit_rejects_tombstoned_leaf",
            "append_duplicate_node_id_after_append_seed",
            "stale_expected_head_revision",
            "identical_and_mutated_turn_commit_replay",
            "settle_claim_after_session_lease_generation_superseded_before_reclaim",
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_backend_store_differential_agrees() {
    let database_url = match std::env::var("LASH_POSTGRES_DATABASE_URL") {
        Ok(database_url) if !database_url.is_empty() => database_url,
        Ok(_) => {
            assert_ne!(
                std::env::var("LASH_REQUIRE_POSTGRES").as_deref(),
                Ok("1"),
                "LASH_POSTGRES_DATABASE_URL must be non-empty when LASH_REQUIRE_POSTGRES=1"
            );
            eprintln!(
                "skipping cross-backend store differential: \
                 LASH_POSTGRES_DATABASE_URL is not set"
            );
            return;
        }
        Err(error) => {
            assert_ne!(
                std::env::var("LASH_REQUIRE_POSTGRES").as_deref(),
                Ok("1"),
                "LASH_POSTGRES_DATABASE_URL must be set when LASH_REQUIRE_POSTGRES=1: {error}"
            );
            eprintln!(
                "skipping cross-backend store differential: \
                 LASH_POSTGRES_DATABASE_URL is not set"
            );
            return;
        }
    };
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
                let observation = runner.observe(result).await;
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
