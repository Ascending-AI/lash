//! The generator is deliberately table-driven for its first landing. These
//! malformed shapes are individually named, reviewable, and shrink no further
//! than the short sequences below. Add a case by extending `generated_cases`;
//! the runner automatically applies every operation to a SQLite memory
//! backend, a SQLite file backend and Postgres and compares the observation
//! after each step.
//!
//! Agreement is not correctness: a differential cannot detect a defect shared
//! by all backends. The FIG-641 case below is the live example of that limit.
//!
//! Nodes are never observed through `load_session`: that constructs a
//! `SessionGraph` read model whose id indexes can hide duplicate durable rows.

use lash_sansio::SessionId;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lash_core::facade_support::ToolStateFacadeOps;
use lash_core::runtime::{QueuedWorkBatchDraft, QueuedWorkClaim, QueuedWorkClaimBoundary};
use lash_core::store::{ConformancePersistence, ConformanceSessionStoreFactory};
use lash_core::store::{GraphAppend, RuntimeCommitReceipt};
use lash_core::{
    AttachmentId, AttachmentOwnerKind, BlobRef, Clock, DeliveryPolicy, EffectAddress,
    ExecutionScope, ForkSessionRequest, HydratedSessionCheckpoint, LeaseClaimNonce,
    LeaseOwnerIdentity, PendingTurnInputDraft, PluginNamespaceState, PluginState,
    ProcessEventLog as _, ProcessRegistrar as _, ProtocolEvent, QueuedWorkAuthority,
    QueuedWorkKind, RuntimeCommit, RuntimeSessionState, RuntimeTurnCommitStamp,
    SessionHistoryRecord, SessionMeta, SessionNodePayload, SessionNodeRecord, SessionRelation,
    SessionStoreCreateRequest, SessionStoreFactory, StoreError, TokenLedgerEntry, TokenUsage,
    ToolState, TurnInput, TurnInputApplication, TurnInputClaim, TurnInputIngress,
    TurnInputStateKind,
};
use lash_postgres_store::PostgresStorage;
use rusqlite::OptionalExtension;
use sqlx::{Connection, PgConnection, PgPool};

#[path = "cross_backend_store_differential/attachment_seeding.rs"]
mod attachment_seeding;
#[path = "cross_backend_store_differential/checkpoint_cases.rs"]
mod checkpoint_cases;
#[path = "cross_backend_store_differential/claim_cases.rs"]
mod claim_cases;
#[path = "cross_backend_store_differential/coalesced_batch_oracles.rs"]
mod coalesced_batch_oracles;
#[path = "cross_backend_store_differential/corrupt_input_cases.rs"]
mod corrupt_input_cases;
#[path = "cross_backend_store_differential/fork_cases.rs"]
mod fork_cases;
#[path = "cross_backend_store_differential/generated_surface.rs"]
mod generated_surface;
#[path = "cross_backend_store_differential/observations.rs"]
mod observations;
#[path = "cross_backend_store_differential/plugin_state_case.rs"]
mod plugin_state_case;
#[path = "cross_backend_store_differential/process_event_pages.rs"]
mod process_event_pages;
#[path = "cross_backend_store_differential/raw_durable_reader.rs"]
mod raw_durable_reader;
#[path = "cross_backend_store_differential/residue.rs"]
mod residue;
#[path = "cross_backend_store_differential/session_meta_layout.rs"]
mod session_meta_layout;
#[path = "cross_backend_store_differential/surface_sweep.rs"]
mod surface_sweep;
#[path = "cross_backend_store_differential/trait_surface_gate.rs"]
mod trait_surface_gate;
use corrupt_input_cases::CorruptTarget;
use observations::*;
use residue::*;
use session_meta_layout::verify_independent_session_meta_layout;
use surface_sweep::{SurfaceMethod, SurfaceScratch};

const SESSION_LEASE_TTL_MS: u64 = 60_000;
// "LASH_PGT" encoded as a positive i64. This must match the shared-database
// advisory lock used by lash-postgres-store's integration-test harness.
const SHARED_DATABASE_LOCK_KEY: i64 = 0x4c41_5348_5f50_4754;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaseName {
    PluginStateSeam,
    DuplicateWithinAppend,
    DuplicateAcrossCommits,
    AppendDuplicateAfterAppendSeed,
    NodelessLeafMove,
    StaleExpectedHeadRevision,
    IdenticalAndMutatedTurnCommitReplay,
    SettleClaimBeforeSuccessorReclaim,
    TurnInputClaimSupersededAfterReclaim,
    QueuedWorkClaimSupersededAfterReclaim,
    SameGenerationExactClaimDeferral,
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
    StoreSurfaceSweep,
    PendingFollowOnRaise,
    TurnBoundClaimBindAndReclaim,
    RootClaimReplay,
    RefusedSurfaceOnDeletedSession,
    SessionCloseLedger,
    RootCancelLedger,
    RootForkLedger,
    CorruptGraphNodeRefusals,
    CorruptPendingTurnInputRefusals,
    CorruptQueuedWorkRefusals,
    CorruptPriorCheckpointRefusals,
}

/// How a case's observations are compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ComparisonMode {
    /// Every backend, compared through the decoded durable digest.
    Decoded,
    /// Every backend, compared without decoding any row: the decoded digest
    /// cannot read a deliberately undecodable record. See
    /// `corrupt_input_cases.rs`.
    RawOnly,
}

impl CaseName {
    fn as_str(self) -> &'static str {
        match self {
            Self::PluginStateSeam => "plugin_state_boundary_fork_rebuild",
            Self::DuplicateWithinAppend => "duplicate_node_id_within_one_append",
            Self::DuplicateAcrossCommits => "duplicate_node_id_across_two_commits",
            Self::AppendDuplicateAfterAppendSeed => "append_duplicate_node_id_after_append_seed",
            Self::NodelessLeafMove => "nodeless_commit_cannot_move_leaf",
            Self::StaleExpectedHeadRevision => "stale_expected_head_revision",
            Self::IdenticalAndMutatedTurnCommitReplay => "identical_and_mutated_turn_commit_replay",
            Self::SettleClaimBeforeSuccessorReclaim => {
                "settle_claim_after_session_lease_handoff_before_reclaim"
            }
            Self::TurnInputClaimSupersededAfterReclaim => {
                "turn_input_claim_superseded_after_successor_reclaim"
            }
            Self::QueuedWorkClaimSupersededAfterReclaim => {
                "queued_work_claim_superseded_after_successor_reclaim"
            }
            Self::SameGenerationExactClaimDeferral => "same_generation_exact_claim_defers",
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
            Self::StoreSurfaceSweep => "store_surface_sweep",
            Self::PendingFollowOnRaise => "pending_follow_on_raise_and_clear",
            Self::TurnBoundClaimBindAndReclaim => {
                "turn_bound_claim_binds_defers_and_reclaims_across_generations"
            }
            Self::RootClaimReplay => "root_claim_replays_exact_result_after_lease_handoff",
            Self::RefusedSurfaceOnDeletedSession => {
                "refused_surface_on_deleted_session_leaves_no_residue"
            }
            Self::RootCancelLedger => "root_cancel_ledger",
            Self::RootForkLedger => "root_fork_ledger",
            Self::SessionCloseLedger => "session_close_ledger_closes_roots_and_tracks_its_intent",
            Self::CorruptGraphNodeRefusals => "corrupt_graph_node_refuses_every_reader",
            Self::CorruptPendingTurnInputRefusals => {
                "corrupt_pending_turn_input_refuses_list_and_claim"
            }
            Self::CorruptQueuedWorkRefusals => "corrupt_queued_work_refuses_list_and_claim",
            Self::CorruptPriorCheckpointRefusals => {
                "corrupt_prior_checkpoint_refuses_read_modify_write"
            }
        }
    }

    fn comparison(self) -> ComparisonMode {
        match self {
            Self::CorruptGraphNodeRefusals
            | Self::CorruptPendingTurnInputRefusals
            | Self::CorruptQueuedWorkRefusals
            | Self::CorruptPriorCheckpointRefusals => ComparisonMode::RawOnly,
            _ => ComparisonMode::Decoded,
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
    /// A turn's terminal head write over the pending follow-on fact
    /// (ADR 0101 §3): `owed_turn_id` set is the frame-switch commit that
    /// leaves the head owing that turn; `None` is the follow-on turn's own
    /// terminal commit, which clears the fact. Seeded so the inventory can
    /// drive `raise_pending_follow_on_attempts` over a live fact.
    CommitFollowOn {
        label: &'static str,
        expected_head_revision: u64,
        turn_id: &'static str,
        owed_turn_id: Option<&'static str>,
    },
    RecordAttachmentIntent,
    ReclaimRetainedEvidence,
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
    /// Re-claim, by exact batch id, work this generation already holds. Every
    /// backend must report no newly claimed rows — the shared claim planner's
    /// deferral agreement check (FIG-1065).
    ClaimHeldBatchById {
        lease: LeaseSlot,
    },
    /// Snapshots the claims a successor-generation reclaim will supersede, so
    /// a later stale settlement can present the superseded authority.
    RetainStaleClaims,
    ReleaseSessionLease {
        lease: LeaseSlot,
    },
    CommitStaleTurnInputClaim {
        expected_head_revision: u64,
    },
    CommitStaleQueuedWorkClaim {
        expected_head_revision: u64,
    },
    /// SQLite and PostgreSQL discard the live store/factory and reopen through
    /// an independent connection. In-memory has no independent durable
    /// instance, so its leg can only reopen the same object through the
    /// retained factory and does not prove cold-instance reconstruction.
    ColdReopenSession,
    /// Enter deletion through `LashCore::delete_session` to exercise the store
    /// tombstone and subsequent admission refusal. The lifecycle backend's
    /// store set supplies its real process registry and trigger store, and the
    /// recording effect host runs session-close and process-deletion effects
    /// through their local executors, so this leg covers the full delete path
    /// an embedder sees.
    DeleteSession,
    AttemptAdmission,
    CreateHandle {
        handle_alias: &'static str,
    },
    DeleteSessionThroughFactory,
    AdmitOnHandle {
        handle_alias: &'static str,
    },
    /// One fallible store-trait method from the operation inventory. See
    /// `surface_sweep.rs`; the inventory is gated by `trait_surface_gate.rs`.
    DriveSurface {
        method: SurfaceMethod,
    },
    SeedCorruptRecord {
        target: CorruptTarget,
    },
    /// Put the original record back, so a shared content-addressed blob is
    /// never left corrupt for a later case.
    RestoreCorruptRecord {
        target: CorruptTarget,
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
            Self::Commit { label, .. } | Self::CommitFollowOn { label, .. } => label,
            Self::RecordAttachmentIntent => "record_attachment_intent",
            Self::ReclaimRetainedEvidence => "reclaim_terminal_evidence_with_retained_fork",
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
            Self::ClaimHeldBatchById { .. } => "claim_held_batch_by_id",
            Self::RetainStaleClaims => "retain_stale_claims",
            Self::ReleaseSessionLease { .. } => "release_first_session_lease_generation",
            Self::CommitStaleTurnInputClaim { .. } => {
                "commit_stale_claim_before_successor_reclaims_row"
            }
            Self::CommitStaleQueuedWorkClaim { .. } => {
                "commit_stale_queued_work_claim_after_successor_reclaims_row"
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
            Self::DriveSurface { method } => method.label(),
            Self::SeedCorruptRecord { target } => target.seed_label(),
            Self::RestoreCorruptRecord { target } => target.restore_label(),
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

    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    fn materialize(self, session_id: &SessionId) -> SessionNodeRecord {
        let frame_key = differential_frame_key(self.node_id);
        SessionNodeRecord {
            node_id: scoped_node_id(session_id, self.node_id).into(),
            parent_node_id: self
                .parent_node_id
                .map(|node_id| scoped_node_id(session_id, node_id).into()),
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

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn differential_frame_key(node_id: &str) -> lash_core::FrameKey {
    lash_core::FrameKey::from_caller_material(&format!("differential-frame:{node_id}"))
        .expect("non-empty differential frame material")
}

fn is_frame_alias(node_id: &str) -> bool {
    matches!(
        node_id,
        "active-frame" | "collision" | "root" | "stale-claim-node"
    )
}

fn scoped_node_id(session_id: &SessionId, node_id: &str) -> String {
    if is_frame_alias(node_id) {
        lash_core::facade_support::frame_node_id(
            session_id,
            differential_frame_key(node_id).as_str(),
        )
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
        claim_cases::settle_claim_before_successor_reclaim(),
        checkpoint_cases::bodies_then_ref_only(),
        checkpoint_cases::bodies_then_cleared(),
        checkpoint_cases::missing_component_ref(),
        fork_cases::fence_precedence_case(),
        fork_cases::pin_fork_unpin(),
        fork_cases::foreign_lineage_case(),
        fork_cases::rewind_case(),
        GeneratedCase {
            name: CaseName::AttachmentAdoption,
            operations: vec![
                StoreOperation::RecordAttachmentIntent,
                StoreOperation::Commit {
                    label: "adopt_attachment_in_runtime_commit",
                    expected_head_revision: 0,
                    graph: append(
                        vec![NodeSpec::new("active-frame", None, "attachment-prefix")],
                        Some("active-frame"),
                    ),
                    turn_commit: Some(TurnCommitSpec {
                        turn_id: "attachment-adoption",
                    }),
                    checkpoint: CheckpointSpec::Empty,
                    usage: true,
                    adopt_attachment: true,
                },
                StoreOperation::PinLeaf,
                StoreOperation::Rewind,
                StoreOperation::ReclaimRetainedEvidence,
                StoreOperation::UnpinLeaf,
            ],
        },
        claim_cases::queued_work_claim_and_abandon(),
        claim_cases::same_generation_exact_claim_deferral(),
        claim_cases::queued_work_claim_superseded_after_reclaim(),
        claim_cases::turn_input_claim_superseded_after_reclaim(),
        GeneratedCase {
            name: CaseName::DeleteThenAttemptAdmission,
            operations: vec![
                StoreOperation::DeleteSession,
                StoreOperation::AttemptAdmission,
            ],
        },
        surface_sweep::surface_sweep_case(),
        surface_sweep::pending_follow_on_raise_case(),
        surface_sweep::turn_bound_claim_case(),
        surface_sweep::root_claim_replay_case(),
        surface_sweep::refused_surface_on_deleted_session_case(),
        surface_sweep::session_close_ledger_case(),
        surface_sweep::root_control_case(false),
        surface_sweep::root_control_case(true),
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
    .into_iter()
    // Last: these are the only cases that leave the shared PostgreSQL
    // database temporarily holding a corrupt record, and each restores it.
    .chain(corrupt_input_cases::corrupt_input_cases())
    .collect()
}

fn materialize_graph(session_id: &SessionId, spec: &GraphSpec) -> GraphAppend {
    let nodes = spec
        .nodes
        .iter()
        .copied()
        .map(|node| node.materialize(session_id))
        .collect::<Vec<_>>();
    if nodes.is_empty() {
        return GraphAppend::PreserveHead;
    }
    debug_assert_eq!(
        spec.leaf_node_id
            .map(|node_id| scoped_node_id(session_id, node_id)),
        nodes.last().map(|node| node.node_id.to_string()),
        "an extended graph leaf must be its terminal appended node"
    );
    GraphAppend::Extend { nodes }
}

// A commit is genuinely this many independent parts; bundling them into a
// params struct here would only move the same fields behind another name.
#[allow(clippy::too_many_arguments)]
fn runtime_commit(
    session_id: &SessionId,
    expected_head_revision: u64,
    graph: &GraphSpec,
    turn_commit: Option<TurnCommitSpec>,
    current_frame_node_id: Option<lash_core::FrameNodeId>,
    checkpoint: HydratedSessionCheckpoint,
    usage_deltas: Vec<TokenLedgerEntry>,
    committed_attachment_ids: Vec<AttachmentId>,
) -> RuntimeCommit {
    let state = RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
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
                lash_core::facade_support::frame_node_id(session_id, frame_key.as_str()),
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

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn checkpoint_bodies() -> HydratedSessionCheckpoint {
    let tool_state = serde_json::from_value::<ToolState>(serde_json::json!({
        "generation": 7,
        "tools": {}
    }))
    .expect("build differential tool state");
    let plugin_state = PluginState {
        plugins: [(
            "differential-plugin".to_string(),
            PluginNamespaceState {
                generation: 11,
                values: std::collections::BTreeMap::from([(
                    "state".into(),
                    serde_json::json!({"mode": "durable"}),
                )]),
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
            lash_core::store::PLUGIN_STATE_CHECKPOINT_COMPONENT.to_string(),
            lash_core::HydratedCheckpointComponent::changed(
                rmp_serde::to_vec_named(&plugin_state)
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
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
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

/// The claimable turn work the generated sequences enqueue: one durable
/// process wake with a fixed `(process, sequence)` source, so a repeated
/// enqueue in one sequence is the same idempotent source on every backend.
fn claim_observability_wake(session_id: &SessionId) -> lash_core::runtime::ProcessWakeDelivery {
    let process_id = || lash_core::runtime::ProcessId::fixture("differential-process");
    lash_core::runtime::ProcessWakeDelivery {
        version: lash_core::runtime::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        wake_id: "differential-process-wake-1".to_string(),
        target_session_id: session_id.clone(),
        process_id: process_id(),
        sequence: 1,
        event_type: "process.wake".to_string(),
        event_invocation: lash_core::runtime::RuntimeInvocation {
            attribution: lash_core::runtime::RuntimeAttribution::for_session(session_id.clone()),
            subject: lash_core::runtime::RuntimeSubject::ProcessEvent {
                process_id: process_id(),
                sequence: 1,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: lash_core::runtime::QueuedWorkAuthority::default(),
        input: "exercise queued-work claim state".to_string(),
        created_at_ms: 1,
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
        usage_disposition: Default::default(),
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn differential_attachment_id() -> AttachmentId {
    AttachmentId::parse("differential-attachment").expect("valid attachment id")
}

/// The process that owns the differential's process-scoped attachment. No
/// registry row backs it: the manifest records its owner by id alone.
fn differential_process_owner_id() -> lash_sansio::ProcessId {
    lash_sansio::ProcessId::fixture("differential-process-owner")
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn differential_process_attachment_id() -> AttachmentId {
    AttachmentId::parse("differential-process-attachment").expect("valid attachment id")
}

// Row shapes for the SQL observation queries. Named because the tuples are wide
// enough that clippy flags them inline, and a name reads better at the use site.
type AttachmentRow = (
    String,
    String,
    i64,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<String>,
);
type LeaseRow = (
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
    i64,
    i64,
);
type QueuedWorkItemRow = (String, i64, String);

enum RawDurableReader {
    /// A SQLite durable core, file or memory: `path` is the file, or the
    /// memory database's URI, a raw connection opens.
    Sqlite {
        path: PathBuf,
        session_id: SessionId,
        store: Option<Arc<dyn ConformancePersistence>>,
    },
    Postgres {
        pool: PgPool,
        session_id: SessionId,
        store: Option<Arc<dyn ConformancePersistence>>,
    },
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
) -> Option<LeaseOwnerIdentity> {
    match (owner_id, incarnation_id) {
        (None, None) => None,
        (Some(owner_id), Some(incarnation_id)) => {
            Some(LeaseOwnerIdentity::opaque(owner_id, incarnation_id))
        }
        fields => panic!("partial lease-owner identity in durable row: {fields:?}"),
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn normalized_sql_node_json(node_json: &str) -> Vec<u8> {
    let value = serde_json::from_str(node_json).expect("decode SQL durable node");
    normalized_node_json(value)
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn normalized_node_json(value: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&value).expect("encode normalized durable node")
}

#[derive(Clone)]
enum BackendReopen {
    /// Fresh handles on the memory backend's databases.
    SqliteMemory {
        backend: Arc<lash_sqlite_store::SqliteStoreSet>,
    },
    Sqlite {
        root: PathBuf,
    },
    Postgres {
        database_url: String,
    },
}

struct NamedHandle {
    store: Arc<dyn ConformancePersistence>,
    meta: SessionMeta,
}

struct BackendRunner {
    name: &'static str,
    session_id: SessionId,
    store: Option<Arc<dyn ConformancePersistence>>,
    factory: Option<Arc<dyn ConformanceSessionStoreFactory>>,
    raw_reader: RawDurableReader,
    reopen: BackendReopen,
    clock: Arc<dyn Clock>,
    handles: BTreeMap<&'static str, NamedHandle>,
    /// The backend the facade lifecycle core runs on: the backend's own
    /// backend over the same storage.
    lifecycle_backend: lash::Backend,
    lifecycle_core: Option<lash::LashCore>,
    reopened_postgres_pool: Option<PgPool>,
    first_lease: Option<lash_core::SessionExecutionLease>,
    successor_lease: Option<lash_core::SessionExecutionLease>,
    stale_turn_input_claim: Option<TurnInputClaim>,
    retained_stale_turn_input_claim: Option<TurnInputClaim>,
    queued_work_claim: Option<QueuedWorkClaim>,
    stale_queued_work_claim: Option<QueuedWorkClaim>,
    current_frame_node_id: Option<lash_core::FrameNodeId>,
    current_leaf_node_id: Option<String>,
    checkpoint_component_refs: Option<CheckpointComponentRefs>,
    expected_execution_state: Option<Vec<u8>>,
    surface: SurfaceScratch,
}

impl BackendRunner {
    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    fn store(&self) -> Arc<dyn ConformancePersistence> {
        Arc::clone(
            self.store
                .as_ref()
                .expect("backend runner is attached to a store"),
        )
    }

    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    fn factory(&self) -> Arc<dyn ConformanceSessionStoreFactory> {
        Arc::clone(
            self.factory
                .as_ref()
                .expect("backend runner is attached to a factory"),
        )
    }

    fn create_request(&self) -> SessionStoreCreateRequest {
        SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: self.session_id.clone(),
            relation: SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        }
    }

    fn assert_session_deleted(&self, error: &StoreError, operation: &str) {
        assert!(
            matches!(
                error,
                StoreError::SessionDeleted { session_id } if session_id == self.session_id
            ),
            "{} {operation} must return typed SessionDeleted for `{}`, got: {error}",
            self.name,
            self.session_id
        );
    }

    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
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
        lash::LashCore::standard_builder(
            self.lifecycle_backend.clone(),
            lash::TurnBudget::Unbounded,
        )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .provider(provider)
        .model(model)
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

    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
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

    /// Drive `commit` through the store and, on success, thread the runner's
    /// frame/leaf tracking and checkpoint expectations forward.
    async fn commit_and_track(
        &mut self,
        commit: RuntimeCommit,
        checkpoint: CheckpointSpec,
    ) -> Result<Option<ComparableRuntimeCommitResult>, StoreError> {
        let next_frame_node_id = commit.current_frame_node_id.clone();
        let result = self.store().commit_runtime_state(commit).await;
        match result {
            Ok(result) => {
                self.current_frame_node_id = next_frame_node_id;
                self.current_leaf_node_id = result
                    .committed_leaf_node_id
                    .clone()
                    .map(|id| id.to_string());
                if matches!(checkpoint, CheckpointSpec::Bodies) {
                    self.checkpoint_component_refs = Some(CheckpointComponentRefs {
                        components: result.manifest.components.clone(),
                    });
                }
                match checkpoint {
                    CheckpointSpec::Bodies => {
                        self.expected_execution_state = checkpoint_bodies()
                            .component_body(lash_core::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
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

    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
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
                self.commit_and_track(commit, *checkpoint).await
            }
            StoreOperation::CommitFollowOn {
                expected_head_revision,
                turn_id,
                owed_turn_id,
                ..
            } => {
                // The follow-on fact rides a turn's terminal head write
                // (ADR 0101 §3): `owed_turn_id` set is the frame switch, and
                // `None` is the follow-on's own terminal commit clearing it.
                let head = self.store().load_session_head_meta().await?;
                let mut commit = runtime_commit(
                    &self.session_id,
                    *expected_head_revision,
                    &append(Vec::new(), None),
                    None,
                    self.current_frame_node_id.clone(),
                    HydratedSessionCheckpoint::default(),
                    Vec::new(),
                    Vec::new(),
                );
                let (frame, leaf) = head
                    .map(|head| {
                        (
                            head.current_frame_node_id
                                .filter(|_| head.leaf_node_id.is_some()),
                            head.leaf_node_id,
                        )
                    })
                    .unwrap_or_default();
                commit.current_frame_node_id = frame;
                commit.graph_base_leaf_node_id = leaf;
                commit.turn_commit =
                    RuntimeTurnCommitStamp::new(lash_core::store::OperationId::turn(
                        &self.session_id,
                        *turn_id,
                        lash_core::store::pending_follow_on::TURN_TERMINAL_OPERATION_KEY,
                    ));
                commit.pending_follow_on =
                    owed_turn_id.map(|owed_turn_id| lash_core::store::PendingFollowOn {
                        follow_on_turn_id: lash_core::TurnId::from(owed_turn_id),
                        frame_id: commit
                            .current_frame_node_id
                            .clone()
                            .expect("a pending follow-on owes the head's current frame"),
                        task: "fig-2841 follow-on task".to_string(),
                        options: None,
                        chain_depth: 1,
                        attempts: 0,
                    });
                self.commit_and_track(commit, CheckpointSpec::Empty).await
            }
            StoreOperation::RecordAttachmentIntent => {
                attachment_seeding::seed_differential_attachment_rows(
                    self.store().as_ref(),
                    &self.session_id,
                )
                .await?;
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
                        pending_observer_intents: Vec::new(),
                        session_id: SessionId::from(format!("{}:fork", self.session_id)),
                        node_id: node_id.clone().into(),
                        relation: SessionRelation::Fork {
                            source_session_id: self.session_id.clone(),
                            source_node_id: node_id.into(),
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
                        lash_core::facade_support::SessionCommand::RefreshToolCatalog {
                            reason: "cross-backend delete observability".to_string(),
                        },
                    )
                    .with_source_key("cross-backend-delete-observability"),
                )
                .await
                .map(|_| None),
            StoreOperation::EnqueueClaimableQueuedWork => self
                .store()
                .enqueue_queued_work(
                    lash_core::runtime::process_wake_batch_draft_with_delivery_policy(
                        claim_observability_wake(&self.session_id),
                        DeliveryPolicy::AfterCurrentTurnCommit,
                    )
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
            StoreOperation::ClaimHeldBatchById { lease } => {
                let lease = self.lease(*lease);
                let owner = lease.owner.clone();
                let held_batch_ids = self
                    .queued_work_claim
                    .as_ref()
                    .expect("generated sequence claimed queued work before the held re-claim")
                    .data
                    .batches
                    .iter()
                    .map(|batch| batch.batch_id.clone())
                    .collect::<Vec<_>>();
                let outcome = self
                    .store()
                    .claim_ready_queued_work_by_batch_ids(
                        &self.session_id,
                        &lease.fence(),
                        &owner,
                        QueuedWorkClaimBoundary::Idle,
                        &held_batch_ids,
                        lash_core::testing::queued_work_claim_policy(1),
                    )
                    .await?;
                assert!(
                    outcome.claim.is_none() && outcome.already_satisfied_batch_ids.is_empty(),
                    "{} must report no newly claimed rows when this generation \
                     exact-claims a batch it already holds",
                    self.name
                );
                Ok(None)
            }
            StoreOperation::RetainStaleClaims => {
                self.retained_stale_turn_input_claim = self.stale_turn_input_claim.clone();
                self.stale_queued_work_claim = self.queued_work_claim.clone();
                Ok(None)
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
                    .retained_stale_turn_input_claim
                    .as_ref()
                    .or(self.stale_turn_input_claim.as_ref())
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
            StoreOperation::CommitStaleQueuedWorkClaim {
                expected_head_revision,
            } => {
                let claim = self
                    .stale_queued_work_claim
                    .as_ref()
                    .or(self.queued_work_claim.as_ref())
                    .expect("generated sequence claimed queued work before stale settlement");
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
                        .completing_queue_claim(claim.completion()),
                    )
                    .await
                    .map(|result| Some(result.into()))
            }
            StoreOperation::ColdReopenSession => {
                let request = self.create_request();
                let reopened = match self.reopen.clone() {
                    BackendReopen::SqliteMemory { backend } => {
                        self.store.take();
                        self.factory.take();
                        self.raw_reader.detach_store();

                        let reopened_backend = backend
                            .reopen_with_clock(Arc::clone(&self.clock))
                            .await
                            .expect("reopen the SQLite memory backend");
                        let concrete_factory = reopened_backend.session_store_factory();
                        let reopened = concrete_factory
                            .open_existing_conformance_store(&request)
                            .await
                            .map_err(StoreError::Backend)?
                            .expect("SQLite memory session must survive an independent reopen");
                        self.factory =
                            Some(concrete_factory as Arc<dyn ConformanceSessionStoreFactory>);
                        self.raw_reader = RawDurableReader::Sqlite {
                            path: PathBuf::from(
                                reopened_backend
                                    .database_uri(lash_sqlite_store::SqliteDatabase::DurableCore),
                            ),
                            session_id: self.session_id.clone(),
                            store: Some(Arc::clone(&reopened)),
                        };
                        reopened
                    }
                    BackendReopen::Sqlite { root } => {
                        self.store.take();
                        self.factory.take();
                        self.raw_reader.detach_store();

                        let concrete_factory = Arc::new(
                            lash_sqlite_store::SqliteSessionStoreFactory::new(root.clone())
                                .with_clock(Arc::clone(&self.clock)),
                        );
                        let reopened = concrete_factory
                            .open_existing_conformance_store(&request)
                            .await
                            .map_err(StoreError::Backend)?
                            .expect("SQLite session must survive an independent reopen");
                        let path =
                            root.join(lash_sqlite_store::SqliteDatabase::DurableCore.file_name());
                        self.factory =
                            Some(concrete_factory as Arc<dyn ConformanceSessionStoreFactory>);
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
                            .open_existing_conformance_store(&request)
                            .await
                            .map_err(StoreError::Backend)?
                            .expect("Postgres session must survive an independent reopen");
                        self.factory =
                            Some(concrete_factory as Arc<dyn ConformanceSessionStoreFactory>);
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
                        .decode_component::<PluginState>(
                            lash_core::store::PLUGIN_STATE_CHECKPOINT_COMPONENT,
                        )?
                        .as_ref()
                        .map(|snapshot| serde_json::to_value(snapshot).expect("encode snapshot")),
                    expected
                        .decode_component::<PluginState>(
                            lash_core::store::PLUGIN_STATE_CHECKPOINT_COMPONENT,
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
            StoreOperation::ReclaimRetainedEvidence => self.reclaim_terminal_evidence().await,
            StoreOperation::DeleteSession => {
                let core = self.build_lifecycle_core();
                let administration = core.session_administration().await;
                let context = administration
                    .delete_context(&self.session_id)
                    .expect("issue the differential delete context");
                lash::LashCore::delete_session(context)
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
                    .open_existing_conformance_store(&request)
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
            StoreOperation::DriveSurface { method } => self.drive_surface(*method).await,
            StoreOperation::SeedCorruptRecord { target } => self.seed_corrupt_record(*target).await,
            StoreOperation::RestoreCorruptRecord { target } => {
                self.restore_corrupt_record(*target).await
            }
            StoreOperation::ObserveSessionAbsent => {
                let request = self.create_request();
                assert!(
                    self.factory()
                        .open_existing_conformance_store(&request)
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

    /// Snapshot every durable row this operation could touch, without
    /// decoding any of them. Taken immediately before and after each step so a
    /// refused operation can be held to the no-residue law.
    async fn residue_digest(&self) -> ResidueDigest {
        self.raw_reader.residue_digest().await
    }

    async fn observe(
        &self,
        before: &ResidueDigest,
        comparison: ComparisonMode,
        result: Result<Option<ComparableRuntimeCommitResult>, StoreError>,
    ) -> (StepObservation, Vec<&'static str>) {
        let (store_error, runtime_commit_result) = match result {
            Ok(result) => (None, result),
            Err(error) => (Some(normalized_store_error(self.name, &error)), None),
        };
        let freshness_head = match self.store().load_session_head_meta().await {
            Ok(Some(head)) => FreshnessHeadObservation::Present {
                head_revision: head.head_revision,
                leaf_node_id: head.leaf_node_id.map(|id| id.to_string()),
                checkpoint_ref: head.checkpoint_ref,
            },
            Ok(None) => FreshnessHeadObservation::Missing,
            Err(error) => {
                FreshnessHeadObservation::Error(normalized_store_error(self.name, &error))
            }
        };
        // A deliberately undecodable row cannot be read through the decoded
        // digest; the raw changed-table set is the comparison instead.
        let durable_state = match comparison {
            ComparisonMode::Decoded => Some(self.raw_reader.observe().await),
            ComparisonMode::RawOnly => None,
        };
        let surface_answer = self.surface.answer.clone();
        let changed_tables = before.changed_tables(&self.residue_digest().await);
        let refusal_mutated = store_error.is_some().then_some(!changed_tables.is_empty());
        (
            StepObservation {
                store_error,
                surface_answer,
                refusal_mutated,
                runtime_commit_result,
                freshness_head,
                durable_state,
                raw_changed_tables: match comparison {
                    ComparisonMode::Decoded => None,
                    ComparisonMode::RawOnly => Some(changed_tables.clone()),
                },
            },
            changed_tables,
        )
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

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
async fn assert_storage_failure_mappings_agree(sqlite_root: &Path, postgres: &PostgresStorage) {
    let create_request = SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(format!("fig-1242-storage-failure:{}", run_nonce())),
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
    let sqlite_connection = rusqlite::Connection::open(sqlite_factory.catalog_uri())
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
        normalized_store_error("sqlite-memory", &first_capture_failure),
        normalized_store_error("sqlite-memory", &second_capture_failure),
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

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let timestamp_ms = 1_000;
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(timestamp_ms),
        )
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

#[test]
fn differential_clock_wall_clock_faces_agree() {
    let clock = DifferentialClock;
    let clock: &dyn lash_core::Clock = &clock;
    let milliseconds = clock.timestamp_ms();
    let datetime = clock.timestamp_datetime();
    let text = chrono::DateTime::parse_from_rfc3339(&clock.timestamp_rfc3339())
        .expect("clock emits RFC 3339");
    assert_eq!(datetime.timestamp_millis() as u64, milliseconds);
    assert_eq!(text.timestamp_millis() as u64, milliseconds);
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

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
async fn runners_for_case_with_clock(
    case: CaseName,
    sqlite_root: &Path,
    postgres: &PostgresStorage,
    postgres_database_url: &str,
    run_nonce: &str,
    clock: Arc<dyn Clock>,
) -> Vec<BackendRunner> {
    let session_id = SessionId::from(format!("fig-778-{run_nonce}-{}", case.as_str()));
    // The deterministic relation is declared at creation on every backend:
    // `save_session_meta` may not move a recorded lineage (FIG-3045), so the
    // metadata install below rewrites a row that already records it.
    let relation = SessionRelation::Child {
        parent_session_id: SessionId::from(format!("fig-778-{run_nonce}-parent")),
        caused_by: None,
    };
    let create_request = SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: session_id.clone(),
        relation: relation.clone(),
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let expected_meta = SessionMeta {
        pending_observer_intents: Vec::new(),
        session_id: session_id.clone(),
        relation,
    };

    let memory_backend = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory_with_clock(Arc::clone(&clock))
            .await
            .expect("open the SQLite memory differential store set"),
    );
    let memory_factory = memory_backend.session_store_factory();
    let memory_store = memory_factory
        .create_conformance_store(&create_request)
        .await
        .expect("create SQLite memory differential store");
    memory_store
        .save_session_meta(expected_meta.clone())
        .await
        .expect("install deterministic SQLite memory session metadata");
    let memory_factory_dyn = Arc::clone(&memory_factory) as Arc<dyn ConformanceSessionStoreFactory>;
    let memory_path =
        PathBuf::from(memory_backend.database_uri(lash_sqlite_store::SqliteDatabase::DurableCore));

    let sqlite_case_root = sqlite_root.join(case.as_str());
    let sqlite_factory = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(sqlite_case_root.clone())
            .with_clock(Arc::clone(&clock)),
    );
    let sqlite_path =
        sqlite_case_root.join(lash_sqlite_store::SqliteDatabase::DurableCore.file_name());
    let sqlite_store = sqlite_factory
        .create_conformance_store(&create_request)
        .await
        .expect("create SQLite differential store");
    sqlite_store
        .save_session_meta(expected_meta.clone())
        .await
        .expect("install deterministic SQLite session metadata");
    let sqlite_factory_dyn = Arc::clone(&sqlite_factory) as Arc<dyn ConformanceSessionStoreFactory>;

    let postgres_factory = Arc::new(
        postgres
            .session_store_factory()
            .with_clock(Arc::clone(&clock)),
    );
    let postgres_store = postgres_factory
        .create_conformance_store(&create_request)
        .await
        .expect("create Postgres differential store");
    postgres_store
        .save_session_meta(expected_meta.clone())
        .await
        .expect("install deterministic Postgres session metadata");
    let postgres_factory_dyn =
        Arc::clone(&postgres_factory) as Arc<dyn ConformanceSessionStoreFactory>;

    let memory_lifecycle: lash::Backend =
        lash_conformance::recording_backend_over(memory_backend.clone());
    let sqlite_lifecycle: lash::Backend = lash_conformance::recording_backend_over(Arc::new(
        lash_sqlite_store::SqliteStoreSet::open_with_options_and_clock(
            &sqlite_case_root,
            lash_sqlite_store::SqliteStoreSetOptions::default(),
            Arc::clone(&clock),
        )
        .await
        .expect("open the SQLite lifecycle store set"),
    ));
    // PostgreSQL is storage only (ADR 0104): the lifecycle runs over its
    // store set, and the session delete it drives needs some effect-host
    // authority to retire scopes against — a recording host is enough; the
    // differential certifies the rows, not the journal.
    let postgres_effects: Arc<dyn lash_core::EffectHost> =
        Arc::new(lash_conformance::RecordingEffectHost::default());
    let postgres_lifecycle: lash::Backend = lash_conformance::backend_over(
        Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
            postgres,
            Arc::new(lash::persistence::FileAttachmentStore::new(
                sqlite_case_root.join("postgres-attachments"),
            )),
            lash_core::WakeDeliveryConfig::default(),
            Arc::clone(&clock),
        )),
        postgres_effects,
    );

    vec![
        BackendRunner {
            name: "sqlite-memory",
            session_id: session_id.clone(),
            store: Some(Arc::clone(&memory_store)),
            factory: Some(memory_factory_dyn),
            raw_reader: RawDurableReader::Sqlite {
                path: memory_path,
                session_id: session_id.clone(),
                store: Some(memory_store),
            },
            reopen: BackendReopen::SqliteMemory {
                backend: memory_backend,
            },
            clock: Arc::clone(&clock),
            handles: BTreeMap::new(),
            lifecycle_backend: memory_lifecycle,
            lifecycle_core: None,
            reopened_postgres_pool: None,
            first_lease: None,
            successor_lease: None,
            stale_turn_input_claim: None,
            retained_stale_turn_input_claim: None,
            queued_work_claim: None,
            stale_queued_work_claim: None,
            current_frame_node_id: None,
            current_leaf_node_id: None,
            checkpoint_component_refs: None,
            expected_execution_state: None,
            surface: SurfaceScratch::default(),
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
            lifecycle_backend: sqlite_lifecycle,
            lifecycle_core: None,
            reopened_postgres_pool: None,
            first_lease: None,
            successor_lease: None,
            stale_turn_input_claim: None,
            retained_stale_turn_input_claim: None,
            queued_work_claim: None,
            stale_queued_work_claim: None,
            current_frame_node_id: None,
            current_leaf_node_id: None,
            checkpoint_component_refs: None,
            expected_execution_state: None,
            surface: SurfaceScratch::default(),
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
            lifecycle_backend: postgres_lifecycle,
            lifecycle_core: None,
            reopened_postgres_pool: None,
            first_lease: None,
            successor_lease: None,
            stale_turn_input_claim: None,
            retained_stale_turn_input_claim: None,
            queued_work_claim: None,
            stale_queued_work_claim: None,
            current_frame_node_id: None,
            current_leaf_node_id: None,
            checkpoint_component_refs: None,
            expected_execution_state: None,
            surface: SurfaceScratch::default(),
        },
    ]
}

/// Two registrar mints that hand out the same ids in the same order, one per
/// backend, so a differential compares rows the two registrars minted under
/// the same id. The random start keeps a run clear of every earlier run's rows
/// in the shared PostgreSQL database.
fn paired_process_id_mints() -> (lash_core::ProcessIdMint, lash_core::ProcessIdMint) {
    let start = fastrand::u64(1..1 << 40) << 16;
    let mint =
        || lash_core::ProcessIdMint::Sequential(Arc::new(std::sync::atomic::AtomicU64::new(start)));
    (mint(), mint())
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
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
    assert_eq!(cases.len(), 33);
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
            "same_generation_exact_claim_defers",
            "queued_work_claim_superseded_after_successor_reclaim",
            "turn_input_claim_superseded_after_successor_reclaim",
            "delete_then_attempt_admission",
            "store_surface_sweep",
            "pending_follow_on_raise_and_clear",
            "turn_bound_claim_binds_defers_and_reclaims_across_generations",
            "root_claim_replays_exact_result_after_lease_handoff",
            "refused_surface_on_deleted_session_leaves_no_residue",
            "session_close_ledger_closes_roots_and_tracks_its_intent",
            "root_cancel_ledger",
            "root_fork_ledger",
            "stale_handle_after_delete",
            "corrupt_graph_node_refuses_every_reader",
            "corrupt_pending_turn_input_refuses_list_and_claim",
            "corrupt_queued_work_refuses_list_and_claim",
            "corrupt_prior_checkpoint_refuses_read_modify_write",
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "compares three durable backends; requires Postgres (`just push-gate`, or LASH_POSTGRES_DATABASE_URL with `kiln run //crates/lash-sim:cross_backend_store_differential__test -- --include-ignored`)"]
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
                 required_backends=[sqlite-memory,sqlite,postgres]; \
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
                 required_backends=[sqlite-memory,sqlite,postgres]; \
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
    // Worker open never provisions (FIG-3797): apply the committed artifact,
    // the same step `lash migrate` performs, before opening.
    sqlx::raw_sql(PostgresStorage::schema_ddl())
        .execute(&mut database_lock)
        .await
        .expect("provision the shared Postgres database from schema.sql");
    let postgres = PostgresStorage::connect(&database_url)
        .await
        .expect("connect required Postgres differential backend");
    let sqlite_root = tempfile::tempdir().expect("create SQLite differential root");
    verify_independent_session_meta_layout(sqlite_root.path(), &postgres).await;
    let run_nonce = run_nonce();
    process_event_pages::compare_bounded_process_event_pages(
        sqlite_root.path(),
        &postgres,
        &run_nonce,
    )
    .await;
    Box::pin(plugin_state_case::compare_plugin_state(
        sqlite_root.path(),
        &postgres,
        &database_url,
        &run_nonce,
    ))
    .await;
    let mut divergences = String::new();
    let mut residue_violations = String::new();
    eprintln!(
        "RUNNING cross-backend store differential; \
         compared_backends=[sqlite-memory,sqlite,postgres]; cases={}",
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
        fork_cases::prepare_retention_case(case.name, &runners).await;
        for (step_index, operation) in case.operations.iter().enumerate() {
            let mut observations = Vec::with_capacity(runners.len());
            for runner in &mut runners {
                runner.surface.answer = None;
                let before = runner.residue_digest().await;
                let result = runner.apply(operation).await;
                let (observation, changed_tables) = runner
                    .observe(&before, case.name.comparison(), result)
                    .await;
                // FIG-2841 law: a refused operation leaves durable state
                // byte-identical to before the call. Recorded rather than
                // panicked so the same step still reports every backend.
                if observation.store_error.is_some() && !changed_tables.is_empty() {
                    let _ = writeln!(
                        residue_violations,
                        "\ncase={} step={} operation={} backend={} error={} mutated_tables={:?}",
                        case.name.as_str(),
                        step_index + 1,
                        operation.label(),
                        runner.name,
                        observation.store_error.as_deref().unwrap_or("<none>"),
                        changed_tables,
                    );
                }
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
    assert!(
        residue_violations.is_empty(),
        "a refused store operation left durable residue:{residue_violations}"
    );
    Box::pin(fork_cases::cross_owner_attachment_adoption(
        sqlite_root.path(),
        &postgres,
    ))
    .await;
    fork_cases::selected_observer_intents(sqlite_root.path(), &postgres, &run_nonce).await;
    assert_storage_failure_mappings_agree(sqlite_root.path(), &postgres).await;
    eprintln!(
        "PASSED cross-backend store differential; \
         compared_backends=[sqlite-memory,sqlite,postgres]"
    );
}
