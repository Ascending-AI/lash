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

/// A well-formed but never-sealed drive fence, for the inventory steps that
/// take a fence in a case that holds no drive. Presenting it is itself a
/// refusal driver: no backend may act on an unsealed epoch.
fn unheld_drive_fence(session_id: &SessionId) -> lash_core::store::DriveFence {
    lash_core::store_backend_support::sealed_drive_fence(
        session_id.clone(),
        0,
        lash_core::store::AdmissionId::new("fig-2841-no-drive"),
    )
}

/// The root the sweep's checkpoint admission is keyed by. It is never
/// admitted as a root: a checkpoint admission needs no root record.
const SURFACE_CHECKPOINT_ROOT_ID: &str = "fig-2841-surface-checkpoint-root";

/// Scratch state the sweep threads between its own steps.
#[derive(Default)]
pub(super) struct SurfaceScratch {
    pub(super) answer: Option<String>,
    pub(super) corrupt_backup: Option<CorruptBackup>,
    pub(super) corrupt_target: Option<corrupt_input_cases::CorruptTarget>,
    pub(super) batch_id: Option<String>,
    /// The first open turn-work batch the last open-queue read found, the
    /// head [`SurfaceMethod::AdmitListedQueuedHead`] presents.
    pub(super) listed_turn_work_head: Option<lash_core::BatchId>,
    pub(super) root_admission: Option<serde_json::Value>,
    pub(super) queued_root_admission: Option<serde_json::Value>,
    /// The `CloseSession` intent this backend's ledger minted for the case's
    /// session: ids are the backend's own clock, so answers compare it by
    /// identity, never by value.
    pub(super) close_intent: Option<lash_core::store::ControlIntentId>,
    /// The claim this backend's run took on the close intent's obligation:
    /// the engine-half writes compare it (ADR 0109).
    pub(super) intent_claim: Option<lash_core::store::ClaimToken>,
    pub(super) capture_first: Option<lash_core::store::CaptureWriterLease>,
}

/// One fallible store-trait method, driven as a compared differential step.
#[derive(Clone, Copy, Debug)]
pub(super) enum SurfaceMethod {
    CaptureOpen {
        successor: bool,
    },
    CaptureAppend,
    CaptureResetInherited,
    CaptureAdvanceBase,
    CaptureSeal,
    CaptureRead,
    LoadSession,
    ListPendingTurnInputs,
    /// [`IngressStore::pending_turn_input`]: the keyed point read of the
    /// case's next-turn input (`known`), or of an id no case enqueues
    /// (FIG-3976).
    PendingTurnInput {
        known: bool,
    },
    ListTurnInputApplications,
    /// [`RootStore::admit_root`](lash_core::store::RootStore::admit_root) of
    /// the sweep's input-headed root, under the first lease and replayed
    /// under its successor.
    AdmitRoot {
        lease: LeaseSlot,
    },
    AdmitRootAfterHeadSettled,
    /// [`RootStore::admit_root`](lash_core::store::RootStore::admit_root) of
    /// the drain root headed by the case's first pending turn-work batch
    /// (FIG-3927); a second drive replays the recorded admission.
    AdmitQueuedRoot,
    /// [`RootStore::admit_root`](lash_core::store::RootStore::admit_root) of
    /// the drain root headed by the batch the last open-queue read found: the
    /// admission's own read of that head, driven where the list reads refuse.
    AdmitListedQueuedHead,
    /// [`RootStore::unfinished_root`](lash_core::store::RootStore::unfinished_root)
    /// of the case's session.
    UnfinishedRoot,
    EnqueueLateTurnInput,
    ReadSessionStateVersion,
    AdmitSessionState,
    LoadKnownNode,
    LoadUnknownNode,
    ReadDriveEpoch,
    ListQueuedWork,
    ListPendingQueuedWork,
    PendingSessionWorkOrdering,
    EnqueueQueuedWorkWithOutcome,
    /// [`IngressStore::open_session_command_run`](lash_core::IngressStore::open_session_command_run):
    /// the command lane's bindless read of its leading ready run.
    OpenSessionCommandRun,
    /// [`RootStore::admit_at_checkpoint`](lash_core::store::RootStore::admit_at_checkpoint)
    /// of an after-work checkpoint, which the sweep's rows do not reach.
    AdmitAtCheckpoint,
    QueuedWorkBatchCompleted,
    CancelQueuedWorkBatch,
    CancelUnknownPendingTurnInput,
    CancelPendingTurnInputs,
    CancelPendingTurnInputSuffix,
    CommittedTurnExists,
    UncommittedTurnExists,
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
    /// of the sweep's drain root: none while it is unfinished, and its lost
    /// end's evidence after it (FIG-3600 S7).
    RootTerminal,
    NonTerminalRootsPage,
    EndLostRoot,
    /// [`RootStore::end_refused_root`](lash_core::store::RootStore::end_refused_root)
    /// of the sweep's drain root: its refusal's end, then nothing more
    /// (FIG-4018).
    EndRefusedRoot,
    /// [`RootStore::root_binding`](lash_core::store::RootStore::root_binding)
    /// of the sweep's next-turn input.
    RootBinding,
    /// [`RootStore::root_of_input`](lash_core::store::RootStore::root_of_input)
    /// of the sweep's next-turn input.
    RootOfInput,
    /// [`RootStore::bound_turn_scopes`](lash_core::store::RootStore::bound_turn_scopes)
    /// of the sweep's root: the turn scopes its bound inputs name.
    BoundTurnScopes,
    /// [`IngressStore::enqueue_pending_turn_input`] of a keyed next-turn
    /// input under a non-default run spec, which interns the spec (FIG-3838).
    EnqueueRunSpecInput,
    /// [`IngressStore::enqueue_pending_turn_inputs`] of a batch under the
    /// sweep's run spec that resends the spec input and adds a new one, or
    /// (`conflicting`) adds a new one beside the spec input with changed
    /// content, which every backend refuses whole, without residue
    /// (FIG-3842).
    EnqueueTurnInputBatch {
        conflicting: bool,
    },
    /// [`IngressStore::admit_pending_turn_inputs`] of a batch under the
    /// sweep's run spec that resends the batch-added input and adds a new
    /// one (FIG-3975). A backend may fold the follow-ups or not, so only the
    /// admitted rows are compared.
    AdmitTurnInputBatch,
    /// [`IngressStore::load_run_spec`] of the spec the sweep interned
    /// (`known`), or of a hash no input names.
    LoadRunSpec {
        known: bool,
    },
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
    /// of the case's close intent, retryable, under the claim the run took
    /// on its obligation.
    RecordIntentFailure,
    /// [`ControlIntentStore::acknowledge_intent`](lash_core::store::ControlIntentStore::acknowledge_intent)
    /// of the case's close intent under the claim the run took on its
    /// obligation, or of an id no ledger minted.
    AcknowledgeIntent {
        known: bool,
    },
    RecordRootPark,
    OpenRootIntent {
        fork: bool,
        stale: bool,
    },
    ListControlIntents,
    AbortUnknownAttachmentWrite,
    CommitUnknownAttachmentRefs,
    ForgetUnknownAttachment,
    Vacuum,
}

impl SurfaceMethod {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::CaptureOpen { successor: false } => "surface:open_capture_writer",
            Self::CaptureOpen { successor: true } => "surface:open_capture_successor",
            Self::CaptureAppend => "surface:append_capture_batch",
            Self::CaptureResetInherited => "surface:persist_attempt_reset",
            Self::CaptureAdvanceBase => "surface:advance_capture_base",
            Self::CaptureSeal => "surface:seal_turn_capture",
            Self::CaptureRead => "surface:read_stopped_partial",
            Self::LoadSession => "surface:load_session",
            Self::ListPendingTurnInputs => "surface:list_pending_turn_inputs",
            Self::PendingTurnInput { known: true } => "surface:pending_turn_input",
            Self::PendingTurnInput { known: false } => "surface:pending_turn_input_unknown",
            Self::ListTurnInputApplications => "surface:list_turn_input_applications",
            Self::AdmitRoot {
                lease: LeaseSlot::First,
            } => "surface:admit_root",
            Self::AdmitRoot {
                lease: LeaseSlot::Successor,
            } => "surface:replay_admit_root",
            Self::AdmitRootAfterHeadSettled => "surface:admit_root_after_head_settled",
            Self::AdmitQueuedRoot => "surface:admit_queued_root",
            Self::AdmitListedQueuedHead => "surface:admit_listed_queued_head",
            Self::UnfinishedRoot => "surface:unfinished_root",
            Self::EnqueueLateTurnInput => "surface:enqueue_late_turn_input",
            Self::ReadSessionStateVersion => "surface:read_session_state_version",
            Self::AdmitSessionState => "surface:admit_session_state",
            Self::LoadKnownNode => "surface:load_node_known",
            Self::LoadUnknownNode => "surface:load_node_unknown",
            Self::ReadDriveEpoch => "surface:read_drive_epoch",
            Self::ListQueuedWork => "surface:list_queued_work",
            Self::ListPendingQueuedWork => "surface:list_open_queued_work",
            Self::PendingSessionWorkOrdering => "surface:pending_session_work_ordering",
            Self::EnqueueQueuedWorkWithOutcome => "surface:enqueue_queued_work_with_outcome",
            Self::OpenSessionCommandRun => "surface:open_session_command_run",
            Self::AdmitAtCheckpoint => "surface:admit_at_checkpoint",
            Self::QueuedWorkBatchCompleted => "surface:queued_work_batch_completed",
            Self::CancelQueuedWorkBatch => "surface:cancel_queued_work_batch",
            Self::CancelUnknownPendingTurnInput => "surface:cancel_pending_turn_input_unknown",
            Self::CancelPendingTurnInputs => "surface:cancel_pending_turn_inputs",
            Self::CancelPendingTurnInputSuffix => "surface:cancel_pending_turn_input_suffix",
            Self::CommittedTurnExists => "surface:committed_turn_exists_committed",
            Self::UncommittedTurnExists => "surface:committed_turn_exists_uncommitted",
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
            Self::NonTerminalRootsPage => "surface:non_terminal_roots_page",
            Self::EndLostRoot => "surface:end_lost_root",
            Self::EndRefusedRoot => "surface:end_refused_root",
            Self::RootBinding => "surface:root_binding",
            Self::RootOfInput => "surface:root_of_input",
            Self::BoundTurnScopes => "surface:bound_turn_scopes",
            Self::EnqueueRunSpecInput => "surface:enqueue_run_spec_input",
            Self::EnqueueTurnInputBatch { conflicting: false } => {
                "surface:enqueue_turn_input_batch"
            }
            Self::EnqueueTurnInputBatch { conflicting: true } => {
                "surface:enqueue_turn_input_batch_conflicting"
            }
            Self::AdmitTurnInputBatch => "surface:admit_turn_input_batch",
            Self::LoadRunSpec { known: true } => "surface:load_run_spec",
            Self::LoadRunSpec { known: false } => "surface:load_run_spec_unknown",
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

fn surface(method: SurfaceMethod) -> StoreOperation {
    StoreOperation::DriveSurface { method }
}

const UNKNOWN_BATCH_ID: &str = "fig-2841-unknown-batch";
/// A run-spec hash no input names.
const UNKNOWN_RUN_SPEC_HASH: &str = "run-spec:v1:blake3:unknown";
/// The ingress-claim TTL the sweep admits under; no relay runs, so any
/// positive TTL serves.
const SURFACE_INGRESS_CLAIM_TTL_MS: u64 = 60_000;

/// The run spec the sweep's spec input carries (FIG-3838).
fn surface_run_spec() -> lash_core::RunSpec {
    lash_core::RunSpec::overrides(lash_core::RunOverrides {
        provider_id: Some("surface-run-spec-route".to_string()),
        ..lash_core::RunOverrides::default()
    })
}

/// The hash [`surface_run_spec`] is interned under. A spec that cannot be
/// hashed has none, and the reads comparing against it disagree loudly.
fn surface_run_spec_hash() -> Option<lash_core::RunSpecHash> {
    surface_run_spec().hash().ok().flatten()
}
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
const CLOSE_CLAIMED_AT_MS: u64 = 6_500;
/// The queue drain whose root the sweep admits on a queued-work head and
/// ends. Its scope names the root, so every backend admits the same root and
/// the reads before admission ask about a root that genuinely does not exist
/// yet.
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

/// The drain root the sweep admits on a queued-work head.
fn surface_queued_root(session_id: &SessionId) -> lash_core::TurnId {
    lash_core::TurnId::from(surface_drain_scope(session_id).id())
}

/// An admission request for `root` headed by `head` under `fence`.
fn surface_admit_request(
    fence: &lash_core::store::DriveFence,
    root: lash_core::TurnId,
    head: lash_core::store::AdmittedHead,
) -> lash_core::store::AdmitRootRequest {
    let mut request =
        lash_core::testing::store_fixtures::admit_root_request_for_test(fence, &root, head);
    request.max_inputs = 8;
    request.policy = lash_core::testing::queued_work_admission_policy(1);
    request.admitted_generation = lash_core::engine::BuildGeneration::for_test("surface-root");
    request
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
        "own={} own_session={} format={} kind={kind} state={state} created_at_ms={} \
         engine={:?} obligation_armed={}",
        Some(intent.id) == own,
        intent.session_id == *session_id,
        intent.format,
        intent.created_at_ms,
        intent.engine,
        intent.obligation.is_some(),
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
            surface(SurfaceMethod::CaptureOpen { successor: false }),
            surface(SurfaceMethod::CaptureAppend),
            surface(SurfaceMethod::CaptureOpen { successor: true }),
            surface(SurfaceMethod::CaptureResetInherited),
            surface(SurfaceMethod::CaptureAdvanceBase),
            surface(SurfaceMethod::CaptureSeal),
            surface(SurfaceMethod::CaptureRead),
            StoreOperation::EnqueueNextTurnInput,
            StoreOperation::EnqueueAdmittableQueuedWork,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "surface-sweep-owner",
            },
            surface(SurfaceMethod::ReadSessionStateVersion),
            surface(SurfaceMethod::AdmitSessionState),
            surface(SurfaceMethod::LoadKnownNode),
            surface(SurfaceMethod::LoadUnknownNode),
            surface(SurfaceMethod::ReadDriveEpoch),
            surface(SurfaceMethod::ListQueuedWork),
            surface(SurfaceMethod::ListPendingQueuedWork),
            surface(SurfaceMethod::PendingSessionWorkOrdering),
            surface(SurfaceMethod::EnqueueQueuedWorkWithOutcome),
            surface(SurfaceMethod::OpenSessionCommandRun),
            surface(SurfaceMethod::AdmitAtCheckpoint),
            surface(SurfaceMethod::QueuedWorkBatchCompleted),
            surface(SurfaceMethod::CancelQueuedWorkBatch),
            surface(SurfaceMethod::CommittedTurnExists),
            surface(SurfaceMethod::UncommittedTurnExists),
            // One queued-headed root end to end (FIG-3927): no unfinished
            // root, then its admission and a replay of it, its lost end, and
            // the drain's end receipt. `drain_end_exists` is driven on both
            // sides of that receipt, so a backend that answers `false`
            // without looking cannot agree.
            surface(SurfaceMethod::UnfinishedRoot),
            surface(SurfaceMethod::AdmitQueuedRoot),
            surface(SurfaceMethod::UnfinishedRoot),
            surface(SurfaceMethod::AdmitQueuedRoot),
            surface(SurfaceMethod::RootTerminal),
            surface(SurfaceMethod::EndLostRoot),
            // The lost end wrote its root's evidence (FIG-3600 S7).
            surface(SurfaceMethod::RootTerminal),
            surface(SurfaceMethod::UnfinishedRoot),
            surface(SurfaceMethod::DrainEndExists),
            surface(SurfaceMethod::CommitDrainEnd),
            surface(SurfaceMethod::DrainEndExists),
            // An input's root binding: unbound, bound once, read back, and a
            // second binding to another root refused.
            surface(SurfaceMethod::RootBinding),
            surface(SurfaceMethod::BoundTurnScopes),
            surface(SurfaceMethod::BindRootInputs { conflicting: false }),
            surface(SurfaceMethod::RootBinding),
            surface(SurfaceMethod::RootOfInput),
            surface(SurfaceMethod::BoundTurnScopes),
            surface(SurfaceMethod::BindRootInputs { conflicting: false }),
            surface(SurfaceMethod::BindRootInputs { conflicting: true }),
            surface(SurfaceMethod::RootBinding),
            // A spec is unknown until an input naming it is admitted, then
            // reads back exactly; a retry interns nothing new.
            surface(SurfaceMethod::LoadRunSpec { known: true }),
            surface(SurfaceMethod::EnqueueRunSpecInput),
            surface(SurfaceMethod::EnqueueRunSpecInput),
            // A batch resends the spec input and adds one; resending the
            // batch adds nothing; a conflicting batch is refused whole.
            surface(SurfaceMethod::EnqueueTurnInputBatch { conflicting: false }),
            surface(SurfaceMethod::EnqueueTurnInputBatch { conflicting: false }),
            surface(SurfaceMethod::EnqueueTurnInputBatch { conflicting: true }),
            // Admission of a batch resending the added input and adding one;
            // resending it admits nothing new.
            surface(SurfaceMethod::AdmitTurnInputBatch),
            surface(SurfaceMethod::AdmitTurnInputBatch),
            surface(SurfaceMethod::LoadRunSpec { known: true }),
            surface(SurfaceMethod::LoadRunSpec { known: false }),
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

/// A failed engine run settles the same open root on every SQL backend.
pub(super) fn lost_root_recovery_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::LostRootRecovery,
        operations: vec![
            StoreOperation::Commit {
                label: "seed_lost_root_graph",
                expected_head_revision: 0,
                graph: append(
                    vec![
                        NodeSpec::new("root", None, "root"),
                        NodeSpec::new("active-frame", Some("root"), "active"),
                    ],
                    Some("active-frame"),
                ),
                turn_commit: None,
                checkpoint: CheckpointSpec::Empty,
                usage: false,
                adopt_attachment: false,
            },
            StoreOperation::EnqueueAdmittableQueuedWork,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "lost-root-owner",
            },
            surface(SurfaceMethod::AdmitQueuedRoot),
            surface(SurfaceMethod::NonTerminalRootsPage),
            surface(SurfaceMethod::EndLostRoot),
            surface(SurfaceMethod::RootTerminal),
            surface(SurfaceMethod::EndLostRoot),
            surface(SurfaceMethod::NonTerminalRootsPage),
        ],
    }
}

/// A root whose run met a typed refusal ends the same way on every SQL
/// backend, once: a second end, and the lost-root end after it, write
/// nothing (FIG-4018).
pub(super) fn refused_root_end_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::RefusedRootEnd,
        operations: vec![
            StoreOperation::Commit {
                label: "seed_refused_root_graph",
                expected_head_revision: 0,
                graph: append(
                    vec![
                        NodeSpec::new("root", None, "root"),
                        NodeSpec::new("active-frame", Some("root"), "active"),
                    ],
                    Some("active-frame"),
                ),
                turn_commit: None,
                checkpoint: CheckpointSpec::Empty,
                usage: false,
                adopt_attachment: false,
            },
            StoreOperation::EnqueueAdmittableQueuedWork,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "refused-root-owner",
            },
            surface(SurfaceMethod::AdmitQueuedRoot),
            surface(SurfaceMethod::UnfinishedRoot),
            surface(SurfaceMethod::EndRefusedRoot),
            surface(SurfaceMethod::RootTerminal),
            surface(SurfaceMethod::UnfinishedRoot),
            surface(SurfaceMethod::EndRefusedRoot),
            surface(SurfaceMethod::EndLostRoot),
            surface(SurfaceMethod::RootTerminal),
            surface(SurfaceMethod::NonTerminalRootsPage),
        ],
    }
}

/// The root's admission is replayed after a lease handoff, even when another
/// input becomes eligible between the admission commit and the journal write.
pub(super) fn root_admission_replay_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::RootAdmissionReplay,
        operations: vec![
            StoreOperation::EnqueueNextTurnInput,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "root-admission-first",
            },
            surface(SurfaceMethod::AdmitRoot {
                lease: LeaseSlot::First,
            }),
            StoreOperation::ReleaseSessionLease {
                lease: LeaseSlot::First,
            },
            surface(SurfaceMethod::EnqueueLateTurnInput),
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::Successor,
                owner: "root-admission-successor",
            },
            surface(SurfaceMethod::AdmitRoot {
                lease: LeaseSlot::Successor,
            }),
            surface(SurfaceMethod::UnfinishedRoot),
            surface(SurfaceMethod::CancelPendingTurnInputs),
            surface(SurfaceMethod::AdmitRootAfterHeadSettled),
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
            StoreOperation::EnqueueAdmittableQueuedWork,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "deleted-surface-owner",
            },
            StoreOperation::DeleteSession,
            surface(SurfaceMethod::ReadSessionStateVersion),
            surface(SurfaceMethod::LoadUnknownNode),
            surface(SurfaceMethod::ReadDriveEpoch),
            surface(SurfaceMethod::ListQueuedWork),
            surface(SurfaceMethod::ListPendingQueuedWork),
            surface(SurfaceMethod::PendingSessionWorkOrdering),
            surface(SurfaceMethod::EnqueueQueuedWorkWithOutcome),
            surface(SurfaceMethod::OpenSessionCommandRun),
            surface(SurfaceMethod::AdmitAtCheckpoint),
            surface(SurfaceMethod::CancelUnknownPendingTurnInput),
            surface(SurfaceMethod::UnfinishedRoot),
            surface(SurfaceMethod::AdmitQueuedRoot),
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
/// root and an admitted queued-headed root — `Cancelled` by the close, a retry
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
            StoreOperation::EnqueueAdmittableQueuedWork,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "session-close-owner",
            },
            surface(SurfaceMethod::BindRootInputs { conflicting: false }),
            surface(SurfaceMethod::AdmitQueuedRoot),
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
            surface(SurfaceMethod::UnfinishedRoot),
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
        // The fence is copied out up front: the sweep mutates its own scratch
        // state inside these arms, which cannot hold a borrow of self.
        let lease_fence = self
            .first_lease
            .clone()
            .unwrap_or_else(|| unheld_drive_fence(&session_id));
        let answer = match method {
            SurfaceMethod::CaptureOpen { successor } => {
                let lease = store
                    .open_capture_writer(&lash_core::store::OpenCaptureWriter {
                        turn: lash_core::facade_support::TurnAddress::new(
                            session_id.clone(),
                            "surface-capture-turn",
                        ),
                        root: "surface-capture-turn".into(),
                        invocation: lash_core::store::CaptureInvocationKey("surface-llm".into()),
                    })
                    .await?;
                if !successor {
                    self.surface.capture_first = Some(lease.clone());
                }
                format!(
                    "epoch={} inherited={}",
                    lease.attempt_epoch,
                    lease.inherited.len()
                )
            }
            SurfaceMethod::CaptureAppend => {
                let lease = self.surface.capture_first.as_ref().ok_or_else(|| {
                    StoreError::Backend("the case opens its capture writer first".into())
                })?;
                let block = lash_sansio::llm::types::StreamBlockIdentity::new("message", 0);
                let ack = store
                    .append_capture_batch(&lash_core::store::CaptureBatch {
                        lease: lease.lease_ref(),
                        batch_ordinal: 0,
                        frames: vec![
                            lash_core::store::CaptureFrame::TextStart {
                                block: block.clone(),
                            },
                            lash_core::store::CaptureFrame::TextDelta {
                                block: block.clone(),
                                text: "surface".into(),
                            },
                            lash_core::store::CaptureFrame::TextEnd {
                                block,
                                text: "surface".into(),
                            },
                        ],
                    })
                    .await?;
                format!("first={} last={}", ack.first_sequence, ack.last_sequence)
            }
            SurfaceMethod::CaptureResetInherited => {
                let lease = self.surface.capture_first.as_ref().ok_or_else(|| {
                    StoreError::Backend("the case opens its capture writer first".into())
                })?;
                let resumed = store
                    .persist_attempt_reset(&lash_core::store::CaptureAttemptReset {
                        lease: lease.lease_ref(),
                    })
                    .await?;
                format!(
                    "epoch={} inherited={}",
                    resumed.attempt_epoch,
                    resumed.inherited.len()
                )
            }
            SurfaceMethod::CaptureAdvanceBase => {
                store
                    .advance_capture_base(&lash_core::store::CaptureBaseAdvance {
                        turn: lash_core::facade_support::TurnAddress::new(
                            session_id.clone(),
                            "surface-capture-turn",
                        ),
                        to: lash_sansio::CaptureBase(1),
                    })
                    .await?;
                "advanced".to_string()
            }
            SurfaceMethod::CaptureSeal => {
                let partial = store
                    .seal_turn_capture(&lash_core::store::SealTurnCapture {
                        turn: lash_core::facade_support::TurnAddress::new(
                            session_id.clone(),
                            "surface-capture-turn",
                        ),
                        root: "surface-capture-turn".into(),
                        reason: lash_sansio::StopReason::UserCancel,
                        recorded_watermark: None,
                        drive_fence: None,
                    })
                    .await?
                    .into_partial();
                format!(
                    "through={} items={} recovered={}",
                    partial.id.sealed_through,
                    partial.items.len(),
                    partial.recovered_after_process_loss
                )
            }
            SurfaceMethod::CaptureRead => {
                let read = store
                    .read_stopped_partial(&lash_core::store::StoppedPartialReadRequest {
                        session_id: session_id.clone(),
                        turn: "surface-capture-turn".into(),
                    })
                    .await?;
                format!("{read:?}")
            }
            SurfaceMethod::LoadSession => {
                format!(
                    "present={}",
                    store
                        .load_session_window(
                            &session_id,
                            lash_core::store::WindowSelector::Current,
                        )
                        .await?
                        .is_some()
                )
            }
            SurfaceMethod::ListPendingTurnInputs => {
                format!(
                    "rows={}",
                    store.list_pending_turn_inputs(&session_id).await?.len()
                )
            }
            SurfaceMethod::PendingTurnInput { known } => {
                // The keyed point read answers what the list answers for the
                // row: `Open`, `Admitted` naming its root, or nothing once
                // the row is terminal or unknown (FIG-3976).
                let input = lash_core::InputId::from(if known {
                    format!("{session_id}:input")
                } else {
                    UNKNOWN_INPUT_ID.to_string()
                });
                match store.pending_turn_input(&session_id, &input).await? {
                    Some(read) => match &read.status {
                        lash_core::PendingTurnInputReadStatus::Open => "status=open".to_string(),
                        lash_core::PendingTurnInputReadStatus::Admitted { root } => {
                            format!("status=admitted:{root}")
                        }
                        other => format!("status={other:?}"),
                    },
                    None => "status=none".to_string(),
                }
            }
            SurfaceMethod::ListTurnInputApplications => {
                format!(
                    "rows={}",
                    store.list_turn_input_applications(&session_id).await?.len()
                )
            }
            SurfaceMethod::AdmitRoot { lease } => {
                let lease_slot = lease;
                let lease = self.lease(lease_slot).clone();
                let head = lash_core::InputId::from(format!("{session_id}:input"));
                let request = surface_admit_request(
                    &lease,
                    lash_core::TurnId::from(SURFACE_ROOT_ID),
                    lash_core::store::AdmittedHead::Input(head.clone()),
                );
                // A replay that differs, a refusal or a widened prefix is a
                // law violation on this backend, never an answer to compare.
                let Some(admission) = store.admit_root(&request).await? else {
                    panic!("{}: root admission did not reach its head", self.name);
                };
                let encoded = serde_json::to_value(&admission)
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                match &self.surface.root_admission {
                    Some(recorded) => assert_eq!(
                        *recorded, encoded,
                        "{}: a successor's root admission must return the recorded result",
                        self.name
                    ),
                    None => self.surface.root_admission = Some(encoded),
                }
                assert_eq!(
                    admission.input_ids(),
                    vec![head],
                    "{}: the root admission must not widen past its recorded prefix",
                    self.name
                );
                format!(
                    "inputs={} base_generation={} base_revision={} turn_index={} replay={}",
                    admission.input_ids().len(),
                    admission.base.generation,
                    admission.base.revision,
                    admission.turn_index,
                    matches!(lease_slot, LeaseSlot::Successor)
                )
            }
            SurfaceMethod::EnqueueLateTurnInput => {
                // Queued between a root's claim commit and its replay: it must
                // never widen the recorded prefix.
                store
                    .enqueue_pending_turn_input(
                        PendingTurnInputDraft::new(
                            &session_id,
                            TurnInputIngress::NextTurn,
                            TurnInput::text("late input after root claim"),
                        )
                        .with_input_id(format!("{session_id}:late-input")),
                    )
                    .await?;
                "enqueued".to_string()
            }
            SurfaceMethod::AdmitRootAfterHeadSettled => {
                // The row may have settled, but the root's admission remains
                // the answer of record and cannot widen to the later row.
                let lease = self.lease(LeaseSlot::Successor).clone();
                let request = surface_admit_request(
                    &lease,
                    lash_core::TurnId::from(SURFACE_ROOT_ID),
                    lash_core::store::AdmittedHead::Input(lash_core::InputId::from(format!(
                        "{session_id}:input"
                    ))),
                );
                let Some(admission) = store.admit_root(&request).await? else {
                    panic!("{}: recorded root admission disappeared", self.name);
                };
                let encoded = serde_json::to_value(admission)
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                assert_eq!(self.surface.root_admission.as_ref(), Some(&encoded));
                "recorded".to_string()
            }
            SurfaceMethod::AdmitQueuedRoot => {
                // The head is the case's first pending turn-work batch, read
                // rather than assumed: backends mint their own batch ids.
                let head = store
                    .list_open_queued_work(&session_id)
                    .await?
                    .into_iter()
                    .filter(|batch| {
                        batch.work_class() == lash_core::store::QueuedWorkClass::TurnWork
                    })
                    .min_by_key(|batch| batch.enqueue_seq)
                    .map(|batch| batch.batch_id);
                let head = match (&self.surface.queued_root_admission, head) {
                    (Some(recorded), _) => {
                        serde_json::from_value::<lash_core::store::RootAdmission>(recorded.clone())
                            .map_err(|error| StoreError::Backend(error.to_string()))?
                            .head
                    }
                    (None, Some(batch)) => lash_core::store::AdmittedHead::Batch(batch),
                    (None, None) => lash_core::store::AdmittedHead::Batch(
                        lash_core::BatchId::from(UNKNOWN_BATCH_ID),
                    ),
                };
                let request =
                    surface_admit_request(&lease_fence, surface_queued_root(&session_id), head);
                match store.admit_root(&request).await? {
                    None => "admitted=false".to_string(),
                    Some(admission) => {
                        let encoded = serde_json::to_value(&admission)
                            .map_err(|error| StoreError::Backend(error.to_string()))?;
                        let replay = match &self.surface.queued_root_admission {
                            Some(recorded) => {
                                assert_eq!(
                                    *recorded, encoded,
                                    "{}: a replayed admission must return the recorded result",
                                    self.name
                                );
                                true
                            }
                            None => {
                                self.surface.queued_root_admission = Some(encoded);
                                false
                            }
                        };
                        format!(
                            "admitted=true inputs={} batches={} base_revision={} turn_index={} \
                             replay={replay}",
                            admission.input_ids().len(),
                            admission
                                .queued
                                .as_ref()
                                .map_or(0, |queued| queued.batches.len()),
                            admission.base.revision,
                            admission.turn_index,
                        )
                    }
                }
            }
            SurfaceMethod::UnfinishedRoot => match store.unfinished_root(&session_id).await? {
                None => "unfinished=none".to_string(),
                Some(unfinished) => format!(
                    "unfinished_root={} head={}",
                    if unfinished.root == surface_queued_root(&session_id) {
                        "drain".to_string()
                    } else {
                        unfinished.root.to_string()
                    },
                    match unfinished.head {
                        lash_core::store::AdmittedHead::Input(_) => "input",
                        lash_core::store::AdmittedHead::Batch(_) => "batch",
                    }
                ),
            },
            SurfaceMethod::AdmitListedQueuedHead => {
                let head = lash_core::store::AdmittedHead::Batch(
                    self.surface
                        .listed_turn_work_head
                        .clone()
                        .unwrap_or_else(|| lash_core::BatchId::from(UNKNOWN_BATCH_ID)),
                );
                let request =
                    surface_admit_request(&lease_fence, surface_queued_root(&session_id), head);
                let admission = store.admit_root(&request).await?;
                format!("admitted={}", admission.is_some())
            }
            SurfaceMethod::ReadSessionStateVersion => {
                format!(
                    "version={}",
                    store.read_session_state_version(&session_id).await?
                )
            }
            SurfaceMethod::AdmitSessionState => {
                let admission = store.admit_session_state(&lease_fence).await?;
                format!("version={}", admission.version)
            }
            SurfaceMethod::LoadKnownNode => {
                let node_id = scoped_node_id(&session_id, "root");
                let page = store
                    .load_ancestors(
                        &session_id,
                        lash_core::store::HistoryAnchor::Node(node_id.into()),
                        lash_core::store::HistoryBudget {
                            max_nodes: std::num::NonZeroU32::MIN,
                            max_bytes: std::num::NonZeroU64::MIN.saturating_add(1024 * 1024 - 1),
                        },
                    )
                    .await?;
                format!("present={}", !page.nodes.is_empty())
            }
            SurfaceMethod::LoadUnknownNode => {
                let page = store
                    .load_ancestors(
                        &session_id,
                        lash_core::store::HistoryAnchor::Node(lash_core::NodeId::from(
                            "fig-2841-unknown-node",
                        )),
                        lash_core::store::HistoryBudget {
                            max_nodes: std::num::NonZeroU32::MIN,
                            max_bytes: std::num::NonZeroU64::MIN.saturating_add(1024 * 1024 - 1),
                        },
                    )
                    .await?;
                format!("present={}", !page.nodes.is_empty())
            }
            SurfaceMethod::ReadDriveEpoch => {
                let observed = store.drive_epoch(&session_id).await?;
                format!(
                    "epoch={} admission_present={}",
                    observed.epoch,
                    observed.admission.is_some()
                )
            }
            SurfaceMethod::ListQueuedWork => {
                format!("rows={}", store.list_queued_work(&session_id).await?.len())
            }
            SurfaceMethod::ListPendingQueuedWork => {
                let open = store.list_open_queued_work(&session_id).await?;
                self.surface.listed_turn_work_head = open
                    .iter()
                    .filter(|batch| {
                        batch.work_class() == lash_core::store::QueuedWorkClass::TurnWork
                    })
                    .min_by_key(|batch| batch.enqueue_seq)
                    .map(|batch| batch.batch_id.clone());
                format!("rows={}", open.len())
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
            SurfaceMethod::OpenSessionCommandRun => {
                let run = store.open_session_command_run(&lease_fence).await?;
                format!("commands={}", run.len())
            }
            SurfaceMethod::AdmitAtCheckpoint => {
                let admission = store
                    .admit_at_checkpoint(&lash_core::store::CheckpointAdmissionRequest {
                        fence: lease_fence.clone(),
                        root: lash_core::TurnId::from(SURFACE_CHECKPOINT_ROOT_ID),
                        turn_id: lash_core::TurnId::from("fig-2841-surface-turn"),
                        checkpoint: lash_core::CheckpointKind::AfterWork,
                        step: "fig-2841-surface-checkpoint".to_string(),
                        max_inputs: 1,
                        policy: lash_core::testing::queued_work_admission_policy(1),
                    })
                    .await?;
                format!(
                    "inputs={} queued={}",
                    admission.inputs.is_some(),
                    admission.queued.is_some()
                )
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
            SurfaceMethod::CommittedTurnExists => {
                let exists = store
                    .committed_turn_exists(
                        &session_id,
                        &lash_core::TurnId::from(SURFACE_COMMITTED_TURN_ID),
                    )
                    .await?;
                format!("exists={exists}")
            }
            SurfaceMethod::UncommittedTurnExists => {
                let exists = store
                    .committed_turn_exists(
                        &session_id,
                        &lash_core::TurnId::from(UNCOMMITTED_TURN_ID),
                    )
                    .await?;
                format!("exists={exists}")
            }
            SurfaceMethod::CommitDrainEnd => {
                // The drain epilogue's end fact, built the way the runtime
                // builds it: a state-preserving commit receipted under the
                // drain's own scope at the reserved `final` key, borrowing
                // the held lane, claiming the committed head's frame and leaf.
                let head = store.load_session_head_meta(&session_id).await?;
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
                commit.drive_fence = Some(Box::new(lease_fence.clone()));
                let result = store.commit_runtime_state(commit).await?;
                self.surface.answer = Some("committed".to_string());
                return Ok(Some(result.into()));
            }
            SurfaceMethod::DrainEndExists => {
                let exists = store
                    .drain_end_exists(&session_id, SURFACE_DRAIN_ID)
                    .await?;
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
                match store.load_pending_follow_on(&session_id).await? {
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
            SurfaceMethod::NonTerminalRootsPage => {
                let factory = self.factory();
                let mut after = None;
                let mut own_roots = 0;
                loop {
                    let page = factory
                        .non_terminal_roots_page(
                            after.as_ref(),
                            std::num::NonZeroUsize::MIN.saturating_add(127),
                        )
                        .await?;
                    own_roots += page
                        .iter()
                        .filter(|root| root.session == session_id)
                        .count();
                    if page.len() < 128 {
                        break;
                    }
                    after = page.last().cloned();
                }
                format!("own_open_roots={own_roots}")
            }
            SurfaceMethod::EndLostRoot => {
                let root = lash_core::engine::RootRef {
                    session: session_id.clone(),
                    root: lash_core::TurnId::from(surface_drain_scope(&session_id).id()),
                };
                match self.factory().end_lost_root(&root, 1).await? {
                    Some(terminal) => format!("ended={:?}", terminal.kind),
                    None => "ended=none".to_string(),
                }
            }
            SurfaceMethod::EndRefusedRoot => {
                let root = lash_core::TurnId::from(surface_drain_scope(&session_id).id());
                let refusal = lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::StoreCommitSuperseded,
                    "the head moved under the root's commit",
                );
                match store
                    .end_refused_root(&session_id, &root, &refusal, 1)
                    .await?
                {
                    Some(terminal) => format!("ended={:?}", terminal.kind),
                    None => "ended=none".to_string(),
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
            SurfaceMethod::BoundTurnScopes => {
                let root = lash_core::TurnId::from(SURFACE_ROOT_ID);
                let scopes = store
                    .bound_turn_scopes(&session_id, &root)
                    .await?
                    .iter()
                    .map(|scope| scope.as_str().replace(session_id.as_str(), "<session>"))
                    .collect::<Vec<_>>();
                format!("scopes={scopes:?}")
            }
            SurfaceMethod::EnqueueRunSpecInput => {
                let input = store
                    .enqueue_pending_turn_input(
                        PendingTurnInputDraft::new(
                            &session_id,
                            TurnInputIngress::NextTurn,
                            TurnInput::text("input under a run spec"),
                        )
                        .with_source_key("surface:run-spec-input")
                        .with_input_id(format!("{session_id}:run-spec-input"))
                        .with_run_spec(surface_run_spec()),
                    )
                    .await?;
                format!(
                    "run_spec_is_interned_hash={}",
                    input.run_spec == surface_run_spec_hash()
                )
            }
            SurfaceMethod::EnqueueTurnInputBatch { conflicting } => {
                let draft = |key: &str, text: &str| {
                    PendingTurnInputDraft::new(
                        &session_id,
                        TurnInputIngress::NextTurn,
                        TurnInput::text(text),
                    )
                    .with_source_key(key)
                    .with_run_spec(surface_run_spec())
                };
                let (added, resent_text) = if conflicting {
                    (
                        "surface:batch-refused-input",
                        "changed input under a run spec",
                    )
                } else {
                    ("surface:batch-input", "input under a run spec")
                };
                let rows = store
                    .enqueue_pending_turn_inputs(lash_core::PendingTurnInputBatch::new(
                        session_id.clone(),
                        vec![
                            // Every backend mints unkeyed ids its own way, so
                            // the sweep names each row it adds.
                            draft(added, "input added by a batch")
                                .with_input_id(format!("{session_id}:{added}")),
                            draft("surface:run-spec-input", resent_text)
                                .with_input_id(format!("{session_id}:run-spec-input")),
                        ],
                    )?)
                    .await?;
                format!(
                    "keys={:?} run_specs_interned={}",
                    rows.iter()
                        .map(|row| row.source_key.as_deref())
                        .collect::<Vec<_>>(),
                    rows.iter()
                        .all(|row| row.run_spec == surface_run_spec_hash())
                )
            }
            SurfaceMethod::AdmitTurnInputBatch => {
                let draft = |key: &str, text: &str| {
                    PendingTurnInputDraft::new(
                        &session_id,
                        TurnInputIngress::NextTurn,
                        TurnInput::text(text),
                    )
                    .with_source_key(key)
                    .with_input_id(format!("{session_id}:{key}"))
                    .with_run_spec(surface_run_spec())
                };
                let admission = store
                    .admit_pending_turn_inputs(
                        lash_core::PendingTurnInputBatch::new(
                            session_id.clone(),
                            vec![
                                draft("surface:batch-input", "input added by a batch"),
                                draft("surface:admitted-input", "input added by an admission"),
                            ],
                        )?,
                        SURFACE_INGRESS_CLAIM_TTL_MS,
                    )
                    .await?;
                let rows = match admission {
                    lash_core::TurnInputAdmission::Fused { rows, .. }
                    | lash_core::TurnInputAdmission::Enqueued(rows) => rows,
                };
                format!(
                    "keys={:?} run_specs_interned={}",
                    rows.iter()
                        .map(|row| row.source_key.as_deref())
                        .collect::<Vec<_>>(),
                    rows.iter()
                        .all(|row| row.run_spec == surface_run_spec_hash())
                )
            }
            SurfaceMethod::LoadRunSpec { known } => {
                let hash = if known {
                    surface_run_spec_hash().unwrap_or_else(|| {
                        lash_core::RunSpecHash::from_stored(UNKNOWN_RUN_SPEC_HASH)
                    })
                } else {
                    lash_core::RunSpecHash::from_stored(UNKNOWN_RUN_SPEC_HASH)
                };
                match store.load_run_spec(&session_id, &hash).await? {
                    Some(spec) => format!("spec_matches={}", spec == surface_run_spec()),
                    None => "spec=none".to_string(),
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
                    .claim_intent_application(self.case_intent(known), CLOSE_CLAIMED_AT_MS)
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
                let claim = self.intent_claim(true).await?;
                match self
                    .factory()
                    .record_intent_failure(
                        self.case_intent(true),
                        &claim,
                        "fig-3600 engine half refused",
                        true,
                        CLOSE_FAILED_AT_MS,
                    )
                    .await?
                {
                    lash_core::store::IntentSettle::Held(intent) => {
                        control_intent_summary(&intent, &session_id, self.surface.close_intent)
                    }
                    lash_core::store::IntentSettle::ClaimLost => "claim_lost".to_string(),
                }
            }
            SurfaceMethod::AcknowledgeIntent { known } => {
                let claim = self.intent_claim(known).await?;
                match self
                    .factory()
                    .acknowledge_intent(self.case_intent(known), &claim, CLOSE_ACKNOWLEDGED_AT_MS)
                    .await?
                {
                    lash_core::store::IntentSettle::Held(_) => "acknowledged".to_string(),
                    lash_core::store::IntentSettle::ClaimLost => "claim_lost".to_string(),
                }
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
                    .vacuum(&session_id)
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

    /// The claim this run holds on the case's close-intent obligation,
    /// taken on first use; a token no ledger minted for an unknown intent.
    async fn intent_claim(
        &mut self,
        known: bool,
    ) -> Result<lash_core::store::ClaimToken, lash_core::StoreError> {
        if !known {
            return Ok(lash_core::store::ClaimToken::new(
                "differential-unknown-claim",
            ));
        }
        if let Some(claim) = &self.surface.intent_claim {
            return Ok(claim.clone());
        }
        let obligation = self
            .factory()
            .load_intent(self.case_intent(true))
            .await?
            .and_then(|intent| intent.obligation)
            .ok_or_else(|| StoreError::Backend("the close armed no obligation".into()))?;
        let claimed = self
            .lifecycle_backend
            .obligation_ledger(lash_core::store::ObligationKind::ControlIntent)
            .claim(
                &obligation,
                &lash_core::store::ClaimToken::mint(),
                CLOSE_CLAIMED_AT_MS,
                3_600_000,
            )
            .await?
            .ok_or_else(|| StoreError::Backend("the close's obligation is not due".into()))?;
        self.surface.intent_claim = Some(claimed.token.clone());
        Ok(claimed.token)
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
