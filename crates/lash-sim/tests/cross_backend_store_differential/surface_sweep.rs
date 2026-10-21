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
//! what a read *found* is. Model-call answers preserve complete typed facts
//! and reservations, whose identities the fixture supplies. The durable contract is carried by the
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
    pub(super) model_ledger: model_call_coverage::ModelLedger,
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
    LinkedPackageStore,
    ExecutionResolutions,
    ProcessAuthority,
    EnvironmentBytes,
    LoadSession,
    RunFactsOf,
    DurableRead(durable_read_coverage::DurableReadMethod),
    PrepareModelCalls,
    ModelCalls,
    ModelCallPins,
    ClaimFacts,
    /// [`DurableStore::readiness`](lash_core::durable_port::DurableStore::readiness):
    /// whether a claim could find work, and whether a due time is ahead.
    Readiness,
    /// [`DurableStore::actor_requirements`](lash_core::durable_port::DurableStore::actor_requirements)
    /// paged whole, read for the case's session actor: the stored claim
    /// requirements the strand report reads (FIG-6993).
    ActorRequirements,
    /// Test authority refuses an absent actor without changing durable state.
    AcquireIdleForTesting,
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
    /// [`RunStore::follow_facts`](lash_core::store::RunStore::follow_facts)
    /// of the sweep's next-turn input and of the sweep's run: the binding,
    /// open row, withdrawal and session fault, and the run's terminal,
    /// operation and park facts, each in one read (FIG-7045).
    FollowFacts,
    /// [`RunStore::bound_turn_scopes`](lash_core::store::RunStore::bound_turn_scopes)
    /// of the sweep's run: the turn scopes its bound inputs name.
    BoundTurnScopes,
    /// [`IngressStore::enqueue_pending_turn_input`] of a keyed next-turn
    /// input under a non-default run spec, which interns the spec (FIG-3838).
    EnqueueRunSpecInput,
    /// The retained submission digest, independent of input lifecycle state.
    TurnInputSubmissionDigest,
    /// [`IngressStore::forsake_turn_input`] of the spec input's key, which an
    /// input holds (`held`), or of a key nothing holds, which writes its
    /// tombstone and keeps it when forsaken again.
    ForsakeTurnInput {
        held: bool,
    },
    /// [`IngressStore::enqueue_pending_turn_input`] under the forsaken key,
    /// which every backend refuses without residue.
    EnqueueForsakenInput,
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
            Self::LinkedPackageStore => "surface:linked_package_store",
            Self::ExecutionResolutions => "surface:execution_resolutions",
            Self::ProcessAuthority => "surface:process_authority",
            Self::EnvironmentBytes => "surface:environment_bytes",
            Self::LoadSession => "surface:load_session",
            Self::RunFactsOf => "surface:run_facts_of",
            Self::DurableRead(method) => method.name(),
            Self::PrepareModelCalls => "surface:prepare_model_calls",
            Self::ModelCalls => "surface:model_calls",
            Self::ModelCallPins => "surface:model_call_pins",
            Self::ClaimFacts => "surface:claim_facts",
            Self::Readiness => "surface:readiness",
            Self::ActorRequirements => "surface:actor_requirements",
            Self::AcquireIdleForTesting => "surface:acquire_idle_for_testing",
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
            Self::FollowFacts => "surface:follow_facts",
            Self::BoundTurnScopes => "surface:bound_turn_scopes",
            Self::EnqueueRunSpecInput => "surface:enqueue_run_spec_input",
            Self::TurnInputSubmissionDigest => "surface:turn_input_submission_digest",
            Self::ForsakeTurnInput { held: true } => "surface:forsake_turn_input_held",
            Self::ForsakeTurnInput { held: false } => "surface:forsake_turn_input",
            Self::EnqueueForsakenInput => "surface:enqueue_forsaken_input",
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

/// An attachment id no case ever writes. The attachment-referrer drivers use
/// it so the inventory covers those methods without mutating an attachment the
/// surrounding case depends on: an unknown entity is itself a refusal driver,
/// and whatever a backend answers, the no-residue law still applies.
#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn unknown_attachment_id() -> AttachmentId {
    AttachmentId::parse("c3".repeat(32)).expect("the unknown-attachment id must parse")
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
const UNKNOWN_INPUT_ID: &str = "fig-2841-unknown-input";
/// The run the sweep binds its next-turn input to, and the other run a
/// second binding names.
pub(super) const SURFACE_RUN_ID: &str = "fig-3600-run";
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
            surface(SurfaceMethod::LinkedPackageStore),
            surface(SurfaceMethod::ExecutionResolutions),
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
            surface(SurfaceMethod::ProcessAuthority),
            surface(SurfaceMethod::EnvironmentBytes),
            surface(SurfaceMethod::RunFactsOf),
            surface(SurfaceMethod::PrepareModelCalls),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::Turn,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::TurnNamespaces,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::TurnEnd,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::RunFacts,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::RunFactsOf,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::RunFactsWithin,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::ExecRoot,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::CellRoots,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::CellRootsWithin,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::PendingWaits,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::Wait,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::Process,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::LiveUntilDescendants,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::UntilChildren,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::SessionClose,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::EndingScopes,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::SessionMailbox,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::PromptSnapshot,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::PromptTexts,
            )),
            surface(SurfaceMethod::DurableRead(
                durable_read_coverage::DurableReadMethod::Hydrate,
            )),
            surface(SurfaceMethod::ModelCalls),
            surface(SurfaceMethod::ModelCallPins),
            surface(SurfaceMethod::ClaimFacts),
            surface(SurfaceMethod::Readiness),
            surface(SurfaceMethod::ActorRequirements),
            surface(SurfaceMethod::AcquireIdleForTesting),
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
            surface(SurfaceMethod::FollowFacts),
            surface(SurfaceMethod::BoundTurnScopes),
            surface(SurfaceMethod::BindRunInputs { conflicting: false }),
            surface(SurfaceMethod::RunBinding),
            surface(SurfaceMethod::RunOfInput),
            surface(SurfaceMethod::FollowFacts),
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
            // A key nothing holds is forsaken, idempotently, and then refuses
            // its send; a key an input holds answers that input.
            surface(SurfaceMethod::ForsakeTurnInput { held: false }),
            surface(SurfaceMethod::ForsakeTurnInput { held: false }),
            surface(SurfaceMethod::ForsakeTurnInput { held: true }),
            surface(SurfaceMethod::EnqueueForsakenInput),
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
            SurfaceMethod::ExecutionResolutions => {
                let stores = self.lifecycle_backend.stores();
                lash_durable::laws::a_resolution_winner_and_its_claim_debt_commit_atomically(
                    stores.durable_store().as_ref(),
                    stores.execution_resolutions().as_ref(),
                )
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
                let resolutions = stores.execution_resolutions();
                assert_eq!(
                    resolutions
                        .requirements(
                            &lash_core::durable_port::ActorKey::process("resolution-owner")
                                .map_err(|error| StoreError::Backend(error.to_string()))?
                        )
                        .await
                        .map_err(|error| StoreError::Backend(error.to_string()))?,
                    lash_durable::packages::vocabulary::claims::ClaimRequirements::default()
                );
                "one immutable winner and atomic claim debt".to_owned()
            }
            SurfaceMethod::LinkedPackageStore => {
                use lash_core_store::artifact_referrer::{
                    ArtifactReferrer, HostArtifactPin, ReferrerClaim, ResolvedArtifactCleanup,
                };
                use lash_core_store::packages::{LinkRecord, PackageRecord};
                use lash_durable::packages::vocabulary::{names::*, package::*};
                let stores = self.lifecycle_backend.stores();
                let packages = stores.package_store();
                let referrer = ArtifactReferrer::HostPin(HostArtifactPin::mint());
                let claim = ReferrerClaim::unguarded(referrer.clone())
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                let modules = stores.module_artifacts();
                let document = DocumentId::from_bytes([73; 32]);
                modules
                    .publish_module_artifact(
                        &claim,
                        &document.to_string(),
                        b"package surface document",
                    )
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                let link = Link {
                    document,
                    fills: Default::default(),
                };
                let link = LinkRecord {
                    id: link.identity(),
                    link,
                };
                let package = Package {
                    path: "surface"
                        .parse()
                        .map_err(|error| StoreError::Backend(format!("{error}")))?,
                    members: [(
                        "run"
                            .parse()
                            .map_err(|error| StoreError::Backend(format!("{error}")))?,
                        Member {
                            visibility: Visibility::Public,
                            kind: MemberKind::Export {
                                link: link.id,
                                export: ExportName {
                                    name: Name::new("run"),
                                    member: None,
                                },
                            },
                        },
                    )]
                    .into(),
                };
                let package = PackageRecord {
                    id: package.identity(),
                    package,
                };
                let publication = PublicationRecord {
                    context: PublicationContext {
                        domain: PublicationDomain("surface".into()),
                        scope: PublicationScope::Host,
                        publisher: PublisherInstance("surface".into()),
                        root: "surface"
                            .parse()
                            .map_err(|error| StoreError::Backend(format!("{error}")))?,
                    },
                    path: package.package.path.clone(),
                    content: PublishedContent::Package(package.id),
                    code_links: [link.id].into(),
                    imported_publications: Vec::new(),
                };
                packages
                    .put_link(&claim, &link, &[])
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                packages
                    .put_package(&claim, &package)
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                packages
                    .put_labels(&claim, &package.id, "{}")
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                assert_eq!(
                    packages
                        .labels(&package.id)
                        .await
                        .map_err(|error| StoreError::Backend(error.to_string()))?,
                    vec!["{}".to_owned()]
                );
                let key = packages
                    .publish(&claim, &publication)
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                packages
                    .acquire_publication(&claim, &key)
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                assert_eq!(
                    packages
                        .link(&link.id)
                        .await
                        .map_err(|error| StoreError::Backend(error.to_string()))?,
                    Some(link.clone())
                );
                assert_eq!(
                    packages
                        .package(&package.id)
                        .await
                        .map_err(|error| StoreError::Backend(error.to_string()))?,
                    Some(package)
                );
                assert_eq!(
                    packages
                        .publication(&key)
                        .await
                        .map_err(|error| StoreError::Backend(error.to_string()))?,
                    Some(publication)
                );
                packages
                    .end_package_referrer(&ResolvedArtifactCleanup {
                        referrer,
                        carries: Vec::new(),
                    })
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                assert!(
                    packages
                        .link(&link.id)
                        .await
                        .map_err(|error| StoreError::Backend(error.to_string()))?
                        .is_none()
                );
                "publication acquired and reclaimed with its closure".to_owned()
            }
            SurfaceMethod::ProcessAuthority => {
                let record = self
                    .lifecycle_backend
                    .process_authority_reads()
                    .process_authority(&lash_core::ProcessId::fixture("surface:unknown-process"))
                    .await?;
                format!("present={}", record.is_some())
            }
            SurfaceMethod::EnvironmentBytes => {
                let env_ref = lash_execution_contracts::testing::process_env_with_limit(999)
                    .stable_ref()
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                let bytes = self
                    .lifecycle_backend
                    .process_authority_reads()
                    .environment_bytes(&env_ref)
                    .await?;
                format!("present={}", bytes.is_some())
            }
            SurfaceMethod::AcquireIdleForTesting => {
                // The test-only authority refuses a missing actor without residue.
                let store = self.lifecycle_backend.durable();
                let node = store
                    .register_node(&lash_core::durable_port::NodeSpec {
                        node: lash_core::durable_port::NodeId::new(format!(
                            "surface-test-writer-{session_id}"
                        )),
                        decodes: Vec::new(),
                        serves: Vec::new(),
                        capabilities: Default::default(),
                        ttl_millis: 60_000,
                    })
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                let absent = lash_core::durable_port::ActorKey::session("surface-test-absent")
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                let refused = store
                    .acquire_idle_for_testing(&node, &absent, lash_core::durable_port::Epoch(0))
                    .await;
                store
                    .release_node(&node)
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                assert!(matches!(
                    refused,
                    Err(lash_core::durable_port::DurableError::MailRefused(
                        lash_core::durable_port::MailRefusal::UnknownActor(_)
                    ))
                ));
                "unknown_actor".to_owned()
            }
            SurfaceMethod::ClaimFacts => {
                let actor = lash_core::durable_port::ActorKey::session(session_id.as_str())
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                let facts = self
                    .lifecycle_backend
                    .durable()
                    .claim_facts(None, &actor)
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                let roots = self
                    .lifecycle_backend
                    .durable()
                    .cell_roots_within(&session_id, &SURFACE_RUN_ID.into(), 4096)
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                format!(
                    "node={} actor={} roots={}",
                    facts.node.is_some(),
                    facts.actor.is_some(),
                    roots.len()
                )
            }
            SurfaceMethod::Readiness => {
                let readiness = self
                    .lifecycle_backend
                    .durable()
                    .readiness()
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                // Store time differs between backends; what a claim could
                // find must not.
                format!(
                    "due={} next_due={}",
                    readiness.due,
                    readiness.next_due.is_some()
                )
            }
            SurfaceMethod::ActorRequirements => {
                let actor = lash_core::durable_port::ActorKey::session(session_id.as_str())
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                // Other cases share the PostgreSQL database: page every actor
                // that carries requirements and keep the case's own.
                let mut after = None;
                let mut found = None;
                loop {
                    let page = self
                        .lifecycle_backend
                        .durable()
                        .actor_requirements(after.as_ref(), 256)
                        .await
                        .map_err(|error| StoreError::Backend(error.to_string()))?;
                    let Some((last, _)) = page.last().cloned() else {
                        break;
                    };
                    if let Some((_, requires)) = page.into_iter().find(|(key, _)| *key == actor) {
                        found = Some(requires);
                    }
                    after = Some(last);
                }
                format!("requires={found:?}")
            }
            SurfaceMethod::DurableRead(method) => {
                durable_read_coverage::drive(
                    self.lifecycle_backend.durable().as_ref(),
                    &session_id,
                    method,
                    &mut self.surface.model_ledger.visited,
                )
                .await
            }
            SurfaceMethod::PrepareModelCalls
            | SurfaceMethod::ModelCalls
            | SurfaceMethod::ModelCallPins => {
                self.surface
                    .model_ledger
                    .drive(
                        self.lifecycle_backend.durable().as_ref(),
                        &session_id,
                        method,
                    )
                    .await
            }
            SurfaceMethod::RunFactsOf => {
                let owner = lash_core::durable_port::domain::OwnerKey::Turn(
                    session_id.clone(),
                    SURFACE_RUN_ID.into(),
                );
                let facts = self
                    .lifecycle_backend
                    .durable()
                    .run_facts_of(&owner, lash_core::durable_port::domain::RunSeq(1))
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                let bounded = self.lifecycle_backend.durable().run_facts_within(&owner, Some(lash_core::durable_port::domain::RunSeq(1)), 4096, lash_core::durable_port::domain::checkpoint_graph::DecodeLimits::within_total(16 * 1024 * 1024)).await
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                assert_eq!(bounded.rows, facts.rows);
                format!("rows={} nodes={}", facts.rows.len(), facts.nodes.len())
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
            SurfaceMethod::FollowFacts => {
                use lash_core::store::{FollowFacts, FollowSubject, RunFollowFacts};
                let run_facts = |facts: Option<RunFollowFacts>| {
                    facts.map(|facts| {
                        format!(
                            "{} terminal={:?} operation_pending={:?} park={:?}",
                            facts.run,
                            facts.terminal.map(|terminal| terminal.kind()),
                            facts.operation_pending,
                            facts.park
                        )
                    })
                };
                let input = lash_core::InputId::fixture(format!("{session_id}:input"));
                let FollowFacts::Input(input) = store
                    .follow_facts(&session_id, &FollowSubject::Input(input))
                    .await?
                else {
                    return Err(lash_core::StoreError::Backend(
                        "an input subject answered run facts".to_string(),
                    ));
                };
                let FollowFacts::Run(run) = store
                    .follow_facts(
                        &session_id,
                        &FollowSubject::Run(lash_core::TurnId::from(SURFACE_RUN_ID)),
                    )
                    .await?
                else {
                    return Err(lash_core::StoreError::Backend(
                        "a run subject answered input facts".to_string(),
                    ));
                };
                format!(
                    "run={:?} open={} fault={:?} forsaken={} withdrawn={} followed={:?}",
                    run_facts(input.run),
                    input.open,
                    input.fault.map(|fault| fault.record.code),
                    input.forsaken,
                    input.withdrawn,
                    run_facts(Some(run))
                )
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
            SurfaceMethod::ForsakeTurnInput { held } => {
                let key = if held {
                    "surface:run-spec-input"
                } else {
                    "surface:forsaken-input"
                };
                match store.forsake_turn_input(&session_id, key).await? {
                    lash_core::store::TurnInputForsaking::Held { .. } => "held".to_string(),
                    lash_core::store::TurnInputForsaking::Forsaken { input_id } => format!(
                        "forsaken input_is_keyed={}",
                        input_id
                            == PendingTurnInputDraft::keyed_input_id(
                                &session_id,
                                "surface:forsaken-input"
                            )
                    ),
                }
            }
            SurfaceMethod::EnqueueForsakenInput => {
                store
                    .enqueue_pending_turn_input(
                        PendingTurnInputDraft::new(
                            &session_id,
                            TurnInputIngress::NextTurn,
                            TurnInput::text("input under a forsaken key"),
                        )
                        .with_source_key("surface:forsaken-input"),
                    )
                    .await?;
                "accepted".to_string()
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
