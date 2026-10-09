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
    /// The `CloseSession` intent this backend's ledger minted for the case's
    /// session: ids are the backend's own clock, so answers compare it by
    /// identity, never by value.
    pub(super) close_intent: Option<lash_core::store::ControlIntentId>,
}

/// One fallible store-trait method, executed as a compared differential step.
#[derive(Clone, Copy, Debug)]
pub(super) enum SurfaceMethod {
    LoadSession,
    ListPendingTurnInputs,
    /// [`IngressStore::pending_turn_input`]: the keyed point read of the
    /// case's next-turn input (`known`), or of an id no case enqueues
    /// (FIG-3976).
    PendingTurnInput {
        known: bool,
    },
    ListTurnInputApplications,
    /// [`RunStore::unfinished_run`](lash_core::store::RunStore::unfinished_run)
    /// of the case's session.
    UnfinishedRun,
    ReadSessionStateVersion,
    AdmitSessionState,
    LoadKnownNode,
    LoadUnknownNode,
    /// The session fault's whole life (ADR 0109 §9): recorded once, kept
    /// against a second recording, read alone and in the
    /// listing, then cleared once.
    SessionFault,
    TurnsChangedSince,
    /// [`SessionHistoryStore::load_committed_turns`](lash_core::store::SessionHistoryStore::load_committed_turns)
    /// of the case's session from its start (FIG-5297).
    LoadCommittedTurns,
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
    /// [`RunStore::run_terminal`](lash_core::store::RunStore::run_terminal)
    /// of the sweep's drain run: none while it is unfinished, and its lost
    /// end's evidence after it (FIG-3600 S7).
    RunTerminal,
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
    /// The retained submission digest, independent of input lifecycle state.
    TurnInputSubmissionDigest,
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
    /// [`ControlIntentStore::session_close_intent`](lash_core::store::ControlIntentStore::session_close_intent)
    /// of the case's session: none before its close, the kept intent after.
    SessionCloseIntent,
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
            Self::LoadSession => "surface:load_session",
            Self::ListPendingTurnInputs => "surface:list_pending_turn_inputs",
            Self::PendingTurnInput { known: true } => "surface:pending_turn_input",
            Self::PendingTurnInput { known: false } => "surface:pending_turn_input_unknown",
            Self::ListTurnInputApplications => "surface:list_turn_input_applications",
            Self::UnfinishedRun => "surface:unfinished_run",
            Self::ReadSessionStateVersion => "surface:read_session_state_version",
            Self::AdmitSessionState => "surface:admit_session_state",
            Self::LoadKnownNode => "surface:load_node_known",
            Self::LoadUnknownNode => "surface:load_node_unknown",
            Self::SessionFault => "surface:session_fault",
            Self::TurnsChangedSince => "surface:turns_changed_since",
            Self::LoadCommittedTurns => "surface:load_committed_turns",
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
            Self::RunTerminal => "surface:run_terminal",
            Self::EndRefusedRun => "surface:end_refused_run",
            Self::EndCommandRun => "surface:end_command_run",
            Self::RunBinding => "surface:run_binding",
            Self::RunOfInput => "surface:run_of_input",
            Self::BoundTurnScopes => "surface:bound_turn_scopes",
            Self::EnqueueRunSpecInput => "surface:enqueue_run_spec_input",
            Self::TurnInputSubmissionDigest => "surface:turn_input_submission_digest",
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
            Self::SessionCloseIntent => "surface:session_close_intent",
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
/// The instant the close case hands the ledger's store half.
const CLOSE_AT_MS: u64 = 5_000;

/// A backend-neutral summary of a control intent: its id and session are
/// compared as "the case's own", never by value.
fn control_intent_summary(
    intent: &lash_core::store::ControlIntent,
    session_id: &SessionId,
    own: Option<lash_core::store::ControlIntentId>,
) -> String {
    format!(
        "own={} own_session={} format={} kind={:?} state={:?} created_at_ms={}",
        Some(intent.id) == own,
        intent.session_id == *session_id,
        intent.format,
        intent.kind,
        intent.state,
        intent.created_at_ms,
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
            surface(SurfaceMethod::ReadSessionStateVersion),
            surface(SurfaceMethod::AdmitSessionState),
            surface(SurfaceMethod::LoadKnownNode),
            surface(SurfaceMethod::LoadUnknownNode),
            surface(SurfaceMethod::SessionFault),
            surface(SurfaceMethod::TurnsChangedSince),
            surface(SurfaceMethod::LoadCommittedTurns),
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
            surface(SurfaceMethod::UnfinishedRun),
            surface(SurfaceMethod::RunTerminal),
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
            surface(SurfaceMethod::TurnInputSubmissionDigest),
            surface(SurfaceMethod::EnqueueRunSpecInput),
            surface(SurfaceMethod::EnqueueRunSpecInput),
            surface(SurfaceMethod::TurnInputSubmissionDigest),
            // A batch resends the spec input and adds one; resending the
            // batch adds nothing; a conflicting batch is refused whole.
            surface(SurfaceMethod::EnqueueTurnInputBatch { conflicting: false }),
            surface(SurfaceMethod::EnqueueTurnInputBatch { conflicting: false }),
            surface(SurfaceMethod::EnqueueTurnInputBatch { conflicting: true }),
            // Admission of a batch resending the added input and adding one;
            // resending it admits nothing new.
            surface(SurfaceMethod::AdmitTurnInputBatch),
            surface(SurfaceMethod::AdmitTurnInputBatch),
            surface(SurfaceMethod::PendingTurnInput { known: true }),
            surface(SurfaceMethod::PendingTurnInput { known: false }),
            surface(SurfaceMethod::LoadRunSpec { known: true }),
            surface(SurfaceMethod::LoadRunSpec { known: false }),
            surface(SurfaceMethod::CancelUnknownPendingTurnInput),
            surface(SurfaceMethod::CancelPendingTurnInputSuffix),
            surface(SurfaceMethod::CancelPendingTurnInputs),
            surface(SurfaceMethod::TurnInputSubmissionDigest),
            surface(SurfaceMethod::AbortUnknownAttachmentWrite),
            surface(SurfaceMethod::AcquireUnknownAttachmentRefs),
            surface(SurfaceMethod::ForgetUnknownAttachment),
            surface(SurfaceMethod::ProbeAttachmentReferrers),
            surface(SurfaceMethod::ProbeSessionReferrerState),
            surface(SurfaceMethod::EndAttachmentReferrer),
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

/// A run whose execution met a typed refusal ends the same way on every SQL
/// backend, once: a second end writes nothing (FIG-4018).
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
            StoreOperation::EnqueueNextTurnInput,
            surface(SurfaceMethod::BindRunInputs { conflicting: false }),
            surface(SurfaceMethod::UnfinishedRun),
            surface(SurfaceMethod::EndRefusedRun),
            surface(SurfaceMethod::RunTerminal),
            surface(SurfaceMethod::UnfinishedRun),
            surface(SurfaceMethod::EndRefusedRun),
            surface(SurfaceMethod::RunTerminal),
            surface(SurfaceMethod::EndCommandRun),
            surface(SurfaceMethod::EndCommandRun),
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
            StoreOperation::DeleteSession,
            surface(SurfaceMethod::ReadSessionStateVersion),
            surface(SurfaceMethod::LoadUnknownNode),
            surface(SurfaceMethod::ListQueuedWork),
            surface(SurfaceMethod::ListPendingQueuedWork),
            surface(SurfaceMethod::PendingSessionWorkOrdering),
            surface(SurfaceMethod::EnqueueQueuedWorkWithOutcome),
            surface(SurfaceMethod::OpenSessionCommandRun),
            surface(SurfaceMethod::AdmitAtCheckpoint),
            surface(SurfaceMethod::CancelUnknownPendingTurnInput),
            surface(SurfaceMethod::UnfinishedRun),
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
/// S7): the store half ends the session's open run, an input's bound run,
/// `Cancelled` by the close, and a retry answers the kept intent. An unknown
/// session closes nothing, and an unknown intent reads as none.
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
            surface(SurfaceMethod::BindRunInputs { conflicting: false }),
            surface(SurfaceMethod::RunTerminal),
            surface(SurfaceMethod::BeginSessionClose {
                known_session: false,
            }),
            surface(SurfaceMethod::LoadIntent { known: false }),
            surface(SurfaceMethod::SessionCloseIntent),
            surface(SurfaceMethod::BeginSessionClose {
                known_session: true,
            }),
            // A retried store half answers the kept intent and writes nothing.
            surface(SurfaceMethod::BeginSessionClose {
                known_session: true,
            }),
            // The close ended the bound run by the session's deletion.
            surface(SurfaceMethod::RunTerminal),
            surface(SurfaceMethod::UnfinishedRun),
            surface(SurfaceMethod::LoadIntent { known: true }),
            surface(SurfaceMethod::SessionCloseIntent),
        ],
    }
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
        let answer = match method {
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
            SurfaceMethod::UnfinishedRun => match store.unfinished_run(&session_id).await? {
                None => "unfinished=none".to_string(),
                Some(unfinished) => format!(
                    "unfinished_run={} head={}",
                    unfinished.run,
                    match unfinished.head {
                        lash_core::store::AdmittedHead::Input(_) => "input",
                        lash_core::store::AdmittedHead::Batch(_) => "batch",
                    }
                ),
            },
            SurfaceMethod::ReadSessionStateVersion => {
                format!(
                    "version={}",
                    store.read_session_state_version(&session_id).await?
                )
            }
            SurfaceMethod::AdmitSessionState => {
                let admission = store.admit_session_state(&session_id).await?;
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
                    "first={first:?} kept_first={} read={} listed={} cleared={cleared} \
                     again={again} after_present={}",
                    kept == first,
                    read == first,
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
            SurfaceMethod::LoadCommittedTurns => {
                let page = store
                    .load_committed_turns(&session_id, None, std::num::NonZeroU32::MAX)
                    .await?;
                let turns: Vec<_> = page
                    .turns
                    .iter()
                    .map(|turn| format!("{}:{:?}:{}", turn.turn_id, turn.outcome, turn.nodes.len()))
                    .collect();
                format!(
                    "turns={turns:?} next_revision={}",
                    page.next.head_revision()
                )
            }
            SurfaceMethod::ListQueuedWork => {
                format!("rows={}", store.list_queued_work(&session_id).await?.len())
            }
            SurfaceMethod::ListPendingQueuedWork => {
                let open = store.list_open_queued_work(&session_id).await?;
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
                let command_run = store.open_session_command_run(&session_id).await?;
                format!("commands={}", command_run.len())
            }
            SurfaceMethod::AdmitAtCheckpoint => {
                let admission = store
                    .admit_at_checkpoint(&lash_core::store::CheckpointAdmissionRequest {
                        session_id: session_id.clone(),
                        run: lash_core::TurnId::from(SURFACE_CHECKPOINT_RUN_ID),
                        turn_id: lash_core::TurnId::from("fig-2841-surface-turn"),
                        checkpoint: lash_core::CheckpointKind::AfterWork,
                        step: "fig-2841-surface-checkpoint".to_string(),
                        max_inputs: 1,
                        policy: lash_core::testing::queued_work_admission_policy(1),
                    })
                    .await?;
                format!("inputs={}", admission.inputs.is_some())
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
            SurfaceMethod::RunTerminal => {
                // The kind and cause are caller-supplied facts; the instant
                // is the backend clock's and is not compared.
                let run = lash_core::TurnId::from(SURFACE_RUN_ID);
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
            SurfaceMethod::EndRefusedRun => {
                let run = lash_core::TurnId::from(SURFACE_RUN_ID);
                let refusal = lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::StoreCommitSuperseded,
                    "the head moved under the run's commit",
                );
                match store
                    .end_refused_run(&session_id, &run, &refusal, 1)
                    .await?
                {
                    lash_core::store::RunEndOutcome::Ended(terminal) => {
                        format!("ended={:?}", terminal.kind())
                    }
                    lash_core::store::RunEndOutcome::AlreadyEnded(terminal) => {
                        format!("already_ended={:?}", terminal.kind())
                    }
                    lash_core::store::RunEndOutcome::Unknown => "ended=none".to_string(),
                }
            }
            SurfaceMethod::EndCommandRun => {
                let run =
                    lash_core::TurnId::fixture(format!("shift-commands:{session_id}-surface"));
                let end = match store.end_command_run(&session_id, &run, 1).await? {
                    lash_core::store::RunEndOutcome::Ended(terminal) => {
                        format!("ended={:?}/{:?}", terminal.kind(), terminal.cause)
                    }
                    lash_core::store::RunEndOutcome::AlreadyEnded(terminal) => {
                        format!("already_ended={:?}/{:?}", terminal.kind(), terminal.cause)
                    }
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
                    Some(run) => format!("bound={run}"),
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
            SurfaceMethod::TurnInputSubmissionDigest => {
                let digest = store
                    .turn_input_submission_digest(&session_id, "surface:run-spec-input")
                    .await?;
                format!("submission_digest={digest:?}")
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
                    .admit_pending_turn_inputs(lash_core::PendingTurnInputBatch::new(
                        session_id.clone(),
                        vec![
                            draft("surface:batch-input", "input added by a batch"),
                            draft("surface:admitted-input", "input added by an admission"),
                        ],
                    )?)
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
            SurfaceMethod::SessionCloseIntent => {
                match self.factory().session_close_intent(&session_id).await? {
                    Some(intent) => {
                        control_intent_summary(&intent, &session_id, self.surface.close_intent)
                    }
                    None => "intent=none".to_string(),
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
