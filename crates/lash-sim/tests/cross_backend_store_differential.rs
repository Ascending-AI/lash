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

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lash_core::facade_support::ToolStateFacadeOps;
use lash_core::runtime::{
    QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaim, QueuedWorkClaimBoundary,
    QueuedWorkPayload,
};
use lash_core::store::{GraphAppend, RuntimeCommitReceipt};
use lash_core::{
    AttachmentId, AttachmentIntent, AttachmentOwnerKind, BlobRef, Clock, DeliveryPolicy,
    ForkSessionRequest, HydratedSessionCheckpoint, LeaseClaimNonce, LeaseOwnerIdentity,
    PendingTurnInputDraft, PluginSessionSnapshot, PluginSnapshotArtifact, PluginSnapshotEntry,
    PluginSnapshotMeta, ProtocolEvent, QueuedWorkAuthority, QueuedWorkKind, RuntimeCommit,
    RuntimePersistence, RuntimeSessionState, RuntimeTurnCommitStamp, SessionHistoryRecord,
    SessionMeta, SessionNodePayload, SessionNodeRecord, SessionRelation, SessionStoreCreateRequest,
    SessionStoreFactory, StoreError, StoreMaintenance, TokenLedgerEntry, TokenUsage, ToolState,
    TriggerOwnerScope, TurnInput, TurnInputApplication, TurnInputClaim, TurnInputIngress,
    TurnInputState, facade_support::InMemorySessionStore,
    facade_support::InMemorySessionStoreFactory,
};
use lash_postgres_store::PostgresStorage;
use rusqlite::OptionalExtension;
use sqlx::{Connection, PgConnection, PgPool};

#[path = "cross_backend_store_differential/checkpoint_cases.rs"]
mod checkpoint_cases;
#[path = "cross_backend_store_differential/coalesced_batch_oracles.rs"]
mod coalesced_batch_oracles;
#[path = "cross_backend_store_differential/fork_cases.rs"]
mod fork_cases;
#[path = "cross_backend_store_differential/generated_surface.rs"]
mod generated_surface;
#[path = "cross_backend_store_differential/observations.rs"]
mod observations;
#[path = "cross_backend_store_differential/raw_durable_reader.rs"]
mod raw_durable_reader;
#[path = "cross_backend_store_differential/session_meta_layout.rs"]
mod session_meta_layout;
use observations::*;
use session_meta_layout::verify_independent_session_meta_layout;

const SESSION_LEASE_TTL_MS: u64 = 60_000;
// "LASH_PGT" encoded as a positive i64. This must match the shared-database
// advisory lock used by lash-postgres-store's integration-test harness.
const SHARED_DATABASE_LOCK_KEY: i64 = 0x4c41_5348_5f50_4754;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaseName {
    DuplicateWithinAppend,
    DuplicateAcrossCommits,
    AppendDuplicateAfterAppendSeed,
    NodelessLeafMove,
    StaleExpectedHeadRevision,
    IdenticalAndMutatedTurnCommitReplay,
    SettleClaimBeforeSuccessorReclaim,
    CheckpointBodiesThenRefOnly,
    CheckpointBodiesThenCleared,
    MissingCheckpointComponentRef,
    ForkFencePrecedence,
    PinForkUnpin,
    ForeignLineageFork,
    Rewind,
    AttachmentAdoption,
    QueuedWorkClaimAndAbandon,
    DeleteThenAttemptAdmission,
    StaleHandleAfterDelete,
}

impl CaseName {
    fn as_str(self) -> &'static str {
        match self {
            Self::DuplicateWithinAppend => "duplicate_node_id_within_one_append",
            Self::DuplicateAcrossCommits => "duplicate_node_id_across_two_commits",
            Self::AppendDuplicateAfterAppendSeed => "append_duplicate_node_id_after_append_seed",
            Self::NodelessLeafMove => "nodeless_commit_cannot_move_leaf",
            Self::StaleExpectedHeadRevision => "stale_expected_head_revision",
            Self::IdenticalAndMutatedTurnCommitReplay => "identical_and_mutated_turn_commit_replay",
            Self::SettleClaimBeforeSuccessorReclaim => {
                "settle_claim_after_session_lease_handoff_before_reclaim"
            }
            Self::CheckpointBodiesThenRefOnly => "checkpoint_bodies_then_ref_only",
            Self::CheckpointBodiesThenCleared => "checkpoint_bodies_then_cleared",
            Self::MissingCheckpointComponentRef => "missing_checkpoint_component_ref",
            Self::ForkFencePrecedence => "fork_fence_exists_precedes_other_fences",
            Self::PinForkUnpin => "pin_fork_unpin_moves_node_anchor",
            Self::ForeignLineageFork => "fork_accepts_foreign_lineage",
            Self::Rewind => "rewind_fork_delete_source_refork",
            Self::AttachmentAdoption => "attachment_intent_adopted_by_commit",
            Self::QueuedWorkClaimAndAbandon => "queued_work_claim_abandon_preserves_fencing_token",
            Self::DeleteThenAttemptAdmission => "delete_then_attempt_admission",
            Self::StaleHandleAfterDelete => "stale_handle_after_delete",
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
        expected_head_revision: u64,
        graph: GraphSpec,
        turn_commit: Option<TurnCommitSpec>,
        checkpoint: CheckpointSpec,
        usage: bool,
        adopt_attachment: bool,
    },
    RecordAttachmentIntent,
    PinLeaf,
    ForkAtLeaf,
    ForkAtExistingTarget,
    ForkAtForeignLineage,
    Rewind,
    UnpinLeaf,
    EnqueueNextTurnInput,
    EnqueueQueuedWork,
    EnqueueClaimableQueuedWork,
    AcquireSessionLease {
        slot: LeaseSlot,
        owner: &'static str,
    },
    ClaimNextTurnInput {
        lease: LeaseSlot,
    },
    ClaimQueuedWork {
        lease: LeaseSlot,
    },
    AbandonQueuedWorkClaim,
    ReleaseSessionLease {
        lease: LeaseSlot,
    },
    CommitStaleTurnInputClaim {
        expected_head_revision: u64,
    },
    /// SQLite and PostgreSQL discard the live store/factory and reopen through
    /// an independent connection. In-memory has no independent durable
    /// instance, so its leg can only reopen the same object through the
    /// retained factory and does not prove cold-instance reconstruction.
    ColdReopenSession,
    /// Enter deletion through `LashCore::delete_session` to exercise the store
    /// tombstone and subsequent admission refusal. This fixture deliberately
    /// wires neither a process registry nor a trigger store, so it does not
    /// cover the process-deletion or trigger-subscription deletion legs.
    DeleteSession,
    AttemptAdmission,
    CreateHandle {
        handle_alias: &'static str,
    },
    DeleteSessionThroughFactory,
    AdmitOnHandle {
        handle_alias: &'static str,
    },
    SaveMetaOnHandle {
        handle_alias: &'static str,
    },
    CommitOnHandle {
        handle_alias: &'static str,
    },
    ObserveSessionAbsent,
}

impl StoreOperation {
    fn label(&self) -> &'static str {
        match self {
            Self::Commit { label, .. } => label,
            Self::RecordAttachmentIntent => "record_attachment_intent",
            Self::PinLeaf => "pin_leaf",
            Self::ForkAtLeaf => "fork_at_leaf",
            Self::ForkAtExistingTarget => "fork_existing_target_precedes_point_fences",
            Self::ForkAtForeignLineage => "fork_at_foreign_lineage",
            Self::Rewind => "rewind_fork_delete_source_refork",
            Self::UnpinLeaf => "unpin_leaf",
            Self::EnqueueNextTurnInput => "enqueue_next_turn_input",
            Self::EnqueueQueuedWork => "enqueue_queued_work",
            Self::EnqueueClaimableQueuedWork => "enqueue_claimable_queued_work",
            Self::AcquireSessionLease {
                slot: LeaseSlot::First,
                ..
            } => "acquire_first_session_lease_generation",
            Self::AcquireSessionLease {
                slot: LeaseSlot::Successor,
                ..
            } => "acquire_successor_session_lease_generation",
            Self::ClaimNextTurnInput { .. } => "claim_next_turn_input",
            Self::ClaimQueuedWork { .. } => "claim_queued_work",
            Self::AbandonQueuedWorkClaim => "abandon_queued_work_claim",
            Self::ReleaseSessionLease { .. } => "release_first_session_lease_generation",
            Self::CommitStaleTurnInputClaim { .. } => {
                "commit_stale_claim_before_successor_reclaims_row"
            }
            Self::ColdReopenSession => "cold_reopen_session",
            Self::DeleteSession => "delete_session",
            Self::AttemptAdmission => "attempt_admission",
            Self::CreateHandle { .. } => "create_handle",
            Self::DeleteSessionThroughFactory => "delete_session_through_factory",
            Self::AdmitOnHandle { .. } => "admit_on_handle",
            Self::SaveMetaOnHandle { .. } => "save_meta_on_handle",
            Self::CommitOnHandle { .. } => "commit_on_handle",
            Self::ObserveSessionAbsent { .. } => "observe_session_absent",
        }
    }
}

#[derive(Clone, Debug)]
struct GraphSpec {
    nodes: Vec<NodeSpec>,
    leaf_node_id: Option<&'static str>,
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
        let frame_key = differential_frame_key(self.node_id);
        SessionNodeRecord {
            node_id: scoped_node_id(session_id, self.node_id),
            parent_node_id: self
                .parent_node_id
                .map(|node_id| scoped_node_id(session_id, node_id)),
            timestamp: "2026-07-26T00:00:00Z".to_string(),
            payload: if is_frame_alias(self.node_id) {
                SessionNodePayload::FrameOpen {
                    frame_key,
                    reason: lash_core::AgentFrameReason::initial(),
                    assignment: lash_core::AgentFrameAssignment::from_policy(
                        lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
                    ),
                    protocol_turn_options: Default::default(),
                }
            } else {
                SessionNodePayload::Event {
                    event: SessionHistoryRecord::Protocol(
                        ProtocolEvent::typed(
                            "store-differential",
                            serde_json::json!({ "contents": self.contents }),
                        )
                        .expect("valid differential protocol event"),
                    ),
                }
            },
        }
    }
}

fn differential_frame_key(node_id: &str) -> String {
    format!("differential-frame:{node_id}")
}

fn is_frame_alias(node_id: &str) -> bool {
    matches!(
        node_id,
        "active-frame" | "collision" | "root" | "stale-claim-node"
    )
}

fn scoped_node_id(session_id: &str, node_id: &str) -> String {
    if is_frame_alias(node_id) {
        lash_core::facade_support::frame_node_id(session_id, &differential_frame_key(node_id))
            .into_inner()
    } else {
        format!("{session_id}:{node_id}")
    }
}

#[derive(Clone, Copy, Debug)]
struct TurnCommitSpec {
    turn_id: &'static str,
}

#[derive(Clone, Copy, Debug)]
enum CheckpointSpec {
    Empty,
    Bodies,
    PriorRefs,
    ClearedComponents,
    MissingExecutionStateRef,
}

#[derive(Clone, Copy, Debug)]
enum LeaseSlot {
    First,
    Successor,
}

fn append(nodes: Vec<NodeSpec>, leaf_node_id: Option<&'static str>) -> GraphSpec {
    GraphSpec {
        nodes,
        leaf_node_id,
    }
}

fn commit(label: &'static str, expected_head_revision: u64, graph: GraphSpec) -> StoreOperation {
    StoreOperation::Commit {
        label,
        expected_head_revision,
        graph,
        turn_commit: None,
        checkpoint: CheckpointSpec::Empty,
        usage: false,
        adopt_attachment: false,
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
                0,
                append(vec![original(), mutated()], Some("collision")),
            )],
        },
        GeneratedCase {
            name: CaseName::DuplicateAcrossCommits,
            operations: vec![
                commit(
                    "append_original",
                    0,
                    append(vec![original()], Some("collision")),
                ),
                commit(
                    "append_committed_id_again",
                    1,
                    append(vec![mutated()], Some("collision")),
                ),
            ],
        },
        GeneratedCase {
            name: CaseName::AppendDuplicateAfterAppendSeed,
            // A duplicate append id must be rejected even when its parent and
            // terminal leaf otherwise form a valid linear continuation.
            operations: vec![
                commit(
                    "seed_graph",
                    0,
                    append(
                        vec![
                            NodeSpec::new("root", None, "root"),
                            NodeSpec::new("active-leaf", Some("root"), "active"),
                        ],
                        Some("active-leaf"),
                    ),
                ),
                commit(
                    "append_duplicate_id_after_append_seed",
                    1,
                    append(
                        vec![NodeSpec::new(
                            "active-leaf",
                            Some("active-leaf"),
                            "duplicate",
                        )],
                        Some("active-leaf"),
                    ),
                ),
            ],
        },
        GeneratedCase {
            name: CaseName::NodelessLeafMove,
            operations: vec![
                commit(
                    "seed_graph",
                    0,
                    append(
                        vec![
                            NodeSpec::new("root", None, "root"),
                            NodeSpec::new("active-frame", Some("root"), "active"),
                        ],
                        Some("active-frame"),
                    ),
                ),
                StoreOperation::Commit {
                    label: "move_leaf_without_appending_nodes",
                    expected_head_revision: 1,
                    graph: append(Vec::new(), Some("root")),
                    turn_commit: Some(TurnCommitSpec {
                        turn_id: "nodeless-leaf-move",
                    }),
                    checkpoint: CheckpointSpec::Empty,
                    usage: false,
                    adopt_attachment: false,
                },
            ],
        },
        GeneratedCase {
            name: CaseName::StaleExpectedHeadRevision,
            operations: vec![
                commit(
                    "append_original",
                    0,
                    append(vec![original()], Some("collision")),
                ),
                commit(
                    "append_with_stale_head",
                    0,
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
                    expected_head_revision: 0,
                    graph: append(vec![original()], Some("collision")),
                    turn_commit: Some(TurnCommitSpec { turn_id: "turn-1" }),
                    checkpoint: CheckpointSpec::Empty,
                    usage: false,
                    adopt_attachment: false,
                },
                StoreOperation::Commit {
                    label: "resubmit_identical_turn_commit_hash",
                    expected_head_revision: 0,
                    graph: append(vec![original()], Some("collision")),
                    turn_commit: Some(TurnCommitSpec { turn_id: "turn-1" }),
                    checkpoint: CheckpointSpec::Empty,
                    usage: false,
                    adopt_attachment: false,
                },
                StoreOperation::Commit {
                    label: "resubmit_mutated_turn_commit_hash",
                    expected_head_revision: 1,
                    graph: append(vec![mutated()], Some("collision")),
                    turn_commit: Some(TurnCommitSpec { turn_id: "turn-1" }),
                    checkpoint: CheckpointSpec::Empty,
                    usage: false,
                    adopt_attachment: false,
                },
            ],
        },
        GeneratedCase {
            name: CaseName::SettleClaimBeforeSuccessorReclaim,
            // FIG-641 / ADR 0029: supersession is reclaim-mediated by design.
            // All three backends currently accept this pre-reclaim settlement;
            // this differential demonstrates agreement, not a conformance law.
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
                    expected_head_revision: 0,
                },
            ],
        },
        checkpoint_cases::bodies_then_ref_only(),
        checkpoint_cases::bodies_then_cleared(),
        GeneratedCase {
            name: CaseName::MissingCheckpointComponentRef,
            operations: vec![StoreOperation::Commit {
                label: "commit_ref_for_never_stored_execution_state",
                expected_head_revision: 0,
                graph: append(Vec::new(), None),
                turn_commit: Some(TurnCommitSpec {
                    turn_id: "missing-component",
                }),
                checkpoint: CheckpointSpec::MissingExecutionStateRef,
                usage: false,
                adopt_attachment: false,
            }],
        },
        fork_cases::fence_precedence_case(),
        GeneratedCase {
            name: CaseName::PinForkUnpin,
            operations: vec![
                StoreOperation::Commit {
                    label: "commit_forkable_leaf",
                    expected_head_revision: 0,
                    graph: append(
                        vec![NodeSpec::new("active-frame", None, "forkable")],
                        Some("active-frame"),
                    ),
                    turn_commit: Some(TurnCommitSpec {
                        turn_id: "forkable-leaf",
                    }),
                    checkpoint: CheckpointSpec::Empty,
                    usage: false,
                    adopt_attachment: false,
                },
                StoreOperation::PinLeaf,
                StoreOperation::ForkAtLeaf,
                StoreOperation::UnpinLeaf,
            ],
        },
        fork_cases::foreign_lineage_case(),
        fork_cases::rewind_case(),
        GeneratedCase {
            name: CaseName::AttachmentAdoption,
            operations: vec![
                StoreOperation::RecordAttachmentIntent,
                StoreOperation::Commit {
                    label: "adopt_attachment_in_runtime_commit",
                    expected_head_revision: 0,
                    graph: append(Vec::new(), None),
                    turn_commit: Some(TurnCommitSpec {
                        turn_id: "attachment-adoption",
                    }),
                    checkpoint: CheckpointSpec::Empty,
                    usage: false,
                    adopt_attachment: true,
                },
            ],
        },
        GeneratedCase {
            name: CaseName::QueuedWorkClaimAndAbandon,
            operations: vec![
                StoreOperation::EnqueueClaimableQueuedWork,
                StoreOperation::AcquireSessionLease {
                    slot: LeaseSlot::First,
                    owner: "queued-work-owner",
                },
                StoreOperation::ClaimQueuedWork {
                    lease: LeaseSlot::First,
                },
                StoreOperation::AbandonQueuedWorkClaim,
            ],
        },
        GeneratedCase {
            name: CaseName::DeleteThenAttemptAdmission,
            operations: vec![
                StoreOperation::DeleteSession,
                StoreOperation::AttemptAdmission,
            ],
        },
        GeneratedCase {
            name: CaseName::StaleHandleAfterDelete,
            operations: vec![
                StoreOperation::EnqueueQueuedWork,
                StoreOperation::CreateHandle {
                    handle_alias: "handle-1",
                },
                StoreOperation::DeleteSessionThroughFactory,
                StoreOperation::AdmitOnHandle {
                    handle_alias: "handle-1",
                },
                StoreOperation::SaveMetaOnHandle {
                    handle_alias: "handle-1",
                },
                StoreOperation::CommitOnHandle {
                    handle_alias: "handle-1",
                },
                StoreOperation::ObserveSessionAbsent,
            ],
        },
    ]
}

fn materialize_graph(session_id: &str, spec: &GraphSpec) -> GraphAppend {
    GraphAppend {
        nodes: spec
            .nodes
            .iter()
            .copied()
            .map(|node| node.materialize(session_id))
            .collect(),
        leaf_node_id: spec
            .leaf_node_id
            .map(|node_id| scoped_node_id(session_id, node_id)),
    }
}

// A commit is genuinely this many independent parts; bundling them into a
// params struct here would only move the same fields behind another name.
#[allow(clippy::too_many_arguments)]
fn runtime_commit(
    session_id: &str,
    expected_head_revision: u64,
    graph: &GraphSpec,
    turn_commit: Option<TurnCommitSpec>,
    current_frame_node_id: Option<lash_core::FrameNodeId>,
    checkpoint: HydratedSessionCheckpoint,
    usage_deltas: Vec<TokenLedgerEntry>,
    committed_attachment_ids: Vec<AttachmentId>,
) -> RuntimeCommit {
    let state = RuntimeSessionState {
        session_id: session_id.to_string(),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    let mut commit = RuntimeCommit::persisted_state_for_test(&state, &usage_deltas);
    commit.expected_head_revision = expected_head_revision;
    commit.graph = materialize_graph(session_id, graph);
    commit.current_frame_node_id = commit
        .graph
        .appended_nodes()
        .filter_map(|node| match &node.payload {
            SessionNodePayload::FrameOpen { frame_key, .. } => Some(
                lash_core::facade_support::frame_node_id(session_id, frame_key),
            ),
            _ => None,
        })
        .last()
        .or(current_frame_node_id);
    if let Some(turn_commit) = turn_commit {
        commit.turn_commit = RuntimeTurnCommitStamp::new(lash_core::store::OperationId::turn(
            session_id,
            turn_commit.turn_id,
            "differential",
        ));
    }
    commit.checkpoint = checkpoint;
    commit.committed_attachment_ids = committed_attachment_ids;
    commit
}

#[derive(Clone, Debug)]
struct CheckpointComponentRefs {
    components: BTreeMap<String, lash_core::CheckpointComponentDescriptor>,
}

fn checkpoint_bodies() -> HydratedSessionCheckpoint {
    let tool_state = serde_json::from_value::<ToolState>(serde_json::json!({
        "generation": 7,
        "tools": {}
    }))
    .expect("build differential tool state");
    let plugin_snapshot = PluginSessionSnapshot {
        plugins: [(
            "differential-plugin".to_string(),
            PluginSnapshotEntry {
                meta: PluginSnapshotMeta {
                    plugin_id: "differential-plugin".to_string(),
                    plugin_version: "1.2.3".to_string(),
                    revision: 11,
                    state: Some(serde_json::json!({"mode": "durable"})),
                },
                artifacts: vec![PluginSnapshotArtifact {
                    name: "snapshot.bin".to_string(),
                    data: vec![4, 2, 4, 2],
                }],
            },
        )]
        .into_iter()
        .collect(),
    };
    let components = [
        (
            lash_core::store::TOOL_STATE_CHECKPOINT_COMPONENT.to_string(),
            lash_core::HydratedCheckpointComponent::changed(
                rmp_serde::to_vec_named(&tool_state).expect("encode differential tool state"),
            ),
        ),
        (
            lash_core::store::PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT.to_string(),
            lash_core::HydratedCheckpointComponent::changed(
                rmp_serde::to_vec_named(&plugin_snapshot)
                    .expect("encode differential plugin snapshot"),
            ),
        ),
        (
            lash_core::store::EXECUTION_STATE_CHECKPOINT_COMPONENT.to_string(),
            lash_core::HydratedCheckpointComponent::changed(vec![9, 8, 7, 6]),
        ),
        (
            "arbitrary/differential".to_string(),
            lash_core::HydratedCheckpointComponent::changed(b"arbitrary-component-body".to_vec()),
        ),
    ]
    .into_iter()
    .collect();
    HydratedSessionCheckpoint {
        turn_state: lash_core::PersistedTurnState {
            turn_index: 37,
            token_usage: TokenUsage {
                input_tokens: 13,
                output_tokens: 8,
                cache_read_input_tokens: 5,
                cache_write_input_tokens: 3,
                reasoning_output_tokens: 2,
            },
            ..Default::default()
        },
        components,
        plugin_snapshot_revision: Some(11),
    }
}

fn checkpoint_from_spec(
    spec: CheckpointSpec,
    prior_refs: Option<&CheckpointComponentRefs>,
) -> HydratedSessionCheckpoint {
    match spec {
        CheckpointSpec::Empty => HydratedSessionCheckpoint::default(),
        CheckpointSpec::Bodies => checkpoint_bodies(),
        CheckpointSpec::PriorRefs => {
            let refs = prior_refs.expect("body commit recorded component refs");
            let components = refs
                .components
                .iter()
                .map(|(key, descriptor)| {
                    (
                        key.clone(),
                        lash_core::HydratedCheckpointComponent::unchanged(descriptor),
                    )
                })
                .collect();
            HydratedSessionCheckpoint {
                turn_state: checkpoint_bodies().turn_state,
                components,
                plugin_snapshot_revision: Some(11),
            }
        }
        CheckpointSpec::ClearedComponents => {
            let refs = prior_refs.expect("body commit recorded component refs");
            let components = refs
                .components
                .iter()
                .filter(|(key, _)| {
                    key.as_str() != lash_core::store::EXECUTION_STATE_CHECKPOINT_COMPONENT
                })
                .map(|(key, descriptor)| {
                    (
                        key.clone(),
                        lash_core::HydratedCheckpointComponent::unchanged(descriptor),
                    )
                })
                .collect();
            HydratedSessionCheckpoint {
                turn_state: checkpoint_bodies().turn_state,
                components,
                plugin_snapshot_revision: Some(11),
            }
        }
        CheckpointSpec::MissingExecutionStateRef => HydratedSessionCheckpoint {
            components: [(
                lash_core::store::EXECUTION_STATE_CHECKPOINT_COMPONENT.to_string(),
                lash_core::HydratedCheckpointComponent::Unchanged {
                    descriptor: lash_core::CheckpointComponentDescriptor {
                        blob_ref: BlobRef(
                            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                                .to_string(),
                        ),
                        encoding_version: lash_core::store::CHECKPOINT_COMPONENT_ENCODING_VERSION,
                    },
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        },
    }
}

fn differential_usage_delta() -> TokenLedgerEntry {
    TokenLedgerEntry {
        source: "differential".to_string(),
        model: "test/model".to_string(),
        usage: TokenUsage {
            input_tokens: 21,
            output_tokens: 12,
            cache_read_input_tokens: 4,
            cache_write_input_tokens: 3,
            reasoning_output_tokens: 2,
        },
    }
}

fn differential_attachment_id() -> AttachmentId {
    AttachmentId::parse("differential-attachment").expect("valid attachment id")
}

// Row shapes for the SQL observation queries. Named because the tuples are wide
// enough that clippy flags them inline, and a name reads better at the use site.
type AttachmentRow = (
    String,
    String,
    i64,
    Option<i64>,
    Option<String>,
    Option<String>,
);
type LeaseRow = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    i64,
    i64,
    i64,
);
type QueuedWorkBatchRow = (
    i64,
    String,
    Option<String>,
    String,
    String,
    String,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    i64,
);
type QueuedWorkItemRow = (String, i64, String);

enum RawDurableReader {
    InMemory {
        store: Arc<InMemorySessionStore>,
        factory: Arc<InMemorySessionStoreFactory>,
        session_id: String,
    },
    Sqlite {
        path: PathBuf,
        session_id: String,
        store: Option<Arc<dyn RuntimePersistence>>,
    },
    Postgres {
        pool: PgPool,
        session_id: String,
        store: Option<Arc<dyn RuntimePersistence>>,
    },
}

fn attachment_manifest_observation(
    entry: lash_core::AttachmentManifestEntry,
) -> AttachmentManifestObservation {
    AttachmentManifestObservation {
        attachment_id: entry.attachment_id,
        canonical_uri: entry.canonical_uri,
        intent_at_epoch_ms: entry.intent_at_epoch_ms,
        committed: entry.committed_at_epoch_ms.is_some(),
        owner_kind: entry.owner_kind,
        owner_id: entry.owner_id,
    }
}

fn decode_attachment_owner_kind(value: Option<&str>) -> Option<AttachmentOwnerKind> {
    value.map(|value| match value {
        "turn" => AttachmentOwnerKind::Turn,
        "process" => AttachmentOwnerKind::Process,
        other => panic!("unknown attachment owner kind `{other}`"),
    })
}

fn usage_delta_observation(entry: TokenLedgerEntry) -> UsageDeltaObservation {
    UsageDeltaObservation {
        source: entry.source,
        model: entry.model,
        usage: entry.usage,
    }
}

fn session_meta_observation(meta: SessionMeta) -> SessionMetaObservation {
    SessionMetaObservation {
        relation: meta.relation,
    }
}

fn decode_lease_owner(
    owner_id: Option<String>,
    incarnation_id: Option<String>,
    liveness_json: Option<String>,
) -> Option<LeaseOwnerIdentity> {
    match (owner_id, incarnation_id, liveness_json) {
        (None, None, None) => None,
        (Some(owner_id), Some(incarnation_id), _) => {
            Some(LeaseOwnerIdentity::opaque(owner_id, incarnation_id))
        }
        fields => panic!("partial lease-owner identity in durable row: {fields:?}"),
    }
}

fn normalized_in_memory_node_json(node: &lash_core::SessionNodeRecord) -> Vec<u8> {
    // The in-memory backend holds records rather than rows, so it has no
    // `node_json` to read back; this side of the comparison has to produce one.
    // It produces it with the same storage-body codec the SQL backends write
    // with, which is what keeps this a comparison of backend semantics instead
    // of a comparison of two separately maintained envelopes -- the codec already
    // omits node identity and parent topology, both of which SQL keeps in indexed
    // columns and which are compared as dedicated `DurableNode` fields.
    let body = node
        .encode_storage_body()
        .expect("encode in-memory durable node");
    normalized_sql_node_json(&body)
}

fn normalized_sql_node_json(node_json: &str) -> Vec<u8> {
    let value = serde_json::from_str(node_json).expect("decode SQL durable node");
    normalized_node_json(value)
}

fn normalized_node_json(value: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&value).expect("encode normalized durable node")
}

async fn read_sqlite_durable_state(
    path: &Path,
    session_id: &str,
    store: &Arc<dyn RuntimePersistence>,
) -> RawDurableState {
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

    let head: Option<(i64, Option<String>, Option<String>)> = connection
        .query_row(
            "SELECT head_revision, leaf_node_id, checkpoint_ref
             FROM session_head
             WHERE session_id = ?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .expect("read SQLite durable head");
    let (head_revision, leaf_node_id, checkpoint_ref) = head.map_or(
        (None, None, None),
        |(revision, leaf_node_id, checkpoint_ref)| {
            (
                Some(revision as u64),
                leaf_node_id,
                checkpoint_ref.map(BlobRef),
            )
        },
    );
    let checkpoint = read_sqlite_checkpoint_observation(path, checkpoint_ref);
    let durable_nodes = {
        let mut statement = connection
            .prepare(
                "SELECT generation, node_id, parent_node_id, node_json
                 FROM graph_nodes
                 WHERE session_id = ?1 AND tombstoned = 0
                 ORDER BY generation ASC",
            )
            .expect("prepare SQLite durable node read");
        statement
            .query_map([session_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .expect("read SQLite durable nodes")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite durable nodes")
            .into_iter()
            .enumerate()
            .map(
                |(ordinal, (_generation, node_id, parent_node_id, node_json))| DurableNode {
                    ordinal,
                    node_id,
                    parent_node_id,
                    bytes: normalized_sql_node_json(&node_json),
                },
            )
            .collect()
    };
    let runtime_turn_commits = {
        let mut statement = connection
            .prepare(
                "SELECT turn_id, turn_commit_hash, result_json
                 FROM runtime_turn_commits
                 WHERE session_id = ?1
                 ORDER BY turn_id ASC",
            )
            .expect("prepare SQLite turn-commit receipt read");
        statement
            .query_map([session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .expect("read SQLite turn-commit receipts")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite turn-commit receipts")
            .into_iter()
            .map(
                |(operation, turn_commit_hash, result_json)| RuntimeTurnCommitObservation {
                    operation,
                    turn_commit_hash,
                    result: serde_json::from_str(&result_json)
                        .expect("decode SQLite turn-commit result"),
                },
            )
            .collect()
    };
    let attachment_manifest = {
        let mut statement = connection
            .prepare(
                "SELECT attachment_id, canonical_uri, intent_at_ms, committed_at_ms,
                        owner_kind, owner_id
                 FROM attachment_manifest
                 WHERE session_id = ?1
                 ORDER BY attachment_id ASC",
            )
            .expect("prepare SQLite attachment-manifest read");
        statement
            .query_map([session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })
            .expect("read SQLite attachment manifest")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite attachment manifest")
            .into_iter()
            .map(
                |(
                    attachment_id,
                    canonical_uri,
                    intent_at_epoch_ms,
                    committed_at_epoch_ms,
                    owner_kind,
                    owner_id,
                )| AttachmentManifestObservation {
                    attachment_id: AttachmentId::parse(attachment_id).expect("valid attachment id"),
                    canonical_uri,
                    intent_at_epoch_ms: intent_at_epoch_ms as u64,
                    committed: committed_at_epoch_ms.is_some(),
                    owner_kind: decode_attachment_owner_kind(owner_kind.as_deref()),
                    owner_id,
                },
            )
            .collect()
    };
    let node_anchors = {
        let mut statement = connection
            .prepare(
                "SELECT node_id, checkpoint_ref, source_session_id
                 FROM node_anchors
                 WHERE source_session_id = ?1
                 ORDER BY node_id ASC",
            )
            .expect("prepare SQLite node-anchor read");
        statement
            .query_map([session_id], |row| {
                Ok(NodeAnchorObservation {
                    node_id: row.get(0)?,
                    checkpoint_ref: BlobRef(row.get(1)?),
                    source_session_id: row.get(2)?,
                })
            })
            .expect("read SQLite node anchors")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite node anchors")
    };
    let usage_deltas = {
        let mut statement = connection
            .prepare(
                "SELECT source, model, input_tokens, output_tokens,
                        cache_read_input_tokens, cache_write_input_tokens,
                        reasoning_output_tokens
                 FROM usage_deltas
                 WHERE session_id = ?1
                 ORDER BY seq ASC",
            )
            .expect("prepare SQLite usage-delta read");
        statement
            .query_map([session_id], |row| {
                Ok(UsageDeltaObservation {
                    source: row.get(0)?,
                    model: row.get(1)?,
                    usage: TokenUsage {
                        input_tokens: row.get(2)?,
                        output_tokens: row.get(3)?,
                        cache_read_input_tokens: row.get(4)?,
                        cache_write_input_tokens: row.get(5)?,
                        reasoning_output_tokens: row.get(6)?,
                    },
                })
            })
            .expect("read SQLite usage deltas")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite usage deltas")
    };
    let session_meta = store
        .load_session_meta()
        .await
        .expect("read SQLite session metadata")
        .map(session_meta_observation);
    let session_execution_leases = {
        let mut statement = connection
            .prepare(
                "SELECT lease_owner_id, lease_owner_incarnation_id,
                        lease_owner_liveness_json, lease_executor_id, lease_token,
                        lease_fencing_token, lease_claimed_at_ms, lease_expires_at_ms,
                        lease_term_ms
                 FROM session_execution_leases
                 WHERE session_id = ?1",
            )
            .expect("prepare SQLite session-execution-lease read");
        statement
            .query_map([session_id], |row| {
                let owner_id = row.get::<_, Option<String>>(0)?;
                let incarnation_id = row.get::<_, Option<String>>(1)?;
                let liveness_json = row.get::<_, Option<String>>(2)?;
                Ok(SessionExecutionLeaseObservation {
                    owner: decode_lease_owner(owner_id, incarnation_id, liveness_json),
                    executor_id: row.get::<_, Option<String>>(3)?,
                    lease_token: row.get::<_, Option<String>>(4)?,
                    fencing_token: row.get::<_, i64>(5)? as u64,
                    claimed: row.get::<_, i64>(6)? != 0,
                    lease_term_ms: (row.get::<_, i64>(6)? != 0)
                        .then_some(row.get::<_, i64>(8)? as u64),
                })
            })
            .expect("read SQLite session-execution lease")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite session-execution lease")
    };
    let pending_turn_inputs = {
        let mut statement = connection
            .prepare(
                "SELECT input_id, enqueue_seq, state, claim_id, claim_fencing_token,
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
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                ))
            })
            .expect("read SQLite pending turn inputs")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite pending turn inputs")
            .into_iter()
            .map(
                |(
                    input_id,
                    enqueue_seq,
                    state,
                    claim_id,
                    fencing_token,
                    claim_session_lease_generation,
                )| {
                    assert_claim_id_spelling(
                        claim_id.as_deref(),
                        "tic",
                        enqueue_seq as u64,
                        fencing_token as u64,
                    );
                    PendingTurnInputObservation {
                        input_id,
                        state: TurnInputState::from_wire_str(&state)
                            .expect("decode SQLite pending-input state"),
                        claim_session_lease_generation: claim_session_lease_generation
                            .map(|generation| generation as u64),
                    }
                },
            )
            .collect()
    };
    let queued_work_batches = {
        let mut statement = connection
            .prepare(
                "SELECT enqueue_seq, batch_id, source_key, delivery_policy, work_kind,
                        authority_json, merge_key, available_at_ms, claim_id, claim_owner_id,
                        claim_owner_incarnation_id, claim_owner_liveness_json, claim_token,
                        claim_fencing_token, claim_session_lease_generation
                 FROM queued_work_batches
                 WHERE session_id = ?1
                 ORDER BY enqueue_seq ASC",
            )
            .expect("prepare SQLite queued-work batch read");
        statement
            .query_map([session_id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                    row.get(14)?,
                ))
            })
            .expect("read SQLite queued-work batches")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite queued-work batches")
    };
    let queued_work_items = {
        let mut statement = connection
            .prepare(
                "SELECT item.batch_id, item.item_index, item.payload_json
                 FROM queued_work_items AS item
                 JOIN queued_work_batches AS batch ON batch.batch_id = item.batch_id
                 WHERE batch.session_id = ?1
                 ORDER BY batch.enqueue_seq ASC, item.item_index ASC",
            )
            .expect("prepare SQLite queued-work item read");
        statement
            .query_map([session_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .expect("read SQLite queued-work items")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite queued-work items")
    };
    let queued_work =
        queued_work_observations_from_sql_rows(queued_work_batches, queued_work_items);
    let session_owned_artifact_refs = session_owned_artifact_ref_observations(
        store
            .raw_session_owned_artifact_refs_for_testing(session_id)
            .await
            .expect("read SQLite session-owned artifact refs"),
    );

    RawDurableState {
        head_revision,
        leaf_node_id,
        checkpoint,
        durable_nodes,
        runtime_turn_commits,
        attachment_manifest,
        node_anchors,
        usage_deltas,
        session_meta,
        session_execution_leases,
        pending_turn_inputs,
        queued_work,
        session_owned_artifact_refs,
    }
}

#[derive(Clone)]
enum BackendReopen {
    /// Retained-factory, same-object reopen only; this cannot establish
    /// independent cold-instance reconstruction for the in-memory backend.
    InMemory,
    Sqlite {
        root: PathBuf,
    },
    Postgres {
        database_url: String,
    },
}

struct NamedHandle {
    store: Arc<dyn RuntimePersistence>,
    meta: SessionMeta,
}

struct BackendRunner {
    name: &'static str,
    session_id: String,
    store: Option<Arc<dyn RuntimePersistence>>,
    factory: Option<Arc<dyn SessionStoreFactory>>,
    raw_reader: RawDurableReader,
    reopen: BackendReopen,
    clock: Arc<dyn Clock>,
    handles: BTreeMap<&'static str, NamedHandle>,
    lifecycle_core: Option<lash::LashCore>,
    reopened_postgres_pool: Option<PgPool>,
    first_lease: Option<lash_core::SessionExecutionLease>,
    successor_lease: Option<lash_core::SessionExecutionLease>,
    stale_turn_input_claim: Option<TurnInputClaim>,
    queued_work_claim: Option<QueuedWorkClaim>,
    current_frame_node_id: Option<lash_core::FrameNodeId>,
    current_leaf_node_id: Option<String>,
    checkpoint_component_refs: Option<CheckpointComponentRefs>,
    expected_execution_state: Option<Vec<u8>>,
}

impl BackendRunner {
    fn store(&self) -> Arc<dyn RuntimePersistence> {
        Arc::clone(
            self.store
                .as_ref()
                .expect("backend runner is attached to a store"),
        )
    }

    fn factory(&self) -> Arc<dyn SessionStoreFactory> {
        Arc::clone(
            self.factory
                .as_ref()
                .expect("backend runner is attached to a factory"),
        )
    }

    fn create_request(&self) -> SessionStoreCreateRequest {
        SessionStoreCreateRequest {
            session_id: self.session_id.clone(),
            relation: SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        }
    }

    fn assert_session_deleted(&self, error: &StoreError, operation: &str) {
        assert!(
            matches!(
                error,
                StoreError::SessionDeleted { session_id } if session_id == &self.session_id
            ),
            "{} {operation} must return typed SessionDeleted for `{}`, got: {error}",
            self.name,
            self.session_id
        );
    }

    fn build_lifecycle_core(&self) -> lash::LashCore {
        let transport = Arc::new(
            lash_sim::ScriptedLlmHttpTransport::from_scripts([])
                .expect("empty provider script queue"),
        );
        let (provider, model, _) = lash_sim::runtime_providers::runtime_provider_components(
            lash_sim::runtime_providers::OPENAI_COMPATIBLE,
            &transport,
        )
        .expect("build differential lifecycle provider");
        lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
            .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
            .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .process_env_store(Arc::new(
                lash::persistence::InMemoryProcessExecutionEnvStore::new(),
            ))
            .store_factory(self.factory())
            .provider(provider)
            .model(model)
            .clock(Arc::clone(&self.clock))
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "cross-backend-differential-test",
                "cross-backend-differential-test-boot",
            ))
            .expect("build differential lifecycle core")
    }
    async fn close_reopened_postgres_pool(&mut self) {
        if let Some(pool) = self.reopened_postgres_pool.take() {
            pool.close().await;
        }
    }

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
                checkpoint,
                usage,
                adopt_attachment,
                ..
            } => {
                let commit = runtime_commit(
                    &self.session_id,
                    *expected_head_revision,
                    graph,
                    *turn_commit,
                    self.current_frame_node_id.clone(),
                    checkpoint_from_spec(*checkpoint, self.checkpoint_component_refs.as_ref()),
                    (*usage)
                        .then(differential_usage_delta)
                        .into_iter()
                        .collect(),
                    (*adopt_attachment)
                        .then(differential_attachment_id)
                        .into_iter()
                        .collect(),
                );
                let next_frame_node_id = commit.current_frame_node_id.clone();
                let next_leaf_node_id = commit.graph.leaf_node_id.clone();
                let result = self.store().commit_runtime_state(commit).await;
                match result {
                    Ok(result) => {
                        self.current_frame_node_id = next_frame_node_id;
                        self.current_leaf_node_id = next_leaf_node_id;
                        if matches!(*checkpoint, CheckpointSpec::Bodies) {
                            self.checkpoint_component_refs = Some(CheckpointComponentRefs {
                                components: result.manifest.components.clone(),
                            });
                        }
                        match checkpoint {
                            CheckpointSpec::Bodies => {
                                self.expected_execution_state = checkpoint_bodies()
                                    .component_body(
                                        lash_core::store::EXECUTION_STATE_CHECKPOINT_COMPONENT,
                                    )
                                    .map(ToOwned::to_owned);
                            }
                            CheckpointSpec::PriorRefs => {}
                            CheckpointSpec::Empty | CheckpointSpec::ClearedComponents => {
                                self.expected_execution_state = None;
                            }
                            CheckpointSpec::MissingExecutionStateRef => {}
                        }
                        Ok(Some(result.into()))
                    }
                    Err(error) => Err(error),
                }
            }
            StoreOperation::RecordAttachmentIntent => {
                let operation = lash_core::store::OperationId::turn(
                    &self.session_id,
                    "attachment-adoption",
                    "differential",
                )
                .storage_key()?;
                self.store().record_intent(AttachmentIntent {
                    attachment_id: differential_attachment_id(),
                    session_id: self.session_id.clone(),
                    canonical_uri: "lash-attachment://sha256/differential-attachment".to_string(),
                    intent_at_epoch_ms: 1_000,
                    owner_kind: Some(AttachmentOwnerKind::Turn),
                    owner_id: Some(operation),
                })?;
                Ok(None)
            }
            StoreOperation::PinLeaf => {
                self.factory()
                    .pin(
                        self.current_leaf_node_id
                            .as_deref()
                            .expect("generated sequence committed a leaf before pin"),
                    )
                    .await?;
                Ok(None)
            }
            StoreOperation::ForkAtLeaf => {
                let node_id = self
                    .current_leaf_node_id
                    .clone()
                    .expect("generated sequence committed a leaf before fork");
                self.factory()
                    .fork_at(&ForkSessionRequest {
                        session_id: format!("{}:fork", self.session_id),
                        node_id: node_id.clone(),
                        relation: SessionRelation::Fork {
                            source_session_id: self.session_id.clone(),
                            source_node_id: node_id,
                            observer_inheritance: lash_core::ObserverInheritance::None,
                            pending_observer_process_ids: Vec::new(),
                        },
                        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
                    })
                    .await?;
                Ok(None)
            }
            StoreOperation::ForkAtExistingTarget
            | StoreOperation::ForkAtForeignLineage
            | StoreOperation::Rewind => self.apply_fork_operation(operation).await,
            StoreOperation::UnpinLeaf => {
                self.factory()
                    .unpin(
                        self.current_leaf_node_id
                            .as_deref()
                            .expect("generated sequence committed a leaf before unpin"),
                    )
                    .await?;
                Ok(None)
            }
            StoreOperation::EnqueueNextTurnInput => self
                .store()
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
            StoreOperation::EnqueueQueuedWork => self
                .store()
                .enqueue_queued_work(
                    QueuedWorkBatchDraft::new(
                        &self.session_id,
                        DeliveryPolicy::EarliestSafeBoundary,
                        vec![QueuedWorkPayload::session_command(
                            lash_core::facade_support::SessionCommand::RefreshToolCatalog {
                                reason: "cross-backend delete observability".to_string(),
                            },
                        )],
                    )
                    .with_source_key("cross-backend-delete-observability"),
                )
                .await
                .map(|_| None),
            StoreOperation::EnqueueClaimableQueuedWork => self
                .store()
                .enqueue_queued_work(
                    QueuedWorkBatchDraft::new(
                        &self.session_id,
                        DeliveryPolicy::AfterCurrentTurnCommit,
                        vec![QueuedWorkPayload::agent_frame_task(
                            lash_core::facade_support::frame_node_id(
                                &self.session_id,
                                "differential-frame",
                            ),
                            "exercise queued-work claim state",
                            None,
                        )],
                    )
                    .with_source_key("cross-backend-claim-observability")
                    .with_available_at_ms(777)
                    .with_merge_key("cross-backend-claim-observability"),
                )
                .await
                .map(|_| None),
            StoreOperation::AcquireSessionLease { slot, owner } => {
                let owner = LeaseOwnerIdentity::opaque(*owner, format!("{owner}:incarnation"));
                // The executor and the claim nonce are caller-supplied bytes, so
                // every backend must persist and return exactly these. Deriving
                // them from the generated operation keeps them identical across
                // the compared backends while staying distinct per slot, so a
                // live holder is still observed as Busy rather than reentered.
                let executor_id = format!("{}:{slot:?}-executor", owner.owner_id);
                let claim_nonce =
                    LeaseClaimNonce::for_testing(format!("{}:{slot:?}-token", owner.owner_id));
                let lease = self
                    .store()
                    .try_claim_session_execution_lease_with_token(
                        &self.session_id,
                        &owner,
                        &executor_id,
                        &claim_nonce,
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
                let store = self.store();
                let mut claim = store
                    .claim_next_turn_inputs(&self.session_id, &lease.fence(), &owner, 1)
                    .await?
                    .ok_or_else(|| {
                        StoreError::Backend(format!(
                            "{} did not return the generated turn-input claim",
                            self.name
                        ))
                    })?;
                claim.record_initial_turn_application(
                    &lash_core::TurnId::from("claim-turn"),
                    "claim-message",
                );
                self.stale_turn_input_claim = Some(claim);
                Ok(None)
            }
            StoreOperation::ClaimQueuedWork { lease } => {
                let lease = self.lease(*lease);
                let owner = lease.owner.clone();
                let claim = self
                    .store()
                    .claim_ready_queued_work(
                        &self.session_id,
                        &lease.fence(),
                        &owner,
                        QueuedWorkClaimBoundary::Idle,
                        lash_core::testing::queued_work_claim_policy(1),
                    )
                    .await?
                    .claim()
                    .ok_or_else(|| {
                        StoreError::Backend(format!(
                            "{} did not return the generated queued-work claim",
                            self.name
                        ))
                    })?;
                self.queued_work_claim = Some(claim);
                Ok(None)
            }
            StoreOperation::AbandonQueuedWorkClaim => {
                let claim = self
                    .queued_work_claim
                    .as_ref()
                    .expect("generated sequence claimed queued work before abandonment")
                    .clone();
                self.store()
                    .abandon_queued_work_claim(&claim)
                    .await
                    .map(|_| None)
            }
            StoreOperation::ReleaseSessionLease { lease } => self
                .store()
                .release_session_execution_lease(&self.lease(*lease).completion())
                .await
                .map(|_| None),
            StoreOperation::CommitStaleTurnInputClaim {
                expected_head_revision,
            } => {
                let claim = self
                    .stale_turn_input_claim
                    .as_ref()
                    .expect("generated sequence claimed input before stale settlement");
                let graph = GraphSpec {
                    nodes: vec![NodeSpec::new("stale-claim-node", None, "stale-claim")],
                    leaf_node_id: Some("stale-claim-node"),
                };
                self.store()
                    .commit_runtime_state(
                        runtime_commit(
                            &self.session_id,
                            *expected_head_revision,
                            &graph,
                            None,
                            self.current_frame_node_id.clone(),
                            HydratedSessionCheckpoint::default(),
                            Vec::new(),
                            Vec::new(),
                        )
                        .completing_turn_input_claim(claim.completion()),
                    )
                    .await
                    .map(|result| Some(result.into()))
            }
            StoreOperation::ColdReopenSession => {
                let request = self.create_request();
                let reopened = match self.reopen.clone() {
                    // This is intentionally a retained-factory, same-object
                    // reopen. Only the SQL legs prove independent cold-instance
                    // reconstruction.
                    BackendReopen::InMemory => self
                        .factory()
                        .open_existing_store(&request)
                        .await
                        .map_err(StoreError::Backend)?
                        .expect("in-memory retained factory must still expose the live session"),
                    BackendReopen::Sqlite { root } => {
                        self.store.take();
                        self.factory.take();
                        self.raw_reader.detach_store();

                        let concrete_factory = Arc::new(
                            lash_sqlite_store::SqliteSessionStoreFactory::new(root.clone())
                                .with_clock(Arc::clone(&self.clock)),
                        );
                        let reopened = concrete_factory
                            .open_existing_store(&request)
                            .await
                            .map_err(StoreError::Backend)?
                            .expect("SQLite session must survive an independent reopen");
                        let path = concrete_factory.catalog_path();
                        self.factory = Some(concrete_factory as Arc<dyn SessionStoreFactory>);
                        self.raw_reader = RawDurableReader::Sqlite {
                            path,
                            session_id: self.session_id.clone(),
                            store: Some(Arc::clone(&reopened)),
                        };
                        reopened
                    }
                    BackendReopen::Postgres { database_url } => {
                        self.store.take();
                        self.factory.take();
                        self.raw_reader.detach_store();

                        let storage = PostgresStorage::connect(&database_url)
                            .await
                            .expect("connect independent Postgres storage");
                        let pool = storage.pool().clone();
                        let concrete_factory = Arc::new(
                            storage
                                .session_store_factory()
                                .with_clock(Arc::clone(&self.clock)),
                        );
                        let reopened = concrete_factory
                            .open_existing_store(&request)
                            .await
                            .map_err(StoreError::Backend)?
                            .expect("Postgres session must survive an independent reopen");
                        self.factory = Some(concrete_factory as Arc<dyn SessionStoreFactory>);
                        self.raw_reader = RawDurableReader::Postgres {
                            pool: pool.clone(),
                            session_id: self.session_id.clone(),
                            store: Some(Arc::clone(&reopened)),
                        };
                        self.reopened_postgres_pool = Some(pool);
                        reopened
                    }
                };
                self.store = Some(Arc::clone(&reopened));

                let loaded = reopened
                    .load_session()
                    .await?
                    .expect("cold-reopened session must have durable state");
                let checkpoint = loaded
                    .checkpoint
                    .expect("cold-reopened session must hydrate its checkpoint");
                let expected = checkpoint_bodies();
                assert_eq!(
                    checkpoint
                        .decode_component::<ToolState>(
                            lash_core::store::TOOL_STATE_CHECKPOINT_COMPONENT,
                        )?
                        .as_ref()
                        .map(ToolState::generation),
                    expected
                        .decode_component::<ToolState>(
                            lash_core::store::TOOL_STATE_CHECKPOINT_COMPONENT,
                        )?
                        .as_ref()
                        .map(ToolState::generation),
                    "{} cold reopen must rehydrate the tool-state body",
                    self.name
                );
                assert_eq!(
                    checkpoint
                        .decode_component::<PluginSessionSnapshot>(
                            lash_core::store::PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT,
                        )?
                        .as_ref()
                        .map(|snapshot| serde_json::to_value(snapshot).expect("encode snapshot")),
                    expected
                        .decode_component::<PluginSessionSnapshot>(
                            lash_core::store::PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT,
                        )?
                        .as_ref()
                        .map(|snapshot| serde_json::to_value(snapshot).expect("encode snapshot")),
                    "{} cold reopen must rehydrate the plugin-snapshot body",
                    self.name
                );
                assert_eq!(
                    checkpoint
                        .component_body(lash_core::store::EXECUTION_STATE_CHECKPOINT_COMPONENT),
                    self.expected_execution_state.as_deref(),
                    "{} cold reopen must rehydrate the execution-state body",
                    self.name
                );
                Ok(None)
            }
            StoreOperation::DeleteSession => {
                let core = self.build_lifecycle_core();
                let scope = core
                    .session_delete_scope(&self.session_id)
                    .await
                    .expect("materialized session must produce a delete scope");
                let effect_host = core.effect_host();
                let scoped = effect_host
                    .scoped_static(scope)
                    .expect("scope the differential delete")
                    .expect("inline effect host must provide a static delete scope");
                core.delete_session(&self.session_id, scoped)
                    .await
                    .expect("delete the materialized session through LashCore");
                self.lifecycle_core = Some(core);
                Ok(None)
            }
            StoreOperation::AttemptAdmission => {
                let core = self
                    .lifecycle_core
                    .as_ref()
                    .expect("generated sequence deletes before attempting admission");
                let error = match core.session(&self.session_id).open().await {
                    Ok(_) => panic!(
                        "{} admitted deleted session `{}`",
                        self.name, self.session_id
                    ),
                    Err(lash::EmbedError::Store(error)) => error,
                    Err(error) => panic!(
                        "{} admission returned an untyped error for deleted session `{}`: {error}",
                        self.name, self.session_id
                    ),
                };
                self.assert_session_deleted(&error, "admission");
                assert!(
                    self.store()
                        .list_turn_input_applications(&self.session_id)
                        .await?
                        .is_empty(),
                    "{} persisted a runtime turn commit while refusing deleted-session admission",
                    self.name
                );
                assert!(
                    self.store()
                        .list_pending_turn_inputs(&self.session_id)
                        .await?
                        .is_empty(),
                    "{} enqueued pending turn input while refusing deleted-session admission",
                    self.name
                );
                Err(error)
            }
            StoreOperation::CreateHandle { handle_alias } => {
                let request = self.create_request();
                let store = self
                    .factory()
                    .open_existing_store(&request)
                    .await
                    .map_err(StoreError::Backend)?
                    .expect("create handle requires a live materialized session");
                let meta = store
                    .load_session_meta()
                    .await?
                    .expect("live handle must retain session metadata");
                assert!(
                    self.handles
                        .insert(*handle_alias, NamedHandle { store, meta })
                        .is_none(),
                    "{} reused handle alias `{handle_alias}`",
                    self.name
                );
                Ok(None)
            }
            StoreOperation::DeleteSessionThroughFactory => {
                let store = self.store();
                let has_session_artifact_refs = store
                    .seed_session_trigger_manifest_ref_for_testing(&self.session_id)
                    .await?;
                if has_session_artifact_refs {
                    assert_eq!(
                        store
                            .raw_session_owned_artifact_refs_for_testing(&self.session_id)
                            .await?,
                        vec![(
                            "lashlang_trigger_manifest".to_string(),
                            TriggerOwnerScope::session(&self.session_id).namespace(),
                        )],
                        "{} did not seed the exact session-owned trigger-manifest ref",
                        self.name
                    );
                }
                self.factory()
                    .delete_session(&self.session_id)
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                Ok(None)
            }
            StoreOperation::AdmitOnHandle { handle_alias } => {
                let request = self.create_request();
                let handle = self
                    .handles
                    .get(handle_alias)
                    .expect("generated sequence creates handle before admission");
                let error = handle
                    .store
                    .admit_and_bind_session(&lash_core::SessionBinding::from_create_request(
                        &request,
                    ))
                    .await
                    .expect_err("stale handle admission must be fenced");
                self.assert_session_deleted(&error, "stale-handle admission");
                Err(error)
            }
            StoreOperation::SaveMetaOnHandle { handle_alias } => {
                let handle = self
                    .handles
                    .get(handle_alias)
                    .expect("generated sequence creates handle before metadata save");
                let error = handle
                    .store
                    .save_session_meta(handle.meta.clone())
                    .await
                    .expect_err("stale handle metadata save must be fenced");
                self.assert_session_deleted(&error, "stale-handle metadata save");
                Err(error)
            }
            StoreOperation::CommitOnHandle { handle_alias } => {
                let handle = self
                    .handles
                    .get(handle_alias)
                    .expect("generated sequence creates handle before commit");
                let state = RuntimeSessionState {
                    session_id: self.session_id.clone(),
                    ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                        lash_core::TurnBudget::Unbounded,
                    ))
                };
                let error = handle
                    .store
                    .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
                    .await
                    .expect_err("stale handle commit must be fenced");
                self.assert_session_deleted(&error, "stale-handle commit");
                Err(error)
            }
            StoreOperation::ObserveSessionAbsent => {
                let request = self.create_request();
                assert!(
                    self.factory()
                        .open_existing_store(&request)
                        .await
                        .map_err(StoreError::Backend)?
                        .is_none(),
                    "{} stale writes resurrected deleted session `{}`",
                    self.name,
                    self.session_id
                );
                Ok(None)
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
        let freshness_head = match self.store().load_session_head_meta().await {
            Ok(Some(head)) => FreshnessHeadObservation::Present {
                head_revision: head.head_revision,
                leaf_node_id: head.leaf_node_id,
                checkpoint_ref: head.checkpoint_ref,
            },
            Ok(None) => FreshnessHeadObservation::Missing,
            Err(error) => {
                FreshnessHeadObservation::Error(normalized_store_error(self.name, &error))
            }
        };
        StepObservation {
            store_error,
            runtime_commit_result,
            freshness_head,
            durable_state: self.raw_reader.observe().await,
        }
    }
}

fn normalized_store_error(_backend: &str, error: &StoreError) -> String {
    match error {
        StoreError::ExecutionStateCaptureFailed { message } => {
            format!("ExecutionStateCaptureFailed:{message}")
        }
        _ => error.variant_name().to_string(),
    }
}

async fn assert_storage_failure_mappings_agree(sqlite_root: &Path, postgres: &PostgresStorage) {
    let create_request = SessionStoreCreateRequest {
        session_id: format!("fig-1242-storage-failure:{}", run_nonce()),
        relation: SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };

    let sqlite_factory = lash_sqlite_store::SqliteSessionStoreFactory::new(
        sqlite_root.join("storage-failure-mapping"),
    );
    let sqlite_store = sqlite_factory
        .create_store(&create_request)
        .await
        .expect("create SQLite storage-failure differential store");
    let sqlite_connection = rusqlite::Connection::open(sqlite_factory.catalog_path())
        .expect("open SQLite storage-failure fixture");
    sqlite_connection
        .execute("DROP TABLE session_meta", [])
        .expect("break SQLite storage-failure fixture");
    let sqlite_error = sqlite_store
        .load_session_meta()
        .await
        .expect_err("broken SQLite catalog must fail");

    let postgres_factory = postgres.session_store_factory();
    postgres.pool().close().await;
    let postgres_error = match postgres_factory.create_store(&create_request).await {
        Ok(_) => panic!("closed PostgreSQL pool must fail"),
        Err(error) => error,
    };

    assert_eq!(
        normalized_store_error("sqlite", &sqlite_error),
        normalized_store_error("postgres", &postgres_error),
        "the same substrate-failure class must retain one typed error surface; \
         sqlite={sqlite_error:?}, postgres={postgres_error:?}"
    );
}

#[test]
fn normalized_store_errors_compare_typedness_and_variant_not_prose() {
    let sqlite_storage_failure = StoreError::StorageFailure {
        backend: "sqlite",
        message: "disk I/O error".to_string(),
    };
    let postgres_storage_failure = StoreError::StorageFailure {
        backend: "postgres",
        message: "connection closed".to_string(),
    };
    let postgres_untyped = StoreError::Backend("connection closed".to_string());
    let postgres_corrupt = StoreError::StoredDataCorrupt {
        record_kind: "session metadata",
        message: "invalid JSON".to_string(),
    };
    let first_capture_failure = StoreError::ExecutionStateCaptureFailed {
        message: "checkpoint encoder failed".to_string(),
    };
    let second_capture_failure = StoreError::ExecutionStateCaptureFailed {
        message: "plugin snapshot failed".to_string(),
    };

    assert_eq!(
        normalized_store_error("sqlite", &sqlite_storage_failure),
        normalized_store_error("postgres", &postgres_storage_failure),
        "backend-specific prose inside one typed variant is not contract-visible"
    );
    assert_ne!(
        normalized_store_error("postgres", &postgres_storage_failure),
        normalized_store_error("postgres", &postgres_untyped),
        "typed and untyped failures must remain distinct"
    );
    assert_ne!(
        normalized_store_error("postgres", &postgres_storage_failure),
        normalized_store_error("postgres", &postgres_corrupt),
        "different typed variants must remain distinct"
    );
    assert_ne!(
        normalized_store_error("in-memory", &first_capture_failure),
        normalized_store_error("in-memory", &second_capture_failure),
        "the existing execution-state capture diagnostic comparison remains exact"
    );
}

#[derive(Debug)]
struct DifferentialClock;

#[async_trait::async_trait]
impl Clock for DifferentialClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_ms(&self) -> u64 {
        1_000
    }

    fn timestamp_rfc3339(&self) -> String {
        "2026-07-26T00:00:00+00:00".to_string()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-07-26T00:00:00+00:00")
            .expect("valid differential timestamp")
            .with_timezone(&chrono::Utc)
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

async fn runners_for_case(
    case: CaseName,
    sqlite_root: &Path,
    postgres: &PostgresStorage,
    postgres_database_url: &str,
    run_nonce: &str,
) -> Vec<BackendRunner> {
    runners_for_case_with_clock(
        case,
        sqlite_root,
        postgres,
        postgres_database_url,
        run_nonce,
        Arc::new(DifferentialClock),
    )
    .await
}

async fn runners_for_case_with_clock(
    case: CaseName,
    sqlite_root: &Path,
    postgres: &PostgresStorage,
    postgres_database_url: &str,
    run_nonce: &str,
    clock: Arc<dyn Clock>,
) -> Vec<BackendRunner> {
    let session_id = format!("fig-778-{run_nonce}-{}", case.as_str());
    let create_request = SessionStoreCreateRequest {
        session_id: session_id.clone(),
        relation: SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let expected_meta = SessionMeta {
        session_id: session_id.clone(),
        relation: SessionRelation::Child {
            parent_session_id: format!("fig-778-{run_nonce}-parent"),
            caused_by: None,
        },
    };

    let memory_factory = Arc::new(InMemorySessionStoreFactory::with_clock(Arc::clone(&clock)));
    let memory_store = memory_factory
        .create_store(&create_request)
        .await
        .expect("create in-memory differential store");
    let memory = memory_factory
        .raw_store_for_testing(&session_id)
        .expect("factory retains concrete in-memory store");
    memory.replace_session_meta_for_testing(expected_meta.clone());
    let memory_factory_dyn = Arc::clone(&memory_factory) as Arc<dyn SessionStoreFactory>;

    let sqlite_case_root = sqlite_root.join(case.as_str());
    let sqlite_factory = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(sqlite_case_root.clone())
            .with_clock(Arc::clone(&clock)),
    );
    let sqlite_path = sqlite_factory.catalog_path();
    let sqlite_store = sqlite_factory
        .create_store(&create_request)
        .await
        .expect("create SQLite differential store");
    sqlite_store
        .save_session_meta(expected_meta.clone())
        .await
        .expect("install deterministic SQLite session metadata");
    let sqlite_factory_dyn = Arc::clone(&sqlite_factory) as Arc<dyn SessionStoreFactory>;

    let postgres_factory = Arc::new(
        postgres
            .session_store_factory()
            .with_clock(Arc::clone(&clock)),
    );
    let postgres_store = postgres_factory
        .create_store(&create_request)
        .await
        .expect("create Postgres differential store");
    postgres_store
        .save_session_meta(expected_meta.clone())
        .await
        .expect("install deterministic Postgres session metadata");
    let postgres_factory_dyn = Arc::clone(&postgres_factory) as Arc<dyn SessionStoreFactory>;

    vec![
        BackendRunner {
            name: "in-memory",
            session_id: session_id.clone(),
            store: Some(memory_store),
            factory: Some(memory_factory_dyn),
            raw_reader: RawDurableReader::InMemory {
                store: memory,
                factory: memory_factory,
                session_id: session_id.clone(),
            },
            reopen: BackendReopen::InMemory,
            clock: Arc::clone(&clock),
            handles: BTreeMap::new(),
            lifecycle_core: None,
            reopened_postgres_pool: None,
            first_lease: None,
            successor_lease: None,
            stale_turn_input_claim: None,
            queued_work_claim: None,
            current_frame_node_id: None,
            current_leaf_node_id: None,
            checkpoint_component_refs: None,
            expected_execution_state: None,
        },
        BackendRunner {
            name: "sqlite",
            session_id: session_id.clone(),
            store: Some(Arc::clone(&sqlite_store)),
            factory: Some(sqlite_factory_dyn),
            raw_reader: RawDurableReader::Sqlite {
                path: sqlite_path,
                session_id: session_id.clone(),
                store: Some(sqlite_store),
            },
            reopen: BackendReopen::Sqlite {
                root: sqlite_case_root,
            },
            clock: Arc::clone(&clock),
            handles: BTreeMap::new(),
            lifecycle_core: None,
            reopened_postgres_pool: None,
            first_lease: None,
            successor_lease: None,
            stale_turn_input_claim: None,
            queued_work_claim: None,
            current_frame_node_id: None,
            current_leaf_node_id: None,
            checkpoint_component_refs: None,
            expected_execution_state: None,
        },
        BackendRunner {
            name: "postgres",
            session_id: session_id.clone(),
            store: Some(Arc::clone(&postgres_store)),
            factory: Some(postgres_factory_dyn),
            raw_reader: RawDurableReader::Postgres {
                pool: postgres.pool().clone(),
                session_id,
                store: Some(postgres_store),
            },
            reopen: BackendReopen::Postgres {
                database_url: postgres_database_url.to_string(),
            },
            clock,
            handles: BTreeMap::new(),
            lifecycle_core: None,
            reopened_postgres_pool: None,
            first_lease: None,
            successor_lease: None,
            stale_turn_input_claim: None,
            queued_work_claim: None,
            current_frame_node_id: None,
            current_leaf_node_id: None,
            checkpoint_component_refs: None,
            expected_execution_state: None,
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
    assert_eq!(cases.len(), 18);
    assert!(cases.iter().all(|case| !case.operations.is_empty()));
    assert_eq!(
        cases
            .iter()
            .map(|case| case.name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "duplicate_node_id_within_one_append",
            "duplicate_node_id_across_two_commits",
            "append_duplicate_node_id_after_append_seed",
            "nodeless_commit_cannot_move_leaf",
            "stale_expected_head_revision",
            "identical_and_mutated_turn_commit_replay",
            "settle_claim_after_session_lease_handoff_before_reclaim",
            "checkpoint_bodies_then_ref_only",
            "checkpoint_bodies_then_cleared",
            "missing_checkpoint_component_ref",
            "fork_fence_exists_precedes_other_fences",
            "pin_fork_unpin_moves_node_anchor",
            "fork_accepts_foreign_lineage",
            "rewind_fork_delete_source_refork",
            "attachment_intent_adopted_by_commit",
            "queued_work_claim_abandon_preserves_fencing_token",
            "delete_then_attempt_admission",
            "stale_handle_after_delete",
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "compares three durable backends; requires Postgres (`just push-gate`, or LASH_POSTGRES_DATABASE_URL with `cargo test -- --include-ignored` / `cargo nextest run --run-ignored all`)"]
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
                "SKIPPED cross-backend store differential; compared_backends=[]; \
                 required_backends=[in-memory,sqlite,postgres]; \
                 reason=LASH_POSTGRES_DATABASE_URL is not set"
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
                "SKIPPED cross-backend store differential; compared_backends=[]; \
                 required_backends=[in-memory,sqlite,postgres]; \
                 reason=LASH_POSTGRES_DATABASE_URL is not set"
            );
            return;
        }
    };
    // `push-gate` runs workspace tests through nextest, so this test is a
    // separate process from the Postgres conformance tests. Hold their common
    // session-level advisory lock for the entire differential: both suites use
    // the configured database as disposable test state.
    let mut database_lock = PgConnection::connect(&database_url)
        .await
        .expect("connect Postgres differential advisory lock");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SHARED_DATABASE_LOCK_KEY)
        .execute(&mut database_lock)
        .await
        .expect("acquire Postgres differential advisory lock");
    let postgres = PostgresStorage::connect(&database_url)
        .await
        .expect("connect required Postgres differential backend");
    let sqlite_root = tempfile::tempdir().expect("create SQLite differential root");
    verify_independent_session_meta_layout(sqlite_root.path(), &postgres).await;
    let run_nonce = run_nonce();
    let mut divergences = String::new();
    eprintln!(
        "RUNNING cross-backend store differential; \
         compared_backends=[in-memory,sqlite,postgres]; cases={}",
        generated_cases().len()
    );

    for case in generated_cases() {
        let mut runners = runners_for_case(
            case.name,
            sqlite_root.path(),
            &postgres,
            &database_url,
            &run_nonce,
        )
        .await;
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
        for runner in &mut runners {
            runner.close_reopened_postgres_pool().await;
        }
    }

    assert!(
        divergences.is_empty(),
        "cross-backend durable state diverged:{divergences}"
    );
    assert_storage_failure_mappings_agree(sqlite_root.path(), &postgres).await;
    eprintln!(
        "PASSED cross-backend store differential; \
         compared_backends=[in-memory,sqlite,postgres]"
    );
}
