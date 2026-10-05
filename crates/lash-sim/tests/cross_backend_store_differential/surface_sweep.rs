//! The operation inventory over the fallible store-trait surface (FIG-2841).
//!
//! `trait_surface_gate.rs` refuses any fallible method of the gated store
//! traits that this binary never calls. The drivers live here: one
//! [`SurfaceMethod`] per trait method, each executed as its own compared step so
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
use lash_core::store::SessionCommitStore as _;
use lash_core::store::WaitReceiptStore;

/// A well-formed but never-sealed shift fence, for the inventory steps that
/// take a fence in a case that holds no shift. Presenting it is itself a
/// refusal driver: no backend may act on an unsealed epoch.
fn unheld_shift_fence(session_id: &SessionId) -> lash_core::store::ShiftFence {
    lash_core::store_backend_support::sealed_shift_fence(
        session_id.clone(),
        0,
        lash_core::store::AdmissionId::new("fig-2841-no-shift"),
    )
}

/// The run the sweep's checkpoint admission is keyed by. It is never
/// admitted as a run: a checkpoint admission needs no run record.
const SURFACE_CHECKPOINT_RUN_ID: &str = "fig-2841-surface-checkpoint-run";

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
    pub(super) run_admission: Option<serde_json::Value>,
    pub(super) queued_run_admission: Option<serde_json::Value>,
    /// The `CloseSession` intent this backend's ledger minted for the case's
    /// session: ids are the backend's own clock, so answers compare it by
    /// identity, never by value.
    pub(super) close_intent: Option<lash_core::store::ControlIntentId>,
    /// The claim this backend's run took on the close intent's obligation:
    /// the engine-half writes compare it (ADR 0109).
    pub(super) intent_claim: Option<lash_core::store::ClaimToken>,
}

/// One fallible store-trait method, executed as a compared differential step.
#[derive(Clone, Copy, Debug)]
pub(super) enum SurfaceMethod {
    RefusedRootAdmission,
    ToolReceipts,
    WaitReceipts,
    LoadSession,
    ListPendingTurnInputs,
    /// [`IngressStore::pending_turn_input`]: the keyed point read of the
    /// case's next-turn input (`known`), or of an id no case enqueues
    /// (FIG-3976).
    PendingTurnInput {
        known: bool,
    },
    ListTurnInputApplications,
    /// [`RunStore::admit_run`](lash_core::store::RunStore::admit_run) of
    /// the sweep's input-headed run, under the first lease and replayed
    /// under its successor.
    AdmitRun {
        lease: LeaseSlot,
    },
    AdmitRunAfterHeadSettled,
    /// [`RunStore::admit_run`](lash_core::store::RunStore::admit_run) of
    /// the drain run headed by the case's first pending turn-work batch
    /// (FIG-3927); a second shift replays the recorded admission.
    AdmitQueuedRun,
    /// [`RunStore::admit_run`](lash_core::store::RunStore::admit_run) of
    /// the drain run headed by the batch the last open-queue read found: the
    /// admission's own read of that head, executed where the list reads refuse.
    AdmitListedQueuedHead,
    /// [`RunStore::unfinished_run`](lash_core::store::RunStore::unfinished_run)
    /// of the case's session.
    UnfinishedRun,
    RunExecutor,
    EnqueueLateTurnInput,
    ReadSessionStateVersion,
    AdmitSessionState,
    LoadKnownNode,
    LoadUnknownNode,
    ReadShiftEpoch,
    /// The session fault's whole life (ADR 0109 §9): recorded once, kept
    /// against a second recording, read alone, on the shift epoch and in the
    /// listing, then cleared once.
    SessionFault,
    TurnsChangedSince,
    ListQueuedWork,
    ListPendingQueuedWork,
    PendingSessionWorkOrdering,
    EnqueueQueuedWorkWithOutcome,
    /// [`IngressStore::open_session_command_run`](lash_core::IngressStore::open_session_command_run):
    /// the command lane's bindless read of its leading ready run.
    OpenSessionCommandRun,
    /// [`RunStore::admit_at_checkpoint`](lash_core::store::RunStore::admit_at_checkpoint)
    /// of an after-work checkpoint, which the sweep's rows do not reach.
    AdmitAtCheckpoint,
    QueuedWorkBatchCompletion,
    CancelQueuedWorkBatch,
    CancelUnknownPendingTurnInput,
    CancelPendingTurnInputs,
    CancelPendingTurnInputSuffix,
    CommittedTurnExists,
    UncommittedTurnExists,
    /// [`SessionCommitStore::raise_pending_follow_on_attempts`], executed over a
    /// live fact for the turn it names (`owed`) and for a turn it does not.
    RaisePendingFollowOnAttempts {
        owed: bool,
    },
    /// [`SessionCommitStore::load_pending_follow_on`]: the head's owed
    /// follow-on as committed, read on an unowed head, beside a live fact,
    /// and after the fact's clearing commit.
    LoadPendingFollowOn,
    /// [`RunStore::run_terminal`](lash_core::store::RunStore::run_terminal)
    /// of the sweep's drain run: none while it is unfinished, and its lost
    /// end's evidence after it (FIG-3600 S7).
    RunTerminal,
    NonTerminalRunsPage,
    EndLostRun,
    /// [`RunStore::end_refused_run`](lash_core::store::RunStore::end_refused_run)
    /// of the sweep's drain run: its refusal's end, then nothing more
    /// (FIG-4018).
    EndRefusedRun,
    /// [`RunStore::end_command_run`](lash_core::store::RunStore::end_command_run)
    /// of a command run run under the sweep's lease: its end with the
    /// commands-applied cause, then the recorded end again (FIG-4202).
    EndCommandRun,
    /// [`RunStore::run_binding`](lash_core::store::RunStore::run_binding)
    /// of the sweep's next-turn input.
    RunBinding,
    /// [`RunStore::run_of_input`](lash_core::store::RunStore::run_of_input)
    /// of the sweep's next-turn input.
    RunOfInput,
    /// [`RunStore::bound_turn_scopes`](lash_core::store::RunStore::bound_turn_scopes)
    /// of the sweep's run: the turn scopes its bound inputs name.
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
    /// [`RunStore::bind_run_inputs`](lash_core::store::RunStore::bind_run_inputs)
    /// of the sweep's next-turn input: to the sweep run, and then to another
    /// run (`conflicting`), which every backend refuses without residue.
    BindRunInputs {
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
    /// [`ControlIntentStore::refuse_intent`](lash_core::store::ControlIntentStore::refuse_intent)
    /// of the case's close intent, for a typed cause, under the claim the
    /// run took on its obligation.
    RefuseIntent,
    /// [`ControlIntentStore::acknowledge_intent`](lash_core::store::ControlIntentStore::acknowledge_intent)
    /// of the case's close intent under the claim the run took on its
    /// obligation, or of an id no ledger minted.
    AcknowledgeIntent {
        known: bool,
    },
    RecordRunPark,
    OpenRunIntent {
        fork: bool,
        stale: bool,
    },
    ListControlIntents,
    AbortUnknownAttachmentWrite,
    AcquireUnknownAttachmentRefs,
    ForgetUnknownAttachment,
    ProbeAttachmentReferrers,
    ProbeSessionReferrerState,
    EndAttachmentReferrer,
    /// [`SessionCatalogStore::resolve_target`](lash_core::SessionCatalogStore::resolve_target)
    /// of the session's head revision, or of the revision past it
    /// (`published: false`), which every backend refuses as pending without
    /// residue (FIG-4731).
    ResolveTarget {
        published: bool,
    },
    /// [`SessionCatalogStore::retention`](lash_core::SessionCatalogStore::retention)
    /// of the case's session.
    ReadRetention,
    /// [`SessionCatalogStore::set_retention`](lash_core::SessionCatalogStore::set_retention)
    /// to a two-turn window.
    SetRetention,
    Vacuum,
}

impl SurfaceMethod {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::RefusedRootAdmission => "surface:refused_root_admission",
            Self::ToolReceipts => "surface:tool_receipts",
            Self::WaitReceipts => "surface:wait_receipts",
            Self::LoadSession => "surface:load_session",
            Self::ListPendingTurnInputs => "surface:list_pending_turn_inputs",
            Self::PendingTurnInput { known: true } => "surface:pending_turn_input",
            Self::PendingTurnInput { known: false } => "surface:pending_turn_input_unknown",
            Self::ListTurnInputApplications => "surface:list_turn_input_applications",
            Self::AdmitRun {
                lease: LeaseSlot::First,
            } => "surface:admit_run",
            Self::AdmitRun {
                lease: LeaseSlot::Successor,
            } => "surface:replay_admit_run",
            Self::AdmitRunAfterHeadSettled => "surface:admit_run_after_head_settled",
            Self::AdmitQueuedRun => "surface:admit_queued_run",
            Self::AdmitListedQueuedHead => "surface:admit_listed_queued_head",
            Self::UnfinishedRun => "surface:unfinished_run",
            Self::RunExecutor => "surface:run_executor",
            Self::EnqueueLateTurnInput => "surface:enqueue_late_turn_input",
            Self::ReadSessionStateVersion => "surface:read_session_state_version",
            Self::AdmitSessionState => "surface:admit_session_state",
            Self::LoadKnownNode => "surface:load_node_known",
            Self::LoadUnknownNode => "surface:load_node_unknown",
            Self::ReadShiftEpoch => "surface:read_shift_epoch",
            Self::SessionFault => "surface:session_fault",
            Self::TurnsChangedSince => "surface:turns_changed_since",
            Self::ListQueuedWork => "surface:list_queued_work",
            Self::ListPendingQueuedWork => "surface:list_open_queued_work",
            Self::PendingSessionWorkOrdering => "surface:pending_session_work_ordering",
            Self::EnqueueQueuedWorkWithOutcome => "surface:enqueue_queued_work_with_outcome",
            Self::OpenSessionCommandRun => "surface:open_session_command_run",
            Self::AdmitAtCheckpoint => "surface:admit_at_checkpoint",
            Self::QueuedWorkBatchCompletion => "surface:queued_work_batch_completion",
            Self::CancelQueuedWorkBatch => "surface:cancel_queued_work_batch",
            Self::CancelUnknownPendingTurnInput => "surface:cancel_pending_turn_input_unknown",
            Self::CancelPendingTurnInputs => "surface:cancel_pending_turn_inputs",
            Self::CancelPendingTurnInputSuffix => "surface:cancel_pending_turn_input_suffix",
            Self::CommittedTurnExists => "surface:committed_turn_exists_committed",
            Self::UncommittedTurnExists => "surface:committed_turn_exists_uncommitted",
            Self::RaisePendingFollowOnAttempts { owed: true } => {
                "surface:raise_pending_follow_on_attempts"
            }
            Self::RaisePendingFollowOnAttempts { owed: false } => {
                "surface:raise_pending_follow_on_attempts_unowed"
            }
            Self::LoadPendingFollowOn => "surface:load_pending_follow_on",
            Self::RunTerminal => "surface:run_terminal",
            Self::NonTerminalRunsPage => "surface:non_terminal_runs_page",
            Self::EndLostRun => "surface:end_lost_run",
            Self::EndRefusedRun => "surface:end_refused_run",
            Self::EndCommandRun => "surface:end_command_run",
            Self::RunBinding => "surface:run_binding",
            Self::RunOfInput => "surface:run_of_input",
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
            Self::BindRunInputs { conflicting: false } => "surface:bind_run_inputs",
            Self::BindRunInputs { conflicting: true } => "surface:bind_run_inputs_conflicting",
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
            Self::RefuseIntent => "surface:refuse_intent",
            Self::AcknowledgeIntent { known: true } => "surface:acknowledge_intent",
            Self::AcknowledgeIntent { known: false } => "surface:acknowledge_intent_unknown",
            Self::RecordRunPark => "surface:record_run_park",
            Self::OpenRunIntent { stale: true, .. } => "surface:open_run_intent_stale",
            Self::OpenRunIntent {
                stale: false,
                fork: false,
            } => "surface:open_run_intent_cancel",
            Self::OpenRunIntent {
                stale: false,
                fork: true,
            } => "surface:open_run_intent_fork",
            Self::ListControlIntents => "surface:list_control_intents",
            Self::AbortUnknownAttachmentWrite => "surface:abort_attachment_write_unknown",
            Self::AcquireUnknownAttachmentRefs => "surface:acquire_attachment_refs_unknown",
            Self::ForgetUnknownAttachment => "surface:forget_attachment_unknown",
            Self::ProbeAttachmentReferrers => "surface:attachment_referrers",
            Self::ProbeSessionReferrerState => "surface:session_referrer_state",
            Self::EndAttachmentReferrer => "surface:end_attachment_referrer",
            Self::ResolveTarget { published: true } => "surface:resolve_target",
            Self::ResolveTarget { published: false } => "surface:resolve_target_pending",
            Self::ReadRetention => "surface:retention",
            Self::SetRetention => "surface:set_retention",
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

#[expect(
    clippy::expect_used,
    reason = "the session referrer always permits an unguarded claim"
)]
fn unknown_attachment_write(session_id: &SessionId) -> lash_core::AttachmentWrite {
    lash_core::AttachmentWrite {
        attachment_id: unknown_attachment_id(),
        claim: lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::Session(
            session_id.clone(),
        ))
        .expect("the session referrer permits an unguarded claim"),
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
        model: Some(lash_core::LlmProfileKey::new("surface-run-spec-model")),
        ..lash_core::RunOverrides::default()
    })
}

/// The hash [`surface_run_spec`] is interned under. A spec that cannot be
/// hashed has none, and the reads comparing against it disagree loudly.
fn surface_run_spec_hash() -> Option<lash_core::RunSpecHash> {
    surface_run_spec().hash().ok().flatten()
}
/// A turn id no case ever commits. Paired with [`SURFACE_COMMITTED_TURN_ID`]
/// so the membership read is executed over both answers, not just the one a
/// backend could return by refusing to look.
const UNCOMMITTED_TURN_ID: &str = "fig-2841-uncommitted-turn";
/// The turn id the surface sweep's seed commit stamps.
const SURFACE_COMMITTED_TURN_ID: &str = "fig-2841-surface-committed-turn";
/// An attachment id no case ever writes. The attachment-referrer drivers use
/// it so the inventory covers those methods without mutating an attachment the
/// surrounding case depends on: an unknown entity is itself a refusal driver,
/// and whatever a backend answers, the no-residue law still applies.
const UNKNOWN_ATTACHMENT_ID: &str = "fig-2841-unknown-attachment";
const UNKNOWN_INPUT_ID: &str = "fig-2841-unknown-input";
/// The run the sweep binds its next-turn input to, and the other run a
/// second binding names.
const SURFACE_RUN_ID: &str = "fig-3600-run";
const SURFACE_OTHER_RUN_ID: &str = "fig-3600-other-run";
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
/// The queue drain whose run the sweep admits on a queued-work head and
/// ends. Its scope names the run, so every backend admits the same run and
/// the reads before admission ask about a run that genuinely does not exist
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
    lash_core::ExecutionScope::turn(session_id.clone(), SURFACE_DRAIN_ID)
}

/// The drain run the sweep admits on a queued-work head.
fn surface_queued_run(session_id: &SessionId) -> lash_core::TurnId {
    lash_core::TurnId::fixture(surface_drain_scope(session_id).id())
}

/// An admission request for `run` headed by `head` under `fence`.
fn surface_admit_request(
    fence: &lash_core::store::ShiftFence,
    run: lash_core::TurnId,
    head: lash_core::store::AdmittedHead,
) -> lash_core::store::AdmitRunRequest {
    let mut request =
        lash_core::testing::store_fixtures::admit_run_request_for_test(fence, &run, head);
    request.max_inputs = 8;
    request.policy = lash_core::testing::queued_work_admission_policy(1);
    request.admitted_generation = lash_core::engine::BuildGeneration::for_test("surface-run");
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
        lash_core::store::ControlIntentKind::Cancel { run, .. } => {
            format!("Cancel {{ run: {run} }}")
        }
        lash_core::store::ControlIntentKind::Redrive { run, .. } => {
            format!("Redrive {{ run: {run} }}")
        }
        lash_core::store::ControlIntentKind::Fork { run, new_run, .. } => format!(
            "Fork {{ run: {run}, direct: {}, derived_root: {} }}",
            new_run.is_some(),
            new_run
                .as_ref()
                .is_none_or(|new| *new == lash_core::store::forked_run(run, intent.id)),
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

/// Every fallible method in the inventory, executed against a live session.
pub(super) fn surface_sweep_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::StoreSurfaceSweep,
        operations: vec![
            // The seed commit stamps a turn so the inventory can work
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
                adopt_attachment: false,
            },
            StoreOperation::EnqueueNextTurnInput,
            StoreOperation::EnqueueAdmittableQueuedWork,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "surface-sweep-owner",
            },
            surface(SurfaceMethod::RefusedRootAdmission),
            surface(SurfaceMethod::ToolReceipts),
            surface(SurfaceMethod::WaitReceipts),
            surface(SurfaceMethod::ReadSessionStateVersion),
            surface(SurfaceMethod::AdmitSessionState),
            surface(SurfaceMethod::LoadKnownNode),
            surface(SurfaceMethod::LoadUnknownNode),
            surface(SurfaceMethod::ReadShiftEpoch),
            surface(SurfaceMethod::SessionFault),
            surface(SurfaceMethod::TurnsChangedSince),
            surface(SurfaceMethod::ListQueuedWork),
            surface(SurfaceMethod::ListPendingQueuedWork),
            surface(SurfaceMethod::PendingSessionWorkOrdering),
            surface(SurfaceMethod::EnqueueQueuedWorkWithOutcome),
            surface(SurfaceMethod::OpenSessionCommandRun),
            surface(SurfaceMethod::AdmitAtCheckpoint),
            surface(SurfaceMethod::QueuedWorkBatchCompletion),
            surface(SurfaceMethod::CancelQueuedWorkBatch),
            surface(SurfaceMethod::CommittedTurnExists),
            surface(SurfaceMethod::UncommittedTurnExists),
            // One queued-headed run end to end (FIG-3927): no unfinished
            // run, then its admission and a replay of it, and its lost end.
            surface(SurfaceMethod::UnfinishedRun),
            surface(SurfaceMethod::RunExecutor),
            surface(SurfaceMethod::AdmitQueuedRun),
            surface(SurfaceMethod::UnfinishedRun),
            surface(SurfaceMethod::RunExecutor),
            surface(SurfaceMethod::AdmitQueuedRun),
            surface(SurfaceMethod::RunTerminal),
            surface(SurfaceMethod::EndLostRun),
            // The lost end wrote its run's evidence (FIG-3600 S7).
            surface(SurfaceMethod::RunTerminal),
            surface(SurfaceMethod::UnfinishedRun),
            surface(SurfaceMethod::RunExecutor),
            // An input's run binding: unbound, bound once, read back, and a
            // second binding to another run refused.
            surface(SurfaceMethod::RunBinding),
            surface(SurfaceMethod::BoundTurnScopes),
            surface(SurfaceMethod::BindRunInputs { conflicting: false }),
            surface(SurfaceMethod::RunBinding),
            surface(SurfaceMethod::RunOfInput),
            surface(SurfaceMethod::BoundTurnScopes),
            surface(SurfaceMethod::BindRunInputs { conflicting: false }),
            surface(SurfaceMethod::BindRunInputs { conflicting: true }),
            surface(SurfaceMethod::RunBinding),
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
            surface(SurfaceMethod::AcquireUnknownAttachmentRefs),
            surface(SurfaceMethod::ForgetUnknownAttachment),
            surface(SurfaceMethod::ProbeAttachmentReferrers),
            surface(SurfaceMethod::ProbeSessionReferrerState),
            surface(SurfaceMethod::EndAttachmentReferrer),
            // With no pending follow-on on the head the raise meets its
            // `FollowOnNotPending` refusal.
            surface(SurfaceMethod::RaisePendingFollowOnAttempts { owed: true }),
            surface(SurfaceMethod::LoadPendingFollowOn),
            // The head resolves to itself; the revision past it names no
            // state yet. The policy reads back its default, then what was set.
            surface(SurfaceMethod::ResolveTarget { published: true }),
            surface(SurfaceMethod::ResolveTarget { published: false }),
            surface(SurfaceMethod::ReadRetention),
            surface(SurfaceMethod::SetRetention),
            surface(SurfaceMethod::ReadRetention),
            surface(SurfaceMethod::Vacuum),
        ],
    }
}

/// A failed engine execution settles the same open run on every SQL backend.
pub(super) fn lost_run_recovery_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::LostRunRecovery,
        operations: vec![
            StoreOperation::Commit {
                label: "seed_lost_run_graph",
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
                adopt_attachment: false,
            },
            StoreOperation::EnqueueAdmittableQueuedWork,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "lost-run-owner",
            },
            surface(SurfaceMethod::AdmitQueuedRun),
            surface(SurfaceMethod::NonTerminalRunsPage),
            surface(SurfaceMethod::EndLostRun),
            surface(SurfaceMethod::RunTerminal),
            surface(SurfaceMethod::EndLostRun),
            surface(SurfaceMethod::NonTerminalRunsPage),
        ],
    }
}

/// A run whose execution met a typed refusal ends the same way on every SQL
/// backend, once: a second end, and the lost-run end after it, write
/// nothing (FIG-4018).
pub(super) fn refused_run_end_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::RunEndOutcome,
        operations: vec![
            StoreOperation::Commit {
                label: "seed_refused_run_graph",
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
                adopt_attachment: false,
            },
            StoreOperation::EnqueueAdmittableQueuedWork,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "refused-run-owner",
            },
            surface(SurfaceMethod::AdmitQueuedRun),
            surface(SurfaceMethod::UnfinishedRun),
            surface(SurfaceMethod::RunExecutor),
            surface(SurfaceMethod::EndRefusedRun),
            surface(SurfaceMethod::RunTerminal),
            surface(SurfaceMethod::UnfinishedRun),
            surface(SurfaceMethod::RunExecutor),
            surface(SurfaceMethod::EndRefusedRun),
            surface(SurfaceMethod::EndLostRun),
            surface(SurfaceMethod::RunTerminal),
            surface(SurfaceMethod::EndCommandRun),
            surface(SurfaceMethod::EndCommandRun),
            surface(SurfaceMethod::NonTerminalRunsPage),
        ],
    }
}

/// The run's admission is replayed after a lease handoff, even when another
/// input becomes eligible between the admission commit and the journal write.
pub(super) fn run_admission_replay_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::RunAdmissionReplay,
        operations: vec![
            StoreOperation::EnqueueNextTurnInput,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "run-admission-first",
            },
            surface(SurfaceMethod::AdmitRun {
                lease: LeaseSlot::First,
            }),
            StoreOperation::ReleaseSessionLease {
                lease: LeaseSlot::First,
            },
            surface(SurfaceMethod::EnqueueLateTurnInput),
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::Successor,
                owner: "run-admission-successor",
            },
            surface(SurfaceMethod::AdmitRun {
                lease: LeaseSlot::Successor,
            }),
            surface(SurfaceMethod::UnfinishedRun),
            surface(SurfaceMethod::RunExecutor),
            surface(SurfaceMethod::CancelPendingTurnInputs),
            surface(SurfaceMethod::AdmitRunAfterHeadSettled),
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
            surface(SurfaceMethod::ReadShiftEpoch),
            surface(SurfaceMethod::ListQueuedWork),
            surface(SurfaceMethod::ListPendingQueuedWork),
            surface(SurfaceMethod::PendingSessionWorkOrdering),
            surface(SurfaceMethod::EnqueueQueuedWorkWithOutcome),
            surface(SurfaceMethod::OpenSessionCommandRun),
            surface(SurfaceMethod::AdmitAtCheckpoint),
            surface(SurfaceMethod::CancelUnknownPendingTurnInput),
            surface(SurfaceMethod::UnfinishedRun),
            surface(SurfaceMethod::RunExecutor),
            surface(SurfaceMethod::AdmitQueuedRun),
            surface(SurfaceMethod::RaisePendingFollowOnAttempts { owed: true }),
            surface(SurfaceMethod::LoadPendingFollowOn),
            surface(SurfaceMethod::CancelQueuedWorkBatch),
            surface(SurfaceMethod::AcquireUnknownAttachmentRefs),
            surface(SurfaceMethod::ForgetUnknownAttachment),
            surface(SurfaceMethod::ProbeAttachmentReferrers),
            surface(SurfaceMethod::ProbeSessionReferrerState),
            surface(SurfaceMethod::EndAttachmentReferrer),
            surface(SurfaceMethod::Vacuum),
        ],
    }
}

/// A session's close through the factory's control-intent ledger (FIG-3600
/// S7): the store half ends the session's open runs — an input's bound
/// run and an admitted queued-headed run — `Cancelled` by the close, a retry
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
            surface(SurfaceMethod::BindRunInputs { conflicting: false }),
            surface(SurfaceMethod::AdmitQueuedRun),
            surface(SurfaceMethod::RunTerminal),
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
            // The close ended the drain run by the session's deletion.
            surface(SurfaceMethod::RunTerminal),
            surface(SurfaceMethod::UnfinishedRun),
            surface(SurfaceMethod::RunExecutor),
            surface(SurfaceMethod::LoadIntent { known: true }),
            surface(SurfaceMethod::ClaimIntentApplication { known: true }),
            surface(SurfaceMethod::AcknowledgeIntent { known: true }),
            surface(SurfaceMethod::ClaimIntentApplication { known: true }),
            // A late refusal never reopens an acknowledged intent.
            surface(SurfaceMethod::RefuseIntent),
            surface(SurfaceMethod::AcknowledgeIntent { known: true }),
            surface(SurfaceMethod::LoadIntent { known: true }),
        ],
    }
}

pub(super) fn run_control_case(fork: bool) -> GeneratedCase {
    let mut case = session_close_ledger_case();
    case.name = if fork {
        CaseName::RunForkLedger
    } else {
        CaseName::RunCancelLedger
    };
    case.operations.truncate(5);
    case.operations.extend([
        surface(SurfaceMethod::RecordRunPark),
        surface(SurfaceMethod::OpenRunIntent { fork, stale: true }),
        surface(SurfaceMethod::OpenRunIntent { fork, stale: false }),
        surface(SurfaceMethod::LoadIntent { known: true }),
        surface(SurfaceMethod::ListControlIntents),
        surface(SurfaceMethod::ListPendingTurnInputs),
        surface(SurfaceMethod::RunBinding),
        surface(SurfaceMethod::ClaimIntentApplication { known: true }),
        surface(SurfaceMethod::AcknowledgeIntent { known: true }),
    ]);
    case
}

impl BackendRunner {
    /// Execute one inventory method. Returns `Err` verbatim: the step loop owns
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
            .unwrap_or_else(|| unheld_shift_fence(&session_id));
        let answer = match method {
            SurfaceMethod::RefusedRootAdmission => {
                let identity = lash_core::store::AdmissionId::new("surface-root#0");
                let executor = lash_core::store::RunExecutor::run(&identity);
                assert!(
                    store
                        .read_shift_admission(&session_id, &identity)
                        .await?
                        .is_none()
                );
                let mut preparation = store
                    .prepare_shift_admission(&session_id, &identity, &executor)
                    .await?;
                let Some(mut selection) = preparation.selection.take() else {
                    panic!("the seeded surface turn is pending");
                };
                let head = match &selection.work {
                    lash_core::store::AdmittedWork::Input { head } => {
                        lash_core::store::AdmittedHead::Input(head.clone())
                    }
                    lash_core::store::AdmittedWork::Queued { head } => {
                        lash_core::store::AdmittedHead::Batch(head.clone())
                    }
                    other => panic!("surface root has turn work: {other:?}"),
                };
                let mut run = surface_admit_request(
                    &preparation.prospective_fence,
                    selection.run.clone(),
                    head,
                );
                run.executor = executor.clone();
                run.unsealed_epoch = Some(preparation.epoch.epoch);
                let Some(run) = store.prepare_run_admission(&run).await? else {
                    panic!("the surface composition reaches its head");
                };
                selection.observed_epoch += 1;
                preparation.selection = Some(selection);
                let result = store
                    .commit_shift_admission(
                        &lash_core::store::ShiftAdmissionWrite {
                            session_id: session_id.clone(),
                            admission: identity,
                            run_start: lash_core::engine::RunStartNonce::new("surface-nonce"),
                            executor,
                            preparation,
                            run: Some(run),
                        },
                        &lash_core::TraceAnchor::Untraced,
                    )
                    .await;
                assert!(
                    matches!(result, Err(StoreError::PreparedRunAdmissionStale { .. })),
                    "changed root selection is refused: {result:?}"
                );
                "selection_changed_without_writes".to_owned()
            }

            SurfaceMethod::ToolReceipts => tool_receipt_law(store.as_ref(), &session_id).await?,
            SurfaceMethod::WaitReceipts => wait_receipt_law(store.as_ref(), &session_id).await?,
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
                // row: `Open`, `Admitted` naming its run, or nothing once
                // the row is terminal or unknown (FIG-3976).
                let input = lash_core::InputId::fixture(if known {
                    format!("{session_id}:input")
                } else {
                    UNKNOWN_INPUT_ID.to_string()
                });
                match store.pending_turn_input(&session_id, &input).await? {
                    Some(read) => match &read.status {
                        lash_core::PendingTurnInputReadStatus::Open => "status=open".to_string(),
                        lash_core::PendingTurnInputReadStatus::Admitted { run } => {
                            format!("status=admitted:{run}")
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
            SurfaceMethod::AdmitRun { lease } => {
                let lease_slot = lease;
                let lease = self.lease(lease_slot).clone();
                let head = lash_core::InputId::fixture(format!("{session_id}:input"));
                let request = surface_admit_request(
                    &lease,
                    lash_core::TurnId::from(SURFACE_RUN_ID),
                    lash_core::store::AdmittedHead::Input(head.clone()),
                );
                // A replay that differs, a refusal or a widened prefix is a
                // law violation on this backend, never an answer to compare.
                let prepared = store
                    .prepare_run_admission(&request)
                    .await?
                    .unwrap_or_else(|| panic!("the prepared composition reaches its head"));
                let Some(admission) = store
                    .commit_run_admission(&prepared, &lash_core::TraceAnchor::Untraced)
                    .await?
                else {
                    panic!("{}: run admission did not reach its head", self.name);
                };
                let encoded = serde_json::to_value(&admission)
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                match &self.surface.run_admission {
                    Some(recorded) => assert_eq!(
                        *recorded, encoded,
                        "{}: a successor's run admission must return the recorded result",
                        self.name
                    ),
                    None => self.surface.run_admission = Some(encoded),
                }
                assert_eq!(
                    admission.input_ids(),
                    vec![head],
                    "{}: the run admission must not widen past its recorded prefix",
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
                // Queued between a run's claim commit and its replay: it must
                // never widen the recorded prefix.
                store
                    .enqueue_pending_turn_input(
                        PendingTurnInputDraft::new(
                            &session_id,
                            TurnInputIngress::NextTurn,
                            TurnInput::text("late input after run claim"),
                        )
                        .with_input_id(lash_core::InputId::fixture(
                            format!("{session_id}:late-input"),
                        )),
                    )
                    .await?;
                "enqueued".to_string()
            }
            SurfaceMethod::AdmitRunAfterHeadSettled => {
                // The row may have settled, but the run's admission remains
                // the answer of record and cannot widen to the later row.
                let lease = self.lease(LeaseSlot::Successor).clone();
                let request = surface_admit_request(
                    &lease,
                    lash_core::TurnId::from(SURFACE_RUN_ID),
                    lash_core::store::AdmittedHead::Input(lash_core::InputId::fixture(format!(
                        "{session_id}:input"
                    ))),
                );
                let Some(admission) = store.admit_run(&request).await? else {
                    panic!("{}: recorded run admission disappeared", self.name);
                };
                let encoded = serde_json::to_value(admission)
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                assert_eq!(self.surface.run_admission.as_ref(), Some(&encoded));
                "recorded".to_string()
            }
            SurfaceMethod::AdmitQueuedRun => {
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
                let head = match (&self.surface.queued_run_admission, head) {
                    (Some(recorded), _) => {
                        serde_json::from_value::<lash_core::store::RunAdmission>(recorded.clone())
                            .map_err(|error| StoreError::Backend(error.to_string()))?
                            .head
                    }
                    (None, Some(batch)) => lash_core::store::AdmittedHead::Batch(batch),
                    (None, None) => lash_core::store::AdmittedHead::Batch(
                        lash_core::BatchId::from(UNKNOWN_BATCH_ID),
                    ),
                };
                let request =
                    surface_admit_request(&lease_fence, surface_queued_run(&session_id), head);
                match store.admit_run(&request).await? {
                    None => "admitted=false".to_string(),
                    Some(admission) => {
                        let encoded = serde_json::to_value(&admission)
                            .map_err(|error| StoreError::Backend(error.to_string()))?;
                        let replay = match &self.surface.queued_run_admission {
                            Some(recorded) => {
                                assert_eq!(
                                    *recorded, encoded,
                                    "{}: a replayed admission must return the recorded result",
                                    self.name
                                );
                                true
                            }
                            None => {
                                self.surface.queued_run_admission = Some(encoded);
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
            SurfaceMethod::RunExecutor => {
                let executor = store
                    .run_executor(&session_id, &surface_queued_run(&session_id))
                    .await?;
                format!(
                    "executor={}",
                    serde_json::to_string(&executor)
                        .map_err(|error| StoreError::Backend(error.to_string()))?
                )
            }
            SurfaceMethod::UnfinishedRun => match store.unfinished_run(&session_id).await? {
                None => "unfinished=none".to_string(),
                Some(unfinished) => format!(
                    "unfinished_run={} head={}",
                    if unfinished.run == surface_queued_run(&session_id) {
                        "drain".to_string()
                    } else {
                        unfinished.run.to_string()
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
                    surface_admit_request(&lease_fence, surface_queued_run(&session_id), head);
                let admission = store.admit_run(&request).await?;
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
                        lash_core::store::HistoryAnchor::Node(lash_core::NodeId::fixture(node_id)),
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
            SurfaceMethod::ReadShiftEpoch => {
                let observed = store.shift_epoch(&session_id).await?;
                format!(
                    "epoch={} admission_present={}",
                    observed.epoch,
                    observed.admission().is_some()
                )
            }
            SurfaceMethod::SessionFault => {
                let record = |message: &str| lash_core::store::SessionFaultRecord {
                    origin: lash_core::store::SessionFaultOrigin::DriveAdmission,
                    code: lash_core::RuntimeErrorCode::RuntimeStoreCorrupt,
                    message: message.to_string(),
                    cause: None,
                };
                let first = store
                    .record_session_fault(&session_id, &record("first"), 7)
                    .await?;
                let kept = store
                    .record_session_fault(&session_id, &record("second"), 9)
                    .await?;
                let read = store.session_fault(&session_id).await?;
                let gate = store.shift_epoch(&session_id).await?.fault;
                let listed = store
                    .list_session_faults(None, std::num::NonZeroUsize::MAX)
                    .await?
                    .into_iter()
                    .filter(|fault| fault.session_id == session_id)
                    .collect::<Vec<_>>();
                let cleared = store.clear_session_fault(&session_id).await?;
                let again = store.clear_session_fault(&session_id).await?;
                let after = store.session_fault(&session_id).await?;
                format!(
                    "first={first:?} kept_first={} read={} gate={} listed={} cleared={cleared} \
                     again={again} after_present={}",
                    kept == first,
                    read == first,
                    gate == first,
                    listed.len(),
                    after.is_some()
                )
            }
            SurfaceMethod::TurnsChangedSince => {
                let factory = self.factory();
                let page = match factory
                    .turns_changed_since(
                        lash_core::store::TurnChangeCursor::initial(),
                        std::num::NonZeroUsize::MAX,
                    )
                    .await
                {
                    // PostgreSQL retains a deployment clock across cases.
                    Err(StoreError::TurnChangeCursorPruned { horizon }) => {
                        factory
                            .turns_changed_since(horizon, std::num::NonZeroUsize::MAX)
                            .await?
                    }
                    result => result?,
                };
                let rows: Vec<_> = page
                    .changes
                    .into_iter()
                    .filter(|change| change.session_id == session_id)
                    .map(|change| format!("{:?}", change.kind))
                    .collect();
                format!("changes={rows:?}")
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
                let command_run = store.open_session_command_run(&lease_fence).await?;
                format!("commands={}", command_run.len())
            }
            SurfaceMethod::AdmitAtCheckpoint => {
                let admission = store
                    .admit_at_checkpoint(&lash_core::store::CheckpointAdmissionRequest {
                        fence: lease_fence.clone(),
                        run: lash_core::TurnId::from(SURFACE_CHECKPOINT_RUN_ID),
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
            SurfaceMethod::QueuedWorkBatchCompletion => {
                let completed = store
                    .queued_work_batch_completion(&session_id, UNKNOWN_BATCH_ID)
                    .await?
                    .is_some();
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
                        &lash_core::engine::BuildGeneration::for_test("surface-recovering"),
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
            SurfaceMethod::RunTerminal => {
                // The kind and cause are caller-supplied facts; the instant
                // is the backend clock's and is not compared.
                let run = lash_core::TurnId::fixture(surface_drain_scope(&session_id).id());
                match store.run_terminal(&session_id, &run).await? {
                    Some(terminal) => {
                        let cause = match &terminal.cause {
                            lash_core::store::RunTerminalCause::SessionDeleted { intent } => {
                                format!(
                                    "SessionDeleted(by_own_close={})",
                                    Some(*intent) == self.surface.close_intent
                                )
                            }
                            other => format!("{other:?}"),
                        };
                        format!("kind={:?} cause={cause}", terminal.kind())
                    }
                    None => "terminal=none".to_string(),
                }
            }
            SurfaceMethod::NonTerminalRunsPage => {
                let factory = self.factory();
                let mut after = None;
                let mut own_executors = Vec::new();
                loop {
                    let page = factory
                        .non_terminal_runs_page(
                            after.as_ref(),
                            std::num::NonZeroUsize::MIN.saturating_add(127),
                        )
                        .await?;
                    own_executors.extend(
                        page.iter()
                            .filter(|open| open.target.session == session_id)
                            .map(|open| format!("{:?}", open.executor)),
                    );
                    if page.len() < 128 {
                        break;
                    }
                    after = page.last().map(|open| open.target.clone());
                }
                format!(
                    "own_open_runs={} executors={own_executors:?}",
                    own_executors.len()
                )
            }
            SurfaceMethod::EndLostRun => {
                let run = lash_core::engine::RunRef {
                    session: session_id.clone(),
                    run: lash_core::TurnId::fixture(surface_drain_scope(&session_id).id()),
                };
                match self
                    .factory()
                    .end_lost_run(&run, lash_core::engine::RunLoss::NoRun, 1)
                    .await?
                {
                    Some(terminal) => format!("ended={:?}", terminal.kind()),
                    None => "ended=none".to_string(),
                }
            }
            SurfaceMethod::EndRefusedRun => {
                let run = lash_core::TurnId::fixture(surface_drain_scope(&session_id).id());
                let refusal = lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::StoreCommitSuperseded,
                    "the head moved under the run's commit",
                );
                match store
                    .end_refused_run(&lease_fence, &run, &refusal, 1)
                    .await?
                {
                    lash_core::store::RunEndOutcome::Ended(terminal) => {
                        format!("ended={:?}", terminal.kind())
                    }
                    lash_core::store::RunEndOutcome::AlreadyEnded(terminal) => {
                        format!("already_ended={:?}", terminal.kind())
                    }
                    lash_core::store::RunEndOutcome::Superseded => "superseded".to_string(),
                    lash_core::store::RunEndOutcome::Unknown => "ended=none".to_string(),
                }
            }
            SurfaceMethod::EndCommandRun => {
                let run =
                    lash_core::TurnId::fixture(format!("shift-commands:{session_id}-surface"));
                let end = match store.end_command_run(&lease_fence, &run, 1).await? {
                    lash_core::store::RunEndOutcome::Ended(terminal) => {
                        format!("ended={:?}/{:?}", terminal.kind(), terminal.cause)
                    }
                    lash_core::store::RunEndOutcome::AlreadyEnded(terminal) => {
                        format!("already_ended={:?}/{:?}", terminal.kind(), terminal.cause)
                    }
                    lash_core::store::RunEndOutcome::Superseded => "superseded".to_string(),
                    lash_core::store::RunEndOutcome::Unknown => "ended=none".to_string(),
                };
                let recorded = store
                    .run_terminal(&session_id, &run)
                    .await?
                    .map(|terminal| format!("{:?}", terminal.kind()));
                format!("{end} recorded={recorded:?}")
            }
            SurfaceMethod::RunBinding => {
                let input = lash_core::InputId::fixture(format!("{session_id}:input"));
                match store.run_binding(&session_id, &input).await? {
                    Some(run) => {
                        let forked = self.surface.close_intent.is_some_and(|intent| {
                            run == lash_core::store::forked_run(&SURFACE_RUN_ID.into(), intent)
                        });
                        if forked {
                            "bound=own_fork".to_string()
                        } else {
                            format!("bound={run}")
                        }
                    }
                    None => "bound=none".to_string(),
                }
            }
            SurfaceMethod::RunOfInput => {
                let input = lash_core::InputId::fixture(format!("{session_id}:input"));
                match store.run_of_input(&session_id, &input).await? {
                    Some(run) => format!("run={run}"),
                    None => "run=none".to_string(),
                }
            }
            SurfaceMethod::BoundTurnScopes => {
                let run = lash_core::TurnId::from(SURFACE_RUN_ID);
                let scopes = store
                    .bound_turn_scopes(&session_id, &run)
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
                        .with_input_id(lash_core::InputId::fixture(format!(
                            "{session_id}:run-spec-input"
                        )))
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
                            draft(added, "input added by a batch").with_input_id(
                                lash_core::InputId::fixture(format!("{session_id}:{added}")),
                            ),
                            draft("surface:run-spec-input", resent_text).with_input_id(
                                lash_core::InputId::fixture(format!("{session_id}:run-spec-input")),
                            ),
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
                    .with_input_id(lash_core::InputId::fixture(format!("{session_id}:{key}")))
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
            SurfaceMethod::BindRunInputs { conflicting } => {
                let run = lash_core::TurnId::from(if conflicting {
                    SURFACE_OTHER_RUN_ID
                } else {
                    SURFACE_RUN_ID
                });
                let input = lash_core::InputId::fixture(format!("{session_id}:input"));
                store.bind_run_inputs(&session_id, &run, &[input]).await?;
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
            SurfaceMethod::RefuseIntent => {
                let claim = self.intent_claim(true).await?;
                match self
                    .factory()
                    .refuse_intent(
                        self.case_intent(true),
                        &claim,
                        &lash_core::store::DeliveryError::new(
                            lash_core::RuntimeErrorCode::EngineHandleMismatch,
                            "fig-3600 engine half refused",
                        ),
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
            SurfaceMethod::RecordRunPark => {
                let park = store
                    .record_turn_park(&lash_core::store::TurnParkWrite::refusal(
                        session_id.clone(),
                        SURFACE_RUN_ID.into(),
                        lash_core::store::ParkReason::ReplayDivergence {
                            message: "differential".into(),
                        },
                        CLOSE_AT_MS,
                    ))
                    .await
                    .map(lash_core::store::StoreTransition::into_record)?;
                format!("parked={}", park.attempts)
            }
            SurfaceMethod::OpenRunIntent { fork, stale } => {
                let park = store
                    .load_turn_park(&session_id)
                    .await?
                    .ok_or(StoreError::Contended)?;
                let request = lash_core::store::RunIntentRequest {
                    session_id: session_id.clone(),
                    run: SURFACE_RUN_ID.into(),
                    park: lash_core::store::ParkId::from_feed_sequence(
                        park.park_id.feed_sequence() + u64::from(stale),
                    ),
                    verb: if fork {
                        lash_core::store::RunVerb::Fork
                    } else {
                        lash_core::store::RunVerb::Cancel
                    },
                };
                let intent = self
                    .factory()
                    .open_run_intent(&request, CLOSE_AT_MS)
                    .await
                    .map_err(|error| match error {
                        lash_core::store::RunIntentRefused::Store(error) => error,
                        error => StoreError::Backend(format!("run intent refused: {error}")),
                    })?;
                let returned_park = match &intent.kind {
                    lash_core::store::ControlIntentKind::Cancel { park, .. }
                    | lash_core::store::ControlIntentKind::Fork { park, .. } => *park,
                    other => panic!("unexpected run intent: {other:?}"),
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
                let intent = unknown_attachment_write(&session_id);
                let outcome = match store.begin_attachment_write(&intent).await? {
                    lash_core::AttachmentWriteFence::Granted(permit) => {
                        store.abort_attachment_write(&intent, permit).await?;
                        "aborted"
                    }
                    _ => "not_granted",
                };
                format!("outcome={outcome}")
            }
            SurfaceMethod::AcquireUnknownAttachmentRefs => {
                store
                    .acquire_attachment_refs(
                        &unknown_attachment_write(&session_id).claim,
                        &[unknown_attachment_id()],
                    )
                    .await?;
                "acquired".to_string()
            }
            SurfaceMethod::ForgetUnknownAttachment => {
                store
                    .forget_attachment_ref(
                        &lash_core::ArtifactReferrer::Session(session_id.clone()),
                        &unknown_attachment_id(),
                    )
                    .await?;
                "forgotten".to_string()
            }
            SurfaceMethod::ProbeAttachmentReferrers => format!(
                "refs={:?}",
                store.attachment_referrers(&unknown_attachment_id()).await?
            ),
            SurfaceMethod::ProbeSessionReferrerState => format!(
                "state={:?}",
                store.session_referrer_state(&session_id).await?
            ),
            SurfaceMethod::EndAttachmentReferrer => {
                let referrer = lash_core::ArtifactReferrer::ProcessRecord(
                    lash_core::ProcessId::fixture(&format!("surface-ended:{session_id}")),
                );
                store.end_attachment_referrer(&referrer).await?;
                "ended".to_string()
            }
            SurfaceMethod::ResolveTarget { published } => {
                let head = self.head_revision().await?;
                let target = lash_core::Target::Revision(if published { head } else { head + 1 });
                let revision = self.factory().resolve_target(&session_id, &target).await?;
                format!(
                    "revision={} leaf_present={} checkpoint_present={} head={} pins={}",
                    revision.head_revision,
                    revision.leaf_node_id.is_some(),
                    revision.checkpoint_ref.is_some(),
                    revision.head,
                    revision.pinned_by.len()
                )
            }
            SurfaceMethod::ReadRetention => {
                format!("{:?}", self.factory().retention(&session_id).await?)
            }
            SurfaceMethod::SetRetention => {
                self.factory()
                    .set_retention(
                        &session_id,
                        lash_core::Retention::LastTurns(
                            std::num::NonZeroU32::MIN.saturating_add(1),
                        ),
                    )
                    .await?;
                "set".to_string()
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
            .and_then(|intent| intent.obligation.map(|obligation| obligation.id))
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
        reason = "test support: the case executes its close before any known-intent step; a missing intent panics the harness with its case name by design"
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

async fn tool_receipt_law(
    store: &dyn RuntimeStore,
    session_id: &SessionId,
) -> Result<String, StoreError> {
    let owners = [
        lash::tracing::TraceToolOwner::Turn {
            session_id: session_id.clone(),
            turn_id: "receipt-turn".into(),
        },
        lash::tracing::TraceToolOwner::Operation {
            session_id: session_id.clone(),
            operation_id: "receipt-operation".into(),
        },
        lash::tracing::TraceToolOwner::Process {
            process_id: lash_sansio::ProcessId::fixture("receipt-process"),
        },
    ];
    for (index, owner) in owners.into_iter().enumerate() {
        let request = lash_core::store::ToolRequestReceipt {
            owner,
            request_key: format!("{session_id}:receipt-tool:{index}"),
            payload_digest: "first-digest".into(),
            payload: serde_json::json!({"prepared": "first"}),
            scope: None,
            context: Default::default(),
            requested_at_ms: 7,
        };
        if store
            .tool_request_receipt(&request.request_key)
            .await?
            .is_some()
        {
            return Err(StoreError::Backend(
                "unaccepted request lookup law failed".into(),
            ));
        }
        let first = store.record_tool_request(&request).await?;
        if store.tool_request_receipt(&request.request_key).await? != Some(first.record.clone()) {
            return Err(StoreError::Backend(
                "accepted request lookup law failed".into(),
            ));
        }
        let mut retry = request.clone();
        retry.requested_at_ms = 99;
        let reused = store.record_tool_request(&retry).await?;
        if !first.changed
            || reused.changed
            || reused.record != first.record
            || reused.permit().is_some()
        {
            return Err(StoreError::Backend(
                "request first-writer law failed".into(),
            ));
        }
        retry.owner = lash::tracing::TraceToolOwner::Process {
            process_id: lash_sansio::ProcessId::fixture("another-owner"),
        };
        if !matches!(
            store.record_tool_request(&retry).await,
            Err(StoreError::ToolRequestConflict { .. })
        ) {
            return Err(StoreError::Backend("owner conflict law failed".into()));
        }
        retry.owner = request.owner.clone();
        retry.payload_digest = "conflicting-digest".into();
        if !matches!(
            store.record_tool_request(&retry).await,
            Err(StoreError::ToolRequestConflict { .. })
        ) {
            return Err(StoreError::Backend("request conflict law failed".into()));
        }
        let completion = lash_core::store::ToolCompletionReceipt {
            owner: request.owner.clone(),
            request_key: request.request_key.clone(),
            payload_digest: request.payload_digest.clone(),
            result: serde_json::json!({"output":"first"}),
            intent_outcomes: serde_json::json!([]),
            completed_at_ms: 11,
        };
        let completed = store.record_tool_completion(&completion).await?;
        let mut retry = completion;
        retry.completed_at_ms = 101;
        retry.result = serde_json::json!({"output":"later"});
        let reused = store.record_tool_completion(&retry).await?;
        if !completed.changed
            || reused.changed
            || reused.record != completed.record
            || reused.permit().is_some()
        {
            return Err(StoreError::Backend(
                "completion first-writer law failed".into(),
            ));
        }
    }
    Ok("request=first completion=first conflicts=typed permits=one".into())
}

#[tokio::test]
async fn tool_receipt_sweep_retains_only_committed_first_writers() {
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("SQLite memory");
    let session_id = SessionId::fixture("tool-receipt-sweep");
    let request = SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: session_id.clone(),
        relation: SessionRelation::Root,
        config: lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        )
        .into(),
        head: SessionCreationHead::Config,
    };
    let store = admit_test_session(stores.session_store_factory(), &request)
        .await
        .expect("admit sweep session");
    assert_eq!(
        tool_receipt_law(store.as_ref(), &session_id)
            .await
            .expect("actual sweep drivers"),
        "request=first completion=first conflicts=typed permits=one"
    );
}

async fn wait_receipt_law(
    store: &dyn RuntimeStore,
    session: &SessionId,
) -> Result<String, StoreError> {
    let request = lash_core::store::WaitRequestReceipt {
        wait_id: format!("{session}:wait"),
        owner_key: format!("{session}:wait-owner"),
        session_id: Some(session.clone()),
        request_digest: "request-digest".into(),
        kind: lash_core::store::EngineWaitKind::Event,
        scope: None,
        context: Default::default(),
        started_at_ms: 7,
    };
    let first = store.record_wait_request(&request).await?;
    let mut retry = request.clone();
    retry.started_at_ms = 99;
    let reused = store.record_wait_request(&retry).await?;
    assert!(first.changed && first.permit().is_some());
    assert!(!reused.changed && reused.permit().is_none());
    assert_eq!(reused.record, first.record);
    retry.request_digest = "changed".into();
    assert!(matches!(
        store.record_wait_request(&retry).await,
        Err(StoreError::WaitReceiptConflict { .. })
    ));
    let resolution = lash_core::store::WaitResolutionReceipt {
        wait_id: request.wait_id.clone(),
        resolution_digest: "resolution-digest".into(),
        resolution: serde_json::json!({"accepted":true}),
        resolved_at_ms: 11,
    };
    let first = store.record_wait_resolution(&resolution).await?;
    let mut retry = resolution;
    retry.resolved_at_ms = 101;
    let reused = store.record_wait_resolution(&retry).await?;
    assert!(first.changed && first.permit().is_some());
    assert!(!reused.changed && reused.permit().is_none());
    assert_eq!(reused.record, first.record);
    retry.resolution_digest = "changed".into();
    assert!(matches!(
        store.record_wait_resolution(&retry).await,
        Err(StoreError::WaitReceiptConflict { .. })
    ));
    store
        .retire_observation_receipts(&request.owner_key, 12)
        .await?;
    Ok("wait=request-first resolution-first conflicts=typed permits=one".into())
}
#[tokio::test]
async fn wait_receipt_sweep_retains_committed_times_and_reclaims_only_retired_owners() {
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("SQLite memory");
    let store = stores.session_store_factory();
    let session = SessionId::fixture("wait-receipt-sweep");
    assert_eq!(
        wait_receipt_law(store.as_ref(), &session)
            .await
            .expect("actual sweep driver"),
        "wait=request-first resolution-first conflicts=typed permits=one"
    );
    let active = lash_core::store::WaitRequestReceipt {
        wait_id: "active-wait".into(),
        owner_key: "active-owner".into(),
        session_id: None,
        request_digest: "active".into(),
        kind: lash_core::store::EngineWaitKind::Timer,
        scope: None,
        context: Default::default(),
        started_at_ms: 5,
    };
    store
        .record_wait_request(&active)
        .await
        .expect("active wait");
    let tool = lash_core::store::ToolRequestReceipt {
        owner: lash::tracing::TraceToolOwner::Process {
            process_id: lash_sansio::ProcessId::fixture("retiring-tool-owner"),
        },
        request_key: "retiring-tool".into(),
        payload_digest: "tool".into(),
        payload: serde_json::Value::Null,
        scope: None,
        context: Default::default(),
        requested_at_ms: 5,
    };
    store
        .record_tool_request(&tool)
        .await
        .expect("process request without a session");
    store
        .retire_observation_receipts(&tool.owner_key().unwrap(), 12)
        .await
        .expect("retire tool owner");
    let report = store
        .reclaim_retained_evidence(lash_core::store::RetentionBound {
            committed_before_epoch_ms: 13,
            turn_watermark: lash_core::store::TurnProjectionWatermark::NoProjector,
        })
        .await
        .expect("retention sweep");
    assert_eq!(report.removed_receipt_count, 2);
    let reused = store
        .record_wait_request(&active)
        .await
        .expect("active retained");
    assert!(!reused.changed);
    assert_eq!(reused.record, active);
}
