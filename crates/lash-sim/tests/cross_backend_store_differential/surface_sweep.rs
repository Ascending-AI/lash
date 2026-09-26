//! The operation inventory over the fallible store-trait surface (FIG-2841).
//!
//! `trait_surface_gate.rs` refuses any fallible method of the gated store
//! traits that this binary never calls. The drivers live here: one
//! [`SurfaceMethod`] per trait method, each driven as its own compared step so
//! the error class, the mutated-or-not verdict, and the post-state digest are
//! all compared across the three backends, and so the no-residue law is
//! checked on any step that refuses.
//!
//! Answers are summarized coarsely and deliberately. Backend-generated ids,
//! sequence counters, and wall-clock stamps are not cross-backend comparable;
//! what a read *found* is. The durable contract itself is carried by the
//! digest comparison the step loop already performs.

use super::*;
use corrupt_input_cases::CorruptBackup;

/// A syntactically valid but never-granted lease authority, for the inventory
/// steps that take a fence in a case that holds no lease. Presenting it is
/// itself a refusal driver: no backend may act on unheld authority.
fn unheld_lease_fence(session_id: &SessionId) -> lash_core::SessionExecutionLeaseAuthority {
    lash_core::SessionExecutionLeaseAuthority {
        session_id: session_id.clone(),
        owner: LeaseOwnerIdentity::opaque("fig-2841-no-lease", "fig-2841-no-lease:incarnation"),
        executor_id: "fig-2841-no-lease-executor".to_string(),
        lease_token: "fig-2841-no-lease-token".to_string(),
        fencing_token: 0,
    }
}

/// Scratch state the sweep threads between its own steps.
#[derive(Default)]
pub(super) struct SurfaceScratch {
    pub(super) answer: Option<String>,
    pub(super) corrupt_backup: Option<CorruptBackup>,
    pub(super) corrupt_target: Option<corrupt_input_cases::CorruptTarget>,
    pub(super) batch_id: Option<String>,
    pub(super) queued_work_claim: Option<QueuedWorkClaim>,
    pub(super) turn_input_claim: Option<TurnInputClaim>,
    pub(super) queued_run: Option<lash_core::store::QueuedRunAdmission>,
    /// The `CloseSession` intent this backend's ledger minted for the case's
    /// session: ids are the backend's own clock, so answers compare it by
    /// identity, never by value.
    pub(super) close_intent: Option<lash_core::store::ControlIntentId>,
}

/// One fallible store-trait method, driven as a compared differential step.
#[derive(Clone, Copy, Debug)]
pub(super) enum SurfaceMethod {
    LoadSession,
    ListPendingTurnInputs,
    ListTurnInputApplications,
    ClaimNextTurnInputs,
    ClaimReadyQueuedWork,
    ReadSessionStateVersion,
    AdmitSessionState,
    LoadKnownNode,
    LoadUnknownNode,
    GetSessionExecutionLease,
    RenewSessionExecutionLease,
    ListQueuedWork,
    ListPendingQueuedWork,
    PendingSessionWorkOrdering,
    EnqueueQueuedWorkWithOutcome,
    ClaimLeadingReadySessionCommand,
    ClaimReadyQueuedWorkByUnknownBatchIds,
    ClaimCheckpointWork,
    AbandonQueuedWorkClaims,
    QueuedWorkBatchCompleted,
    CancelQueuedWorkBatch,
    ClaimActiveTurnInputs,
    AbandonTurnInputClaim,
    AbandonTurnInputClaims,
    BindTurnInputClaim,
    BindTurnInputClaimOfReceipt,
    ReclaimTurnBoundInputs,
    CancelUnknownPendingTurnInput,
    CancelPendingTurnInputs,
    CancelPendingTurnInputSuffix,
    OrphanedActiveTurnIds,
    CommittedTurnExists,
    UncommittedTurnExists,
    PendingQueuedRun,
    QueuedRun,
    BeginOrResumeQueuedRun,
    SelectQueuedRun,
    SettleQueuedRun,
    CommitDrainEnd,
    DrainEndExists,
    /// [`SessionCommitStore::raise_pending_follow_on_attempts`], driven over a
    /// live fact for the turn it names (`owed`) and for a turn it does not.
    RaisePendingFollowOnAttempts {
        owed: bool,
    },
    /// [`SessionCommitStore::load_pending_follow_on`]: the head's owed
    /// follow-on as committed, read on an unowed head, beside a live fact,
    /// and after the fact's clearing commit.
    LoadPendingFollowOn,
    /// [`RootStore::root_terminal`](lash_core::store::RootStore::root_terminal)
    /// of the sweep's drain root: none before its settlement, and the failed
    /// settlement's evidence after it (FIG-3600 S7).
    RootTerminal,
    /// [`RootStore::root_binding`](lash_core::store::RootStore::root_binding)
    /// of the sweep's next-turn input.
    RootBinding,
    /// [`RootStore::root_of_input`](lash_core::store::RootStore::root_of_input)
    /// of the sweep's next-turn input.
    RootOfInput,
    /// [`RootStore::bind_root_inputs`](lash_core::store::RootStore::bind_root_inputs)
    /// of the sweep's next-turn input: to the sweep root, and then to another
    /// root (`conflicting`), which every backend refuses without residue.
    BindRootInputs {
        conflicting: bool,
    },
    /// [`ControlIntentStore::begin_session_close`](lash_core::store::ControlIntentStore::begin_session_close)
    /// of the case's session, or of a session no backend holds
    /// (`known_session: false`), which closes nothing (FIG-3600 S7).
    BeginSessionClose {
        known_session: bool,
    },
    /// [`ControlIntentStore::load_intent`](lash_core::store::ControlIntentStore::load_intent)
    /// of the case's close intent, or of an id no ledger minted.
    LoadIntent {
        known: bool,
    },
    /// [`ControlIntentStore::claim_intent_application`](lash_core::store::ControlIntentStore::claim_intent_application)
    /// of the case's close intent, or of an id no ledger minted, which every
    /// backend refuses without residue.
    ClaimIntentApplication {
        known: bool,
    },
    /// [`ControlIntentStore::record_intent_failure`](lash_core::store::ControlIntentStore::record_intent_failure)
    /// of the case's close intent, retryable.
    RecordIntentFailure,
    /// [`ControlIntentStore::acknowledge_intent`](lash_core::store::ControlIntentStore::acknowledge_intent)
    /// of the case's close intent, or of an id no ledger minted.
    AcknowledgeIntent {
        known: bool,
    },
    RecordRootPark,
    OpenRootIntent {
        fork: bool,
        stale: bool,
    },
    ListControlIntents,
    ListTerminalRoots,
    ListReconcilableSessions,
    AbortUnknownAttachmentWrite,
    CommitUnknownAttachmentRefs,
    ForgetUnknownAttachment,
    Vacuum,
}

impl SurfaceMethod {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::LoadSession => "surface:load_session",
            Self::ListPendingTurnInputs => "surface:list_pending_turn_inputs",
            Self::ListTurnInputApplications => "surface:list_turn_input_applications",
            Self::ClaimNextTurnInputs => "surface:claim_next_turn_inputs",
            Self::ClaimReadyQueuedWork => "surface:claim_ready_queued_work",
            Self::ReadSessionStateVersion => "surface:read_session_state_version",
            Self::AdmitSessionState => "surface:admit_session_state",
            Self::LoadKnownNode => "surface:load_node_known",
            Self::LoadUnknownNode => "surface:load_node_unknown",
            Self::GetSessionExecutionLease => "surface:get_session_execution_lease",
            Self::RenewSessionExecutionLease => "surface:renew_session_execution_lease",
            Self::ListQueuedWork => "surface:list_queued_work",
            Self::ListPendingQueuedWork => "surface:list_pending_queued_work",
            Self::PendingSessionWorkOrdering => "surface:pending_session_work_ordering",
            Self::EnqueueQueuedWorkWithOutcome => "surface:enqueue_queued_work_with_outcome",
            Self::ClaimLeadingReadySessionCommand => "surface:claim_leading_ready_session_command",
            Self::ClaimReadyQueuedWorkByUnknownBatchIds => {
                "surface:claim_ready_queued_work_by_batch_ids_unknown"
            }
            Self::ClaimCheckpointWork => "surface:claim_checkpoint_work",
            Self::AbandonQueuedWorkClaims => "surface:abandon_queued_work_claims",
            Self::QueuedWorkBatchCompleted => "surface:queued_work_batch_completed",
            Self::CancelQueuedWorkBatch => "surface:cancel_queued_work_batch",
            Self::ClaimActiveTurnInputs => "surface:claim_active_turn_inputs",
            Self::AbandonTurnInputClaim => "surface:abandon_turn_input_claim",
            Self::AbandonTurnInputClaims => "surface:abandon_turn_input_claims",
            Self::BindTurnInputClaim => "surface:bind_turn_input_claim",
            Self::BindTurnInputClaimOfReceipt => "surface:bind_turn_input_claim_of_receipt",
            Self::ReclaimTurnBoundInputs => "surface:reclaim_turn_bound_inputs",
            Self::CancelUnknownPendingTurnInput => "surface:cancel_pending_turn_input_unknown",
            Self::CancelPendingTurnInputs => "surface:cancel_pending_turn_inputs",
            Self::CancelPendingTurnInputSuffix => "surface:cancel_pending_turn_input_suffix",
            Self::OrphanedActiveTurnIds => "surface:orphaned_active_turn_ids",
            Self::CommittedTurnExists => "surface:committed_turn_exists_committed",
            Self::UncommittedTurnExists => "surface:committed_turn_exists_uncommitted",
            Self::PendingQueuedRun => "surface:pending_queued_run",
            Self::QueuedRun => "surface:queued_run",
            Self::BeginOrResumeQueuedRun => "surface:begin_or_resume_queued_run",
            Self::SelectQueuedRun => "surface:select_queued_run",
            Self::SettleQueuedRun => "surface:settle_queued_run",
            Self::CommitDrainEnd => "surface:commit_drain_end_receipt",
            Self::DrainEndExists => "surface:drain_end_exists",
            Self::RaisePendingFollowOnAttempts { owed: true } => {
                "surface:raise_pending_follow_on_attempts"
            }
            Self::RaisePendingFollowOnAttempts { owed: false } => {
                "surface:raise_pending_follow_on_attempts_unowed"
            }
            Self::LoadPendingFollowOn => "surface:load_pending_follow_on",
            Self::RootTerminal => "surface:root_terminal",
            Self::RootBinding => "surface:root_binding",
            Self::RootOfInput => "surface:root_of_input",
            Self::BindRootInputs { conflicting: false } => "surface:bind_root_inputs",
            Self::BindRootInputs { conflicting: true } => "surface:bind_root_inputs_conflicting",
            Self::BeginSessionClose {
                known_session: true,
            } => "surface:begin_session_close",
            Self::BeginSessionClose {
                known_session: false,
            } => "surface:begin_session_close_unknown_session",
            Self::LoadIntent { known: true } => "surface:load_intent",
            Self::LoadIntent { known: false } => "surface:load_intent_unknown",
            Self::ClaimIntentApplication { known: true } => "surface:claim_intent_application",
            Self::ClaimIntentApplication { known: false } => {
                "surface:claim_intent_application_unknown"
            }
            Self::RecordIntentFailure => "surface:record_intent_failure",
            Self::AcknowledgeIntent { known: true } => "surface:acknowledge_intent",
            Self::AcknowledgeIntent { known: false } => "surface:acknowledge_intent_unknown",
            Self::RecordRootPark => "surface:record_root_park",
            Self::OpenRootIntent { stale: true, .. } => "surface:open_root_intent_stale",
            Self::OpenRootIntent {
                stale: false,
                fork: false,
            } => "surface:open_root_intent_cancel",
            Self::OpenRootIntent {
                stale: false,
                fork: true,
            } => "surface:open_root_intent_fork",
            Self::ListControlIntents => "surface:list_control_intents",
            Self::ListTerminalRoots => "surface:list_terminal_roots",
            Self::ListReconcilableSessions => "surface:list_reconcilable_sessions",
            Self::AbortUnknownAttachmentWrite => "surface:abort_attachment_write_unknown",
            Self::CommitUnknownAttachmentRefs => "surface:commit_refs_unknown",
            Self::ForgetUnknownAttachment => "surface:forget_attachment_unknown",
            Self::Vacuum => "surface:vacuum",
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn unknown_attachment_id() -> AttachmentId {
    AttachmentId::parse(UNKNOWN_ATTACHMENT_ID).expect("the unknown-attachment id must parse")
}

fn unknown_attachment_intent(session_id: &SessionId) -> lash_core::AttachmentIntent {
    lash_core::AttachmentIntent {
        attachment_id: unknown_attachment_id(),
        session_id: session_id.clone(),
        canonical_uri: format!("lash-attachment://blake3/{UNKNOWN_ATTACHMENT_ID}"),
        intent_at_epoch_ms: 1_000,
        owner: None,
    }
}

/// The read status of every pending turn input, in queue order, with the ids
/// the backends mint left out: what a bind or reclaim changed durably.
async fn turn_input_statuses(
    store: &Arc<dyn ConformancePersistence>,
    session_id: &SessionId,
) -> Result<String, StoreError> {
    let statuses = store
        .list_pending_turn_inputs(session_id)
        .await?
        .into_iter()
        .map(|read| match read.status {
            lash_core::PendingTurnInputReadStatus::Pending => "pending".to_string(),
            lash_core::PendingTurnInputReadStatus::Held { .. } => "held".to_string(),
            lash_core::PendingTurnInputReadStatus::TurnBound {
                turn_id,
                receipt_input_id,
            } => format!(
                "turn_bound(turn={turn_id:?},receipt_is_row={})",
                receipt_input_id == read.input.input_id
            ),
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>();
    Ok(format!("statuses={statuses:?}"))
}

fn surface(method: SurfaceMethod) -> StoreOperation {
    StoreOperation::DriveSurface { method }
}

const UNKNOWN_BATCH_ID: &str = "fig-2841-unknown-batch";
/// A turn id no case ever commits. Paired with [`SURFACE_COMMITTED_TURN_ID`]
/// so the membership read is driven over both answers, not just the one a
/// backend could return by refusing to look.
const UNCOMMITTED_TURN_ID: &str = "fig-2841-uncommitted-turn";
/// The turn id the surface sweep's seed commit stamps.
const SURFACE_COMMITTED_TURN_ID: &str = "fig-2841-surface-committed-turn";
/// An attachment id no case ever writes. The manifest drivers use
/// it so the inventory covers those methods without mutating an attachment the
/// surrounding case depends on: an unknown entity is itself a refusal driver,
/// and whatever a backend answers, the no-residue law still applies.
const UNKNOWN_ATTACHMENT_ID: &str = "fig-2841-unknown-attachment";
const UNKNOWN_INPUT_ID: &str = "fig-2841-unknown-input";
/// The root the sweep binds its next-turn input to, and the other root a
/// second binding names.
const SURFACE_ROOT_ID: &str = "fig-3600-root";
const SURFACE_OTHER_ROOT_ID: &str = "fig-3600-other-root";
/// A session no case ever creates: closing it closes nothing.
const UNKNOWN_CLOSE_SESSION_ID: &str = "fig-3600-never-created-session";
/// An intent id no ledger mints within this run: the unknown-intent refusal
/// driver. Every backend's intent clock stays far below it.
const UNKNOWN_INTENT_SEQUENCE: u64 = 9_000_000_000_000_000_000;
/// The instants the close case hands the ledger: its store half, a retained
/// failure, and the acknowledgement.
const CLOSE_AT_MS: u64 = 5_000;
const CLOSE_FAILED_AT_MS: u64 = 6_000;
const CLOSE_ACKNOWLEDGED_AT_MS: u64 = 7_000;
/// The aborted direct turn the sweep binds its drive claim to (FIG-3589).
const SURFACE_ABORTED_TURN_ID: &str = "fig-3589-surface-aborted-turn";
/// The queue drain the sweep admits, selects, settles and ends. Its scope is
/// the run's caller-supplied identity, so every backend admits the same
/// `QueueDrain` scope and the reads before admission ask about a drain that
/// genuinely does not exist yet.
const SURFACE_DRAIN_ID: &str = "fig-2841-surface-drain";
/// The turn whose terminal commit writes the follow-on fact (ADR 0101 §3).
const SURFACE_FOLLOW_ON_SWITCH_TURN_ID: &str = "fig-2841-surface-switch-turn";
/// The follow-on turn that fact owes: the one `raise_pending_follow_on_attempts`
/// may raise, and the one whose own terminal commit clears it.
const SURFACE_FOLLOW_ON_TURN_ID: &str = "fig-2841-surface-follow-on";
/// A turn no follow-on fact ever names: the raise's `FollowOnNotPending`
/// refusal path over a live fact.
const SURFACE_UNOWED_FOLLOW_ON_TURN_ID: &str = "fig-2841-unowed-follow-on";

fn surface_drain_scope(session_id: &SessionId) -> lash_core::ExecutionScope {
    lash_core::ExecutionScope::queue_drain(session_id.clone(), SURFACE_DRAIN_ID)
}

fn surface_queued_run_configuration(session_id: &SessionId) -> lash_core::PersistedSessionConfig {
    let state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    RuntimeCommit::persisted_state_for_test(&state, &[]).config
}

/// A coarse, backend-neutral summary of a queued-run admission: the drain id
/// a backend mints is compared as "is it the scope we named", never by value.
fn queued_run_summary(
    admission: &lash_core::store::QueuedRunAdmission,
    session_id: &SessionId,
) -> String {
    let terminal = match &admission.terminal {
        None => "none",
        Some(lash_core::store::QueuedRunTerminal::Completed { .. }) => "completed",
        Some(lash_core::store::QueuedRunTerminal::Empty) => "empty",
        Some(lash_core::store::QueuedRunTerminal::Failed { .. }) => "failed",
    };
    format!(
        "named_scope={} origin={:?} request={:?} revision={} physical_ordinal={} turn_index={} \
         members={:?} initial_members={:?} withheld={} assigned={} terminal={terminal}",
        admission.scope == surface_drain_scope(session_id),
        admission.origin,
        admission.request,
        admission.revision,
        admission.position.physical_ordinal,
        admission.position.turn_index,
        admission.members.as_ref().map(Vec::len),
        admission.initial_members.as_ref().map(Vec::len),
        admission.withheld_members.len(),
        admission.assigned_members.len(),
    )
}

/// A backend-neutral summary of a control intent: its id and session are
/// compared as "the case's own", never by value.
fn control_intent_summary(
    intent: &lash_core::store::ControlIntent,
    session_id: &SessionId,
    own: Option<lash_core::store::ControlIntentId>,
) -> String {
    let state = match &intent.state {
        lash_core::store::ControlIntentState::Superseded { by } => {
            format!("superseded(by_own={})", Some(*by) == own)
        }
        other => format!("{other:?}"),
    };
    let kind = match &intent.kind {
        lash_core::store::ControlIntentKind::Cancel { root, .. } => {
            format!("Cancel {{ root: {root} }}")
        }
        lash_core::store::ControlIntentKind::Redrive { root, .. } => {
            format!("Redrive {{ root: {root} }}")
        }
        lash_core::store::ControlIntentKind::Fork { root, new_root, .. } => format!(
            "Fork {{ root: {root}, direct: {}, derived_root: {} }}",
            new_root.is_some(),
            new_root
                .as_ref()
                .is_none_or(|new| *new == lash_core::store::forked_root(root, intent.id)),
        ),
        other => format!("{other:?}"),
    };
    format!(
        "own={} own_session={} format={} kind={kind} state={state} attempts={} created_at_ms={} \
         engine={:?}",
        Some(intent.id) == own,
        intent.session_id == *session_id,
        intent.format,
        intent.attempts,
        intent.created_at_ms,
        intent.engine,
    )
}

/// Every fallible method in the inventory, driven against a live session.
pub(super) fn surface_sweep_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::StoreSurfaceSweep,
        operations: vec![
            // The seed commit stamps a turn so the inventory can drive
            // `committed_turn_exists` over a turn that is actually committed.
            StoreOperation::Commit {
                label: "seed_surface_sweep_graph",
                expected_head_revision: 0,
                graph: append(
                    vec![
                        NodeSpec::new("root", None, "root"),
                        NodeSpec::new("active-frame", Some("root"), "active"),
                    ],
                    Some("active-frame"),
                ),
                turn_commit: Some(TurnCommitSpec {
                    turn_id: SURFACE_COMMITTED_TURN_ID,
                }),
                checkpoint: CheckpointSpec::Empty,
                usage: false,
                adopt_attachment: false,
            },
            StoreOperation::EnqueueNextTurnInput,
            StoreOperation::EnqueueClaimableQueuedWork,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "surface-sweep-owner",
            },
            surface(SurfaceMethod::ReadSessionStateVersion),
            surface(SurfaceMethod::AdmitSessionState),
            surface(SurfaceMethod::LoadKnownNode),
            surface(SurfaceMethod::LoadUnknownNode),
            surface(SurfaceMethod::GetSessionExecutionLease),
            surface(SurfaceMethod::RenewSessionExecutionLease),
            surface(SurfaceMethod::ListQueuedWork),
            surface(SurfaceMethod::ListPendingQueuedWork),
            surface(SurfaceMethod::PendingSessionWorkOrdering),
            surface(SurfaceMethod::EnqueueQueuedWorkWithOutcome),
            surface(SurfaceMethod::ClaimLeadingReadySessionCommand),
            surface(SurfaceMethod::AbandonQueuedWorkClaims),
            surface(SurfaceMethod::ClaimReadyQueuedWorkByUnknownBatchIds),
            surface(SurfaceMethod::ClaimCheckpointWork),
            surface(SurfaceMethod::AbandonQueuedWorkClaims),
            surface(SurfaceMethod::QueuedWorkBatchCompleted),
            surface(SurfaceMethod::CancelQueuedWorkBatch),
            surface(SurfaceMethod::ClaimActiveTurnInputs),
            surface(SurfaceMethod::AbandonTurnInputClaim),
            surface(SurfaceMethod::AbandonTurnInputClaims),
            surface(SurfaceMethod::OrphanedActiveTurnIds),
            surface(SurfaceMethod::CommittedTurnExists),
            surface(SurfaceMethod::UncommittedTurnExists),
            // One queue drain end to end (FIG-3419): nothing pending and the
            // named drain unknown, then admission, selection, a Failed
            // settlement, and the end receipt. `drain_end_exists` is driven
            // on both sides of that receipt, so a backend that answers
            // `false` without looking cannot agree.
            surface(SurfaceMethod::PendingQueuedRun),
            surface(SurfaceMethod::QueuedRun),
            surface(SurfaceMethod::BeginOrResumeQueuedRun),
            surface(SurfaceMethod::PendingQueuedRun),
            surface(SurfaceMethod::BeginOrResumeQueuedRun),
            surface(SurfaceMethod::SelectQueuedRun),
            surface(SurfaceMethod::RootTerminal),
            surface(SurfaceMethod::SettleQueuedRun),
            // The failed settlement wrote its root's evidence (FIG-3600 S7).
            surface(SurfaceMethod::RootTerminal),
            surface(SurfaceMethod::QueuedRun),
            surface(SurfaceMethod::PendingQueuedRun),
            surface(SurfaceMethod::DrainEndExists),
            surface(SurfaceMethod::CommitDrainEnd),
            surface(SurfaceMethod::DrainEndExists),
            // An input's root binding: unbound, bound once, read back, and a
            // second binding to another root refused.
            surface(SurfaceMethod::RootBinding),
            surface(SurfaceMethod::BindRootInputs { conflicting: false }),
            surface(SurfaceMethod::RootBinding),
            surface(SurfaceMethod::RootOfInput),
            surface(SurfaceMethod::BindRootInputs { conflicting: false }),
            surface(SurfaceMethod::BindRootInputs { conflicting: true }),
            surface(SurfaceMethod::RootBinding),
            surface(SurfaceMethod::CancelUnknownPendingTurnInput),
            surface(SurfaceMethod::CancelPendingTurnInputSuffix),
            surface(SurfaceMethod::CancelPendingTurnInputs),
            surface(SurfaceMethod::AbortUnknownAttachmentWrite),
            surface(SurfaceMethod::CommitUnknownAttachmentRefs),
            surface(SurfaceMethod::ForgetUnknownAttachment),
            // With no pending follow-on on the head the raise meets its
            // `FollowOnNotPending` refusal.
            surface(SurfaceMethod::RaisePendingFollowOnAttempts { owed: true }),
            surface(SurfaceMethod::LoadPendingFollowOn),
            surface(SurfaceMethod::Vacuum),
        ],
    }
}

/// `raise_pending_follow_on_attempts` over a live fact (ADR 0101 §3): a
/// frame-switch terminal commit leaves the head owing a follow-on, the held
/// lease raises its attempts count twice without moving the head revision,
/// a raise for a turn the fact does not name refuses, and the follow-on's
/// own terminal commit clears it — after which the raise refuses again.
/// `load_pending_follow_on` reads the fact the same commits leave: on the
/// unowed head, beside the live fact, after a raise, and after the clear.
pub(super) fn pending_follow_on_raise_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::PendingFollowOnRaise,
        operations: vec![
            commit(
                "seed_follow_on_frame",
                0,
                append(
                    vec![
                        NodeSpec::new("root", None, "root"),
                        NodeSpec::new("active-frame", Some("root"), "active"),
                    ],
                    Some("active-frame"),
                ),
            ),
            // The seeded head owes no follow-on.
            surface(SurfaceMethod::LoadPendingFollowOn),
            StoreOperation::CommitFollowOn {
                label: "frame_switch_leaves_pending_follow_on",
                expected_head_revision: 1,
                turn_id: SURFACE_FOLLOW_ON_SWITCH_TURN_ID,
                owed_turn_id: Some(SURFACE_FOLLOW_ON_TURN_ID),
            },
            // The switch left the fact on the head, still at zero attempts.
            surface(SurfaceMethod::LoadPendingFollowOn),
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "follow-on-raise-owner",
            },
            surface(SurfaceMethod::RaisePendingFollowOnAttempts { owed: true }),
            // The raise is a committed head fact: the read sees attempts=1.
            surface(SurfaceMethod::LoadPendingFollowOn),
            surface(SurfaceMethod::RaisePendingFollowOnAttempts { owed: true }),
            surface(SurfaceMethod::RaisePendingFollowOnAttempts { owed: false }),
            StoreOperation::CommitFollowOn {
                label: "follow_on_terminal_clears_pending_follow_on",
                expected_head_revision: 2,
                turn_id: SURFACE_FOLLOW_ON_TURN_ID,
                owed_turn_id: None,
            },
            // The follow-on's own terminal commit cleared the fact.
            surface(SurfaceMethod::LoadPendingFollowOn),
            surface(SurfaceMethod::RaisePendingFollowOnAttempts { owed: true }),
        ],
    }
}

/// An aborted direct turn's drive claim, bound and re-taken (FIG-3589).
///
/// The sweep drives with the first lease slot, so each lane turnover
/// re-acquires that slot under a new owner: a reclaim under the generation
/// that bound the claim defers, and one under a successor generation re-takes
/// the rows. Both binds are driven: with the claim the turn knows, and by the
/// receipt's row under the generation a lost drive ran under. The closing
/// abandon returns the row to the queue.
pub(super) fn turn_bound_claim_case() -> GeneratedCase {
    fn turnover(owner: &'static str) -> [StoreOperation; 2] {
        [
            StoreOperation::ReleaseSessionLease {
                lease: LeaseSlot::First,
            },
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner,
            },
        ]
    }
    let mut operations = vec![
        StoreOperation::EnqueueNextTurnInput,
        StoreOperation::AcquireSessionLease {
            slot: LeaseSlot::First,
            owner: "turn-bound-drive-owner",
        },
        surface(SurfaceMethod::ClaimNextTurnInputs),
        surface(SurfaceMethod::BindTurnInputClaim),
        surface(SurfaceMethod::ReclaimTurnBoundInputs),
    ];
    operations.extend(turnover("turn-bound-redrive-owner"));
    operations.extend([
        surface(SurfaceMethod::ReclaimTurnBoundInputs),
        surface(SurfaceMethod::BindTurnInputClaimOfReceipt),
        surface(SurfaceMethod::ReclaimTurnBoundInputs),
    ]);
    operations.extend(turnover("turn-bound-second-redrive-owner"));
    operations.extend([
        surface(SurfaceMethod::ReclaimTurnBoundInputs),
        surface(SurfaceMethod::AbandonTurnInputClaim),
        surface(SurfaceMethod::ListPendingTurnInputs),
    ]);
    GeneratedCase {
        name: CaseName::TurnBoundClaimBindAndReclaim,
        operations,
    }
}

/// The same surface after the session is gone: a refusal driver whose whole
/// point is that nothing it refuses may leave a durable trace.
pub(super) fn refused_surface_on_deleted_session_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::RefusedSurfaceOnDeletedSession,
        operations: vec![
            commit(
                "seed_deleted_session_surface_graph",
                0,
                append(
                    vec![
                        NodeSpec::new("root", None, "root"),
                        NodeSpec::new("active-frame", Some("root"), "active"),
                    ],
                    Some("active-frame"),
                ),
            ),
            StoreOperation::EnqueueNextTurnInput,
            StoreOperation::EnqueueClaimableQueuedWork,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "deleted-surface-owner",
            },
            StoreOperation::DeleteSession,
            surface(SurfaceMethod::ReadSessionStateVersion),
            surface(SurfaceMethod::LoadUnknownNode),
            surface(SurfaceMethod::GetSessionExecutionLease),
            surface(SurfaceMethod::RenewSessionExecutionLease),
            surface(SurfaceMethod::ListQueuedWork),
            surface(SurfaceMethod::ListPendingQueuedWork),
            surface(SurfaceMethod::PendingSessionWorkOrdering),
            surface(SurfaceMethod::EnqueueQueuedWorkWithOutcome),
            surface(SurfaceMethod::ClaimLeadingReadySessionCommand),
            surface(SurfaceMethod::CancelUnknownPendingTurnInput),
            surface(SurfaceMethod::PendingQueuedRun),
            surface(SurfaceMethod::QueuedRun),
            surface(SurfaceMethod::BeginOrResumeQueuedRun),
            surface(SurfaceMethod::DrainEndExists),
            surface(SurfaceMethod::RaisePendingFollowOnAttempts { owed: true }),
            surface(SurfaceMethod::LoadPendingFollowOn),
            surface(SurfaceMethod::CancelQueuedWorkBatch),
            surface(SurfaceMethod::CommitUnknownAttachmentRefs),
            surface(SurfaceMethod::ForgetUnknownAttachment),
            surface(SurfaceMethod::Vacuum),
        ],
    }
}

/// A session's close through the factory's control-intent ledger (FIG-3600
/// S7): the store half ends the session's open roots — an input's bound
/// root and a pending queue drain — `Cancelled` by the close, a retry
/// answers the kept intent, and the engine half's lifecycle is claimed,
/// failed retryably, claimed again, acknowledged and then done. An unknown
/// session closes nothing, and an unknown intent is refused without residue.
pub(super) fn session_close_ledger_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::SessionCloseLedger,
        operations: vec![
            commit(
                "seed_session_close_graph",
                0,
                append(
                    vec![
                        NodeSpec::new("root", None, "root"),
                        NodeSpec::new("active-frame", Some("root"), "active"),
                    ],
                    Some("active-frame"),
                ),
            ),
            StoreOperation::EnqueueNextTurnInput,
            StoreOperation::EnqueueClaimableQueuedWork,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "session-close-owner",
            },
            surface(SurfaceMethod::BindRootInputs { conflicting: false }),
            surface(SurfaceMethod::BeginOrResumeQueuedRun),
            surface(SurfaceMethod::RootTerminal),
            surface(SurfaceMethod::BeginSessionClose {
                known_session: false,
            }),
            surface(SurfaceMethod::LoadIntent { known: false }),
            surface(SurfaceMethod::ClaimIntentApplication { known: false }),
            surface(SurfaceMethod::AcknowledgeIntent { known: false }),
            surface(SurfaceMethod::BeginSessionClose {
                known_session: true,
            }),
            // A retried store half answers the kept intent and writes nothing.
            surface(SurfaceMethod::BeginSessionClose {
                known_session: true,
            }),
            // The close ended the drain root by the session's deletion.
            surface(SurfaceMethod::RootTerminal),
            surface(SurfaceMethod::PendingQueuedRun),
            surface(SurfaceMethod::LoadIntent { known: true }),
            surface(SurfaceMethod::ClaimIntentApplication { known: true }),
            surface(SurfaceMethod::RecordIntentFailure),
            surface(SurfaceMethod::ClaimIntentApplication { known: true }),
            surface(SurfaceMethod::AcknowledgeIntent { known: true }),
            surface(SurfaceMethod::ClaimIntentApplication { known: true }),
            // A late failure never reopens an acknowledged intent.
            surface(SurfaceMethod::RecordIntentFailure),
            surface(SurfaceMethod::AcknowledgeIntent { known: true }),
            surface(SurfaceMethod::LoadIntent { known: true }),
        ],
    }
}

pub(super) fn root_control_case(fork: bool) -> GeneratedCase {
    let mut case = session_close_ledger_case();
    case.name = if fork {
        CaseName::RootForkLedger
    } else {
        CaseName::RootCancelLedger
    };
    case.operations.truncate(5);
    case.operations.extend([
        surface(SurfaceMethod::RecordRootPark),
        surface(SurfaceMethod::OpenRootIntent { fork, stale: true }),
        surface(SurfaceMethod::OpenRootIntent { fork, stale: false }),
        surface(SurfaceMethod::LoadIntent { known: true }),
        surface(SurfaceMethod::ListControlIntents),
        surface(SurfaceMethod::ListTerminalRoots),
        surface(SurfaceMethod::ListReconcilableSessions),
        surface(SurfaceMethod::ListPendingTurnInputs),
        surface(SurfaceMethod::RootBinding),
        surface(SurfaceMethod::ClaimIntentApplication { known: true }),
        surface(SurfaceMethod::AcknowledgeIntent { known: true }),
    ]);
    case
}

impl BackendRunner {
    /// Drive one inventory method. Returns `Err` verbatim: the step loop owns
    /// the refusal comparison and the no-residue check.
    pub(super) async fn drive_surface(
        &mut self,
        method: SurfaceMethod,
    ) -> Result<Option<ComparableRuntimeCommitResult>, StoreError> {
        let store = self.store();
        let session_id = self.session_id.clone();
        // Lease credentials are copied out up front: the sweep mutates its own
        // scratch state inside these arms, which cannot hold a borrow of self.
        let lease_credentials = self.first_lease.as_ref().map(|lease| {
            (
                lease.owner.clone(),
                lease.fence(),
                lease.completion().fencing_token,
            )
        });
        let (lease_owner, lease_fence) = lease_credentials
            .map(|(owner, fence, _)| (owner, fence))
            .unwrap_or_else(|| {
                (
                    LeaseOwnerIdentity::opaque(
                        "fig-2841-no-lease",
                        "fig-2841-no-lease:incarnation",
                    ),
                    unheld_lease_fence(&session_id),
                )
            });
        let answer = match method {
            SurfaceMethod::LoadSession => {
                format!("present={}", store.load_session().await?.is_some())
            }
            SurfaceMethod::ListPendingTurnInputs => {
                format!(
                    "rows={}",
                    store.list_pending_turn_inputs(&session_id).await?.len()
                )
            }
            SurfaceMethod::ListTurnInputApplications => {
                format!(
                    "rows={}",
                    store.list_turn_input_applications(&session_id).await?.len()
                )
            }
            SurfaceMethod::ClaimNextTurnInputs => {
                let claim = store
                    .claim_next_turn_inputs(&session_id, &lease_fence, &lease_owner, 1)
                    .await?;
                let claimed = claim.is_some();
                if let Some(claim) = claim {
                    self.surface.turn_input_claim = Some(claim);
                }
                format!("claimed={claimed}")
            }
            SurfaceMethod::ClaimReadyQueuedWork => {
                let outcome = store
                    .claim_ready_queued_work(
                        &session_id,
                        &lease_fence,
                        &lease_owner,
                        QueuedWorkClaimBoundary::Idle,
                        lash_core::testing::queued_work_claim_policy(1),
                    )
                    .await?;
                let claim = outcome.claim();
                let claimed = claim.is_some();
                if let Some(claim) = claim {
                    self.surface.queued_work_claim = Some(claim);
                }
                format!("claimed={claimed}")
            }
            SurfaceMethod::ReadSessionStateVersion => {
                format!("version={}", store.read_session_state_version().await?)
            }
            SurfaceMethod::AdmitSessionState => {
                let admission = store.admit_session_state(&lease_fence).await?;
                format!("version={}", admission.version)
            }
            SurfaceMethod::LoadKnownNode => {
                let node_id = scoped_node_id(&session_id, "root");
                let node = store.load_node(&node_id).await?;
                format!("present={}", node.is_some())
            }
            SurfaceMethod::LoadUnknownNode => {
                let node = store.load_node("fig-2841-unknown-node").await?;
                format!("present={}", node.is_some())
            }
            SurfaceMethod::GetSessionExecutionLease => {
                let observation = store.get_session_execution_lease(&session_id).await?;
                format!("lease_present={}", observation.lease.is_some())
            }
            SurfaceMethod::RenewSessionExecutionLease => {
                let renewed = store
                    .renew_session_execution_lease(&lease_fence, SESSION_LEASE_TTL_MS)
                    .await?;
                format!("fencing_token={}", renewed.fencing_token)
            }
            SurfaceMethod::ListQueuedWork => {
                format!("rows={}", store.list_queued_work(&session_id).await?.len())
            }
            SurfaceMethod::ListPendingQueuedWork => {
                format!(
                    "rows={}",
                    store.list_pending_queued_work(&session_id).await?.len()
                )
            }
            SurfaceMethod::PendingSessionWorkOrdering => {
                let ordering = store.pending_session_work_ordering(&session_id).await?;
                format!(
                    "session_command={} turn_input={}",
                    ordering.session_command.is_some(),
                    ordering.turn_input.is_some()
                )
            }
            SurfaceMethod::EnqueueQueuedWorkWithOutcome => {
                let outcome = store
                    .enqueue_queued_work_with_outcome(
                        QueuedWorkBatchDraft::new(
                            &session_id,
                            DeliveryPolicy::EarliestSafeBoundary,
                            lash_core::facade_support::SessionCommand::RefreshToolCatalog {
                                reason: "fig-2841 surface sweep".to_string(),
                            },
                        )
                        .with_source_key("fig-2841-surface-sweep"),
                    )
                    .await?;
                let batch = outcome.into_batch();
                self.surface.batch_id = Some(batch.batch_id.to_string());
                format!("source_key={:?}", batch.source_key)
            }
            SurfaceMethod::ClaimLeadingReadySessionCommand => {
                let claim = store
                    .claim_leading_ready_session_command(&session_id, &lease_fence, &lease_owner)
                    .await?;
                let claimed = claim.is_some();
                if let Some(claim) = claim {
                    self.surface.queued_work_claim = Some(claim);
                }
                format!("claimed={claimed}")
            }
            SurfaceMethod::ClaimReadyQueuedWorkByUnknownBatchIds => {
                let outcome = store
                    .claim_ready_queued_work_by_batch_ids(
                        &session_id,
                        &lease_fence,
                        &lease_owner,
                        QueuedWorkClaimBoundary::Idle,
                        &[UNKNOWN_BATCH_ID.into()],
                        lash_core::testing::queued_work_claim_policy(1),
                    )
                    .await?;
                format!(
                    "claimed={} already_satisfied={}",
                    outcome.claim.is_some(),
                    outcome.already_satisfied_batch_ids.len()
                )
            }
            SurfaceMethod::ClaimCheckpointWork => {
                let (input_claim, work_claim) = store
                    .claim_checkpoint_work(
                        &session_id,
                        &lease_fence,
                        &lease_owner,
                        &lash_core::TurnId::from("fig-2841-surface-turn"),
                        lash_core::CheckpointKind::AfterWork,
                        1,
                        lash_core::testing::queued_work_claim_policy(1),
                    )
                    .await?;
                let summary = format!(
                    "input_claim={} work_claim={}",
                    input_claim.is_some(),
                    work_claim.is_some()
                );
                if let Some(claim) = input_claim {
                    self.surface.turn_input_claim = Some(claim);
                }
                if let Some(claim) = work_claim {
                    self.surface.queued_work_claim = Some(claim);
                }
                summary
            }
            SurfaceMethod::AbandonQueuedWorkClaims => {
                let claims: Vec<QueuedWorkClaim> =
                    self.surface.queued_work_claim.take().into_iter().collect();
                let count = claims.len();
                store.abandon_queued_work_claims(&claims).await?;
                format!("abandoned={count}")
            }
            SurfaceMethod::QueuedWorkBatchCompleted => {
                let completed = store
                    .queued_work_batch_completed(&session_id, UNKNOWN_BATCH_ID)
                    .await?;
                format!("completed={completed}")
            }
            SurfaceMethod::CancelQueuedWorkBatch => {
                let batch_id = self
                    .surface
                    .batch_id
                    .clone()
                    .unwrap_or_else(|| UNKNOWN_BATCH_ID.to_string());
                let cancelled = store
                    .cancel_queued_work_batch(&session_id, &batch_id)
                    .await?;
                format!("cancelled={}", cancelled.is_some())
            }
            SurfaceMethod::ClaimActiveTurnInputs => {
                let claim = store
                    .claim_active_turn_inputs(
                        &session_id,
                        &lease_fence,
                        &lease_owner,
                        &lash_core::TurnId::from("fig-2841-surface-turn"),
                        lash_core::CheckpointKind::AfterWork,
                        1,
                    )
                    .await?;
                let claimed = claim.is_some();
                if let Some(claim) = claim {
                    self.surface.turn_input_claim = Some(claim);
                }
                format!("claimed={claimed}")
            }
            SurfaceMethod::AbandonTurnInputClaim => match self.surface.turn_input_claim.take() {
                Some(claim) => {
                    store.abandon_turn_input_claim(&claim).await?;
                    "abandoned=1".to_string()
                }
                None => "abandoned=0".to_string(),
            },
            SurfaceMethod::AbandonTurnInputClaims => {
                let claims: Vec<TurnInputClaim> =
                    self.surface.turn_input_claim.take().into_iter().collect();
                let count = claims.len();
                store.abandon_turn_input_claims(&claims).await?;
                format!("abandoned={count}")
            }
            SurfaceMethod::BindTurnInputClaim => match self.surface.turn_input_claim.take() {
                Some(claim) => {
                    let receipt = claim.inputs[0].input_id.clone();
                    store
                        .bind_turn_input_claim(
                            &claim,
                            &lash_core::TurnId::from(SURFACE_ABORTED_TURN_ID),
                            &receipt,
                        )
                        .await?;
                    turn_input_statuses(&store, &session_id).await?
                }
                None => "no_claim".to_string(),
            },
            SurfaceMethod::BindTurnInputClaimOfReceipt => {
                match self.surface.turn_input_claim.take() {
                    Some(claim) => {
                        store
                            .bind_turn_input_claim_of_receipt(
                                &session_id,
                                &claim.inputs[0].input_id,
                                lease_fence.fencing_token,
                                &lash_core::TurnId::from(SURFACE_ABORTED_TURN_ID),
                            )
                            .await?;
                        turn_input_statuses(&store, &session_id).await?
                    }
                    None => "no_claim".to_string(),
                }
            }
            SurfaceMethod::ReclaimTurnBoundInputs => {
                let claim = store
                    .reclaim_turn_bound_inputs(
                        &session_id,
                        &lease_fence,
                        &lease_owner,
                        &lash_core::TurnId::from(SURFACE_ABORTED_TURN_ID),
                    )
                    .await?;
                let reclaimed = claim.as_ref().map_or(0, |claim| claim.inputs.len());
                if let Some(claim) = claim {
                    self.surface.turn_input_claim = Some(claim);
                }
                format!(
                    "reclaimed={reclaimed} {}",
                    turn_input_statuses(&store, &session_id).await?
                )
            }
            SurfaceMethod::CancelUnknownPendingTurnInput => {
                let outcome = store
                    .cancel_pending_turn_input(&session_id, UNKNOWN_INPUT_ID)
                    .await?;
                format!(
                    "not_found={}",
                    matches!(outcome, lash_core::PendingTurnInputCancelOutcome::NotFound)
                )
            }
            SurfaceMethod::CancelPendingTurnInputs => {
                let targets = vec![lash_core::PendingTurnInputCancelTarget::input_id(format!(
                    "{session_id}:input"
                ))];
                let receipts = store
                    .cancel_pending_turn_inputs(&session_id, &targets)
                    .await?;
                format!("receipts={}", receipts.len())
            }
            SurfaceMethod::CancelPendingTurnInputSuffix => {
                let anchor = lash_core::PendingTurnInputCancelTarget::input_id(UNKNOWN_INPUT_ID);
                let outcome = store
                    .cancel_pending_turn_input_suffix(&session_id, &anchor)
                    .await?;
                match outcome {
                    lash_core::PendingTurnInputSuffixCancelOutcome::AnchorNotFound { .. } => {
                        "anchor_not_found".to_string()
                    }
                    lash_core::PendingTurnInputSuffixCancelOutcome::Outcomes {
                        outcomes, ..
                    } => format!("outcomes={}", outcomes.len()),
                }
            }
            SurfaceMethod::OrphanedActiveTurnIds => {
                let turn_ids = store
                    .orphaned_active_turn_ids(
                        &session_id,
                        &lease_fence,
                        lash_core::store::OrphanedTurnInputScope::LaneGeneration {
                            resumable_turn_id: None,
                        },
                    )
                    .await?;
                format!("turn_ids={}", turn_ids.len())
            }
            SurfaceMethod::CommittedTurnExists => {
                let exists = store
                    .committed_turn_exists(&lash_core::TurnId::from(SURFACE_COMMITTED_TURN_ID))
                    .await?;
                format!("exists={exists}")
            }
            SurfaceMethod::UncommittedTurnExists => {
                let exists = store
                    .committed_turn_exists(&lash_core::TurnId::from(UNCOMMITTED_TURN_ID))
                    .await?;
                format!("exists={exists}")
            }
            SurfaceMethod::PendingQueuedRun => match store.pending_queued_run(&session_id).await? {
                Some(admission) => {
                    format!("pending={}", queued_run_summary(&admission, &session_id))
                }
                None => "pending=none".to_string(),
            },
            SurfaceMethod::QueuedRun => {
                match store.queued_run(&surface_drain_scope(&session_id)).await? {
                    Some(admission) => {
                        format!("recorded={}", queued_run_summary(&admission, &session_id))
                    }
                    None => "recorded=none".to_string(),
                }
            }
            SurfaceMethod::BeginOrResumeQueuedRun => {
                // The head the admission is fenced against is read, not
                // assumed: the second drive of this step is a resume, which
                // must answer the same admission whatever the head is.
                let expected_head_revision = store
                    .load_session_head_meta()
                    .await?
                    .map_or(0, |head| head.head_revision);
                let admission = store
                    .begin_or_resume_queued_run(
                        &lease_fence,
                        lash_core::store::BeginQueuedRun {
                            session_id: session_id.clone(),
                            identity: Some(surface_drain_scope(&session_id)),
                            request: lash_core::store::QueuedRunRequest::Automatic,
                            configuration: surface_queued_run_configuration(&session_id),
                            expected_head_revision,
                            initial_turn_index: 1,
                            generation: None,
                        },
                    )
                    .await?;
                let summary = queued_run_summary(&admission, &session_id);
                self.surface.queued_run = Some(admission);
                summary
            }
            SurfaceMethod::SelectQueuedRun => {
                let selected = store
                    .select_queued_run(
                        &lease_fence,
                        &surface_drain_scope(&session_id),
                        &lease_owner,
                        1,
                        &surface_queued_run_configuration(&session_id),
                        lash_core::testing::queued_work_claim_policy(1),
                    )
                    .await?;
                let summary = format!(
                    "input_claims={} input_rows={} queued_claims={} queued_batches={} \
                     already_satisfied={} refused={} admission={}",
                    selected.inputs.len(),
                    selected
                        .inputs
                        .iter()
                        .map(|claim| claim.inputs.len())
                        .sum::<usize>(),
                    selected.queued.len(),
                    selected
                        .queued
                        .iter()
                        .map(|claim| claim.batches.len())
                        .sum::<usize>(),
                    selected.already_satisfied.len(),
                    selected.refusal.is_some(),
                    queued_run_summary(&selected.admission, &session_id),
                );
                self.surface.queued_run = Some(selected.admission);
                summary
            }
            SurfaceMethod::SettleQueuedRun => {
                let expected_revision = self
                    .surface
                    .queued_run
                    .as_ref()
                    .map_or(0, |admission| admission.revision);
                let admission = store
                    .settle_queued_run(
                        &lease_fence,
                        lash_core::store::QueuedRunCommit {
                            scope: surface_drain_scope(&session_id),
                            expected_revision,
                            progress: lash_core::store::QueuedRunProgress::Settle {
                                terminal: lash_core::store::QueuedRunTerminal::Failed {
                                    code: lash_core::RuntimeErrorCode::QueuedWork,
                                    message: "fig-2841 surface sweep settles its drain".into(),
                                },
                            },
                        },
                    )
                    .await?;
                let summary = queued_run_summary(&admission, &session_id);
                self.surface.queued_run = Some(admission);
                summary
            }
            SurfaceMethod::CommitDrainEnd => {
                // The drain epilogue's end fact, built the way the runtime
                // builds it: a state-preserving commit receipted under the
                // drain's own scope at the reserved `final` key, borrowing
                // the held lane, claiming the committed head's frame and leaf.
                let head = store.load_session_head_meta().await?;
                let mut commit = runtime_commit(
                    &session_id,
                    head.as_ref().map_or(0, |head| head.head_revision),
                    &append(Vec::new(), None),
                    None,
                    None,
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
                commit.turn_commit = RuntimeTurnCommitStamp::new(
                    lash_core::store::OperationId::new(surface_drain_scope(&session_id), "final"),
                );
                commit.session_execution_lease_fence = Some(lease_fence.clone());
                let result = store.commit_runtime_state(commit).await?;
                self.surface.answer = Some("committed".to_string());
                return Ok(Some(result.into()));
            }
            SurfaceMethod::DrainEndExists => {
                let exists = store.drain_end_exists(SURFACE_DRAIN_ID).await?;
                format!("exists={exists}")
            }
            SurfaceMethod::RaisePendingFollowOnAttempts { owed } => {
                // The raise is leased like every other head write and moves
                // no head revision (ADR 0101 §3): the returned attempts count
                // is the cross-backend-comparable part of the raised fact.
                let turn_id = if owed {
                    SURFACE_FOLLOW_ON_TURN_ID
                } else {
                    SURFACE_UNOWED_FOLLOW_ON_TURN_ID
                };
                let raised = store
                    .raise_pending_follow_on_attempts(
                        &lease_fence,
                        &lash_core::TurnId::from(turn_id),
                    )
                    .await?;
                format!("attempts={}", raised.attempts)
            }
            SurfaceMethod::LoadPendingFollowOn => {
                // Presence, the turn the fact names, and the recovery count
                // are all caller-supplied facts, so they compare across
                // backends where a backend-minted id would not.
                match store.load_pending_follow_on().await? {
                    Some(fact) => format!(
                        "owed={} attempts={}",
                        fact.follow_on_turn_id == SURFACE_FOLLOW_ON_TURN_ID,
                        fact.attempts
                    ),
                    None => "owed=none".to_string(),
                }
            }
            SurfaceMethod::RootTerminal => {
                // The kind and cause are caller-supplied facts; the instant
                // is the backend clock's and is not compared.
                let root = lash_core::TurnId::from(surface_drain_scope(&session_id).id());
                match store.root_terminal(&session_id, &root).await? {
                    Some(terminal) => {
                        let cause = match &terminal.cause {
                            lash_core::store::RootTerminalCause::SessionDeleted { intent } => {
                                format!(
                                    "SessionDeleted(by_own_close={})",
                                    Some(*intent) == self.surface.close_intent
                                )
                            }
                            other => format!("{other:?}"),
                        };
                        format!("kind={:?} cause={cause}", terminal.kind)
                    }
                    None => "terminal=none".to_string(),
                }
            }
            SurfaceMethod::RootBinding => {
                let input = lash_core::InputId::from(format!("{session_id}:input"));
                match store.root_binding(&session_id, &input).await? {
                    Some(root) => {
                        let forked = self.surface.close_intent.is_some_and(|intent| {
                            root == lash_core::store::forked_root(&SURFACE_ROOT_ID.into(), intent)
                        });
                        if forked {
                            "bound=own_fork".to_string()
                        } else {
                            format!("bound={root}")
                        }
                    }
                    None => "bound=none".to_string(),
                }
            }
            SurfaceMethod::RootOfInput => {
                let input = lash_core::InputId::from(format!("{session_id}:input"));
                match store.root_of_input(&session_id, &input).await? {
                    Some(root) => format!("root={root}"),
                    None => "root=none".to_string(),
                }
            }
            SurfaceMethod::BindRootInputs { conflicting } => {
                let root = lash_core::TurnId::from(if conflicting {
                    SURFACE_OTHER_ROOT_ID
                } else {
                    SURFACE_ROOT_ID
                });
                let input = lash_core::InputId::from(format!("{session_id}:input"));
                store.bind_root_inputs(&session_id, &root, &[input]).await?;
                "bound".to_string()
            }
            SurfaceMethod::BeginSessionClose { known_session } => {
                let closing = if known_session {
                    session_id.clone()
                } else {
                    SessionId::from(UNKNOWN_CLOSE_SESSION_ID)
                };
                match self
                    .factory()
                    .begin_session_close(&closing, CLOSE_AT_MS)
                    .await?
                {
                    Some(intent) => {
                        if known_session {
                            self.surface.close_intent.get_or_insert(intent.id);
                        }
                        control_intent_summary(&intent, &closing, self.surface.close_intent)
                    }
                    None => "closed=none".to_string(),
                }
            }
            SurfaceMethod::LoadIntent { known } => {
                match self.factory().load_intent(self.case_intent(known)).await? {
                    Some(intent) => {
                        control_intent_summary(&intent, &session_id, self.surface.close_intent)
                    }
                    None => "intent=none".to_string(),
                }
            }
            SurfaceMethod::ClaimIntentApplication { known } => {
                let application = self
                    .factory()
                    .claim_intent_application(self.case_intent(known))
                    .await?;
                let verdict = match &application {
                    lash_core::store::IntentApplication::Apply(_) => "apply",
                    lash_core::store::IntentApplication::Superseded(_) => "superseded",
                    lash_core::store::IntentApplication::Done(_) => "done",
                };
                format!(
                    "{verdict} {}",
                    control_intent_summary(
                        application.intent(),
                        &session_id,
                        self.surface.close_intent
                    )
                )
            }
            SurfaceMethod::RecordIntentFailure => {
                let intent = self
                    .factory()
                    .record_intent_failure(
                        self.case_intent(true),
                        "fig-3600 engine half refused",
                        true,
                        CLOSE_FAILED_AT_MS,
                    )
                    .await?;
                control_intent_summary(&intent, &session_id, self.surface.close_intent)
            }
            SurfaceMethod::AcknowledgeIntent { known } => {
                self.factory()
                    .acknowledge_intent(self.case_intent(known), CLOSE_ACKNOWLEDGED_AT_MS)
                    .await?;
                "acknowledged".to_string()
            }
            SurfaceMethod::RecordRootPark => {
                let park = store
                    .record_turn_park(&lash_core::store::TurnParkWrite::refusal(
                        session_id.clone(),
                        SURFACE_ROOT_ID.into(),
                        lash_core::store::ParkReason::ReplayDivergence {
                            message: "differential".into(),
                        },
                        CLOSE_AT_MS,
                    ))
                    .await?;
                format!("parked={}", park.attempts)
            }
            SurfaceMethod::OpenRootIntent { fork, stale } => {
                let park = store
                    .load_turn_park(&session_id)
                    .await?
                    .ok_or(StoreError::Contended)?;
                let request = lash_core::store::RootIntentRequest {
                    session_id: session_id.clone(),
                    root: SURFACE_ROOT_ID.into(),
                    park: lash_core::store::ParkId::from_feed_sequence(
                        park.park_id.feed_sequence() + u64::from(stale),
                    ),
                    verb: if fork {
                        lash_core::store::RootVerb::Fork
                    } else {
                        lash_core::store::RootVerb::Cancel
                    },
                };
                let intent = self
                    .factory()
                    .open_root_intent(&request, CLOSE_AT_MS)
                    .await
                    .map_err(|error| match error {
                        lash_core::store::RootIntentRefused::Store(error) => error,
                        error => StoreError::Backend(format!("root intent refused: {error}")),
                    })?;
                let returned_park = match &intent.kind {
                    lash_core::store::ControlIntentKind::Cancel { park, .. }
                    | lash_core::store::ControlIntentKind::Fork { park, .. } => *park,
                    other => panic!("unexpected root intent: {other:?}"),
                };
                assert_eq!(returned_park, request.park);
                self.surface.close_intent = Some(intent.id);
                control_intent_summary(&intent, &session_id, self.surface.close_intent)
            }
            SurfaceMethod::ListControlIntents => format!(
                "intents={}",
                self.factory()
                    .list_control_intents(None, std::num::NonZeroUsize::MIN)
                    .await?
                    .len()
            ),
            SurfaceMethod::ListTerminalRoots => format!(
                "terminals={}",
                self.factory()
                    .list_terminal_roots(None, std::num::NonZeroUsize::MIN)
                    .await?
                    .len()
            ),
            SurfaceMethod::ListReconcilableSessions => format!(
                "sessions={}",
                self.factory()
                    .list_reconcilable_sessions(None, std::num::NonZeroUsize::MIN)
                    .await?
                    .len()
            ),
            SurfaceMethod::AbortUnknownAttachmentWrite => {
                let intent = unknown_attachment_intent(&session_id);
                let outcome = match store.begin_attachment_write(intent.clone()).await? {
                    lash_core::AttachmentWriteFence::Granted(permit) => {
                        store.abort_attachment_write(&intent, permit).await?;
                        "aborted"
                    }
                    _ => "not_granted",
                };
                format!("outcome={outcome}")
            }
            SurfaceMethod::CommitUnknownAttachmentRefs => {
                store
                    .commit_refs(&session_id, &[unknown_attachment_id()])
                    .await?;
                "committed".to_string()
            }
            SurfaceMethod::ForgetUnknownAttachment => {
                store.forget(&session_id, &unknown_attachment_id()).await?;
                "forgotten".to_string()
            }
            SurfaceMethod::Vacuum => {
                let report = store
                    .vacuum()
                    .await
                    .map_err(|_| StoreError::Backend("vacuum_failed".to_string()))?;
                format!(
                    "removed_nodes={} removed_input_tombstones={}",
                    report.removed_node_count, report.removed_pending_turn_input_tombstone_count
                )
            }
        };
        self.surface.answer = Some(answer);
        Ok(None)
    }

    /// The case's close intent, or an id no ledger minted.
    #[expect(
        clippy::expect_used,
        reason = "test support: the case drives its close before any known-intent step; a missing intent panics the harness with its case name by design"
    )]
    fn case_intent(&self, known: bool) -> lash_core::store::ControlIntentId {
        if known {
            self.surface
                .close_intent
                .expect("the case began its session's close before this step")
        } else {
            lash_core::store::ControlIntentId::from_sequence(UNKNOWN_INTENT_SEQUENCE)
        }
    }
}
