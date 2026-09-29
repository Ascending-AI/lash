//! The session drive: every turn runs through one drive body whose admission
//! is a recorded step (FIG-3600, ADR 0104 O1/O2/O6, ADR 0105 §2).
//!
//! A drive of one session loops: a recorded `AdmitDrive` step decides what
//! runs next (an unfinished root it resumes, a follow-on the head owes, or
//! the queue prefix it takes, minting the root), a recorded `SealDriveAdmission` step raises the
//! session's drive epoch for that admission, and the root's turns run to
//! their terminal commit. It stops when admission answers anything but an
//! admitted root. What the drive decides is recorded: a redrive replays the
//! admission and the seal, and the root replays its recorded claim and the
//! head that claim was admitted on (FIG-3682).
//!
//! The root's claim body repairs orphaned inputs before it claims the admitted
//! head. A separate `InspectAdmittedHead` step records whether that head is
//! ready, ceded, or divergent. A redrive reads both outcomes from its journal
//! and issues no second repair. It revalidates a `Ready` verdict under its
//! current drive epoch before any turn effect: a root whose claim lost
//! authority while the handler was down can only cede or park. This check
//! cannot select new work or change the claim's recorded base (ADR 0105 §2).
//! Rule 6 of the substrate lint pins direct store calls and the orphan-repair
//! helper in the drive.
//!
//! The drive epoch fences admission and repairs claims left by older epochs.
//! Commit CAS still protects the session head from stale writes.
//!
//! The drive names no engine. An engine that runs drives in process hands
//! [`drive_session`] one controller, and the drive rescopes it per step
//! ([`drive_admission_scope`], [`drive_root_scope`]). An engine that splits
//! the drive over its own handlers calls [`admit_drive`] from its
//! per-session handler and [`run_admitted_root`] from its per-root handler.
//!
//! [`drive_admission_scope`]: crate::engine::drive_admission_scope
//! [`drive_root_scope`]: crate::engine::drive_root_scope

mod admission;
#[cfg(test)]
mod attempt_drop_tests;
mod close;
mod control;
pub mod ingress;
mod parent_end_relay;
mod park;
mod reconcile;
pub mod relay;
mod relays;
mod root;
pub mod scope_close;
mod turn_config;

pub use control::{ControlIntentRelay, intent_drive_request};
pub use ingress::{FIRST_INGRESS_ATTEMPT, IngressRelay, ingress_drive_request};
pub use parent_end_relay::ParentEndRelay;
pub use park::StoreParkRecovery;
pub use reconcile::{
    DrainHandOverPass, ReconcileParts, ReconcileProcesses, drain_hand_over_slot, reconcile_once,
};
pub use relays::{
    ObligationRelayUnavailable, RelayNeed, RelayParts, RelaySupply, obligation_relays,
};
pub use scope_close::{ScopeCloseRelay, deliver_scope_close};
pub(crate) use turn_config::provider_binding_unavailable;
pub use turn_config::{validate_route, validate_route_with};

use std::sync::Arc;

use crate::engine::{
    AdmitRequest, AdmitVerdict, Admitted, DriveAbort, DriveLoop, DriveOutcome, DriveRequest,
    DriveStop, RootOutcome, drive_admission_replay_key, drive_admission_scope, drive_root_scope,
    drive_root_start_replay_key, drive_seal_replay_key,
};
use crate::runtime::LashRuntime;
use crate::{
    AgentFrameRun, EffectAddress, EventSink, LocalTurnStop, RuntimeAttribution,
    RuntimeEffectCommand, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectInvocation, RuntimeEffectLocalExecutor, RuntimeError, RuntimeErrorCode,
    ScopedEffectController, TurnActivitySink, TurnId,
};

/// Where the turns of a drive publish, and the host-local stop they honour.
///
/// An engine-run drive publishes to its session's observation sinks; the
/// facade's in-process entries publish to the caller's too. Nothing a turn
/// publishes decides anything.
#[derive(Clone)]
pub struct DriveSinks<'a> {
    pub events: &'a dyn EventSink,
    pub turn_events: &'a dyn TurnActivitySink,
    pub local_stop: LocalTurnStop,
    /// Where each root this drive commits hands its report over.
    pub settled: &'a dyn RootSettledSink,
}

impl Default for DriveSinks<'_> {
    fn default() -> Self {
        Self {
            events: &crate::runtime::NOOP_EVENT_SINK,
            turn_events: &crate::runtime::NOOP_TURN_ACTIVITY_SINK,
            local_stop: LocalTurnStop::default(),
            settled: &NoopRootSettledSink,
        }
    }
}

/// A root this runtime ran to its final commit: its final physical turn as
/// it ran, and the accepted inputs its admission drove.
pub struct SettledRoot<'r> {
    pub root: &'r TurnId,
    pub turn: &'r crate::AssembledTurn,
    pub driven_inputs: &'r [crate::InputId],
}

/// Where a drive hands a committed root's report over (FIG-3979): once its
/// final commit and that commit's `TurnPersisted` delivery are done, before
/// the root's recorded scope close. The runtime passed is the one that ran
/// the root, holding its commit. Nothing a sink does decides anything, and
/// the drive waits for it before the close, so a sink returns at once.
#[async_trait::async_trait]
pub trait RootSettledSink: Send + Sync {
    async fn settled(&self, runtime: &LashRuntime, root: SettledRoot<'_>);
}

/// A sink that takes no report.
pub struct NoopRootSettledSink;

#[async_trait::async_trait]
impl RootSettledSink for NoopRootSettledSink {
    async fn settled(&self, _runtime: &LashRuntime, _root: SettledRoot<'_>) {}
}

/// The admitted root a runtime is running, and the fence its seal raised
/// (FIG-3600 S7, ADR 0105 §2).
///
/// Every commit of one of the root's physical turns presents the fence, so a
/// successor's seal refuses it; the commit of its final physical turn writes
/// the root's terminal evidence in its own transaction.
#[derive(Clone, Debug)]
pub(crate) struct DriveRootRun {
    /// The logical root the evidence names. A follow-on recovery's root is
    /// the recovery's; its evidence names the root that owed the follow-on.
    root: TurnId,
    pub(crate) fence: crate::store::DriveFence,
    /// The drain generation stamped on the journal the root runs on: the
    /// admitting drive's request stamp (FIG-3795 S9). A park this run writes
    /// records it, so the drain routes the root's resume to the build its
    /// journal belongs to.
    journal_generation: crate::engine::BuildGeneration,
    /// Whether the root's terminal evidence is durable: a commit of this run
    /// wrote it, or answered the receipt of the commit that did. It gates the
    /// recorded scope-close step, so it is a durable fact every execution of
    /// the root reads alike, never whether this execution was the writer
    /// (FIG-3893).
    terminal_written: bool,
}

impl DriveRootRun {
    fn sealed(admitted: &Admitted, fence: crate::store::DriveFence) -> Self {
        Self {
            root: evidence_root(admitted),
            fence,
            journal_generation: admitted.admitted_generation().clone(),
            terminal_written: false,
        }
    }

    /// What the commit of physical turn `turn` presents: the fence, the root
    /// whose admitted rows it settles, and the root's terminal evidence when
    /// the turn ends the root. `None` for a turn that is not one of the
    /// root's physical turns.
    ///
    /// A turn ends its root when it finishes or stops and leaves nothing
    /// owed: no follow-on on the head, and no withheld work a follow-on turn
    /// drives (`owes_follow_on`).
    pub(crate) fn commit_facts(
        &self,
        turn: &TurnId,
        outcome: &crate::TurnOutcome,
        owes_follow_on: bool,
    ) -> Option<DriveCommit> {
        let commit = crate::store::TurnCommitId::of_physical_turn(&self.root, turn)?;
        let (stop, terminal) = match outcome {
            crate::TurnOutcome::Finished(_) => (None, true),
            crate::TurnOutcome::Stopped(stop) => (Some(stop.clone()), true),
            crate::TurnOutcome::AgentFrameSwitch { .. } => (None, false),
        };
        Some(DriveCommit {
            fence: self.fence.clone(),
            root: self.root.clone(),
            terminal: (terminal && !owes_follow_on).then(|| crate::store::RootTerminalWrite {
                root: self.root.clone(),
                commit,
                turn: turn.clone(),
                stop,
            }),
        })
    }

    /// The logical root this run's evidence and park name.
    pub(crate) fn root(&self) -> &TurnId {
        &self.root
    }

    /// The drain generation stamped on the journal this run's root runs on.
    pub(crate) fn journal_generation(&self) -> &crate::engine::BuildGeneration {
        &self.journal_generation
    }

    pub(crate) fn mark_terminal_written(&mut self) {
        self.terminal_written = true;
    }
}

/// The logical root whose terminal evidence `admitted`'s run writes: the
/// admitted root, except for a follow-on recovery, whose evidence names the
/// root that owed the follow-on.
fn evidence_root(admitted: &Admitted) -> TurnId {
    match admitted.work() {
        crate::engine::AdmittedWork::FollowOn { follow_on, .. } => {
            crate::store::PhysicalTurn::split_turn_id(follow_on).0
        }
        crate::engine::AdmittedWork::Input { .. }
        | crate::engine::AdmittedWork::Queued { .. }
        | crate::engine::AdmittedWork::Commands { .. } => admitted.root().clone(),
    }
}

/// What a root's physical-turn commit presents (FIG-3600 S7, FIG-3927).
#[derive(Clone, Debug)]
pub(crate) struct DriveCommit {
    /// The fence of the drive admission the root runs under.
    pub(crate) fence: crate::store::DriveFence,
    /// The root whose admitted rows the commit settles.
    pub(crate) root: TurnId,
    /// The root's terminal evidence, when the turn ends the root.
    pub(crate) terminal: Option<crate::store::RootTerminalWrite>,
}

/// One admitted root's run, with the physical turns it assembled.
pub(crate) struct RootRun {
    pub(crate) outcome: RootOutcome,
    /// The root's physical turns, when its turns ran in this process.
    pub(crate) run: Option<AgentFrameRun>,
    /// The accepted inputs the root's recorded admission drove.
    pub(crate) driven_inputs: Vec<crate::InputId>,
    /// A follow-on recovery root that ran no turn here: why its drain ran
    /// nothing.
    pub(crate) empty_drain: Option<crate::runtime::turn_loop::EmptyQueuedDrainReason>,
}

/// Whether a drive recovers the follow-on the session head owes when
/// admission names it (ADR 0101 §3, FIG-3542).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FollowOnRecovery {
    /// Run the recovery root: every drive an engine runs, and a queued
    /// drain.
    Recover,
    /// Stop before it: a direct turn's own drive, whose input waits behind
    /// the follow-on and is answered queued. The session's drive recovers
    /// the follow-on and then answers the input.
    Decline,
}

/// The bounds a drive loop runs under: whether it recovers the follow-on
/// the session head owes, and how many roots one invocation runs before it
/// yields to a continuation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DriveLimits {
    pub(crate) follow_on: FollowOnRecovery,
    pub(crate) max_roots: Option<usize>,
}

/// How a drive loop ended, with the roots it ran.
pub(crate) struct DriveRun {
    pub(crate) outcome: DriveOutcome,
    pub(crate) runs: Vec<RootRun>,
    /// The drive stopped at an admitted follow-on recovery it declined.
    pub(crate) declined_follow_on: bool,
    pub(crate) budget_exhausted: bool,
}

/// Drive `request` on `runtime`'s session to a stop: admit, seal and run
/// roots until admission answers something other than an admitted root.
///
/// `controller` is rescoped for every step: admission ordinal `n` runs under
/// [`drive_admission_scope`](crate::engine::drive_admission_scope) at
/// [`drive_admission_replay_key`](crate::engine::drive_admission_replay_key),
/// and each admitted root under
/// [`drive_root_scope`](crate::engine::drive_root_scope).
pub async fn drive_session(
    runtime: &mut LashRuntime,
    controller: &ScopedEffectController<'_>,
    request: &DriveRequest,
) -> Result<DriveOutcome, DriveAbort> {
    drive_session_with(runtime, controller, request, DriveSinks::default()).await
}

/// [`drive_session`], publishing every turn to `sinks`.
#[doc(hidden)]
pub async fn drive_session_with(
    runtime: &mut LashRuntime,
    controller: &ScopedEffectController<'_>,
    request: &DriveRequest,
    sinks: DriveSinks<'_>,
) -> Result<DriveOutcome, DriveAbort> {
    let run = Box::pin(runtime.drive_until(
        controller,
        request,
        &sinks,
        None,
        DriveLimits {
            follow_on: FollowOnRecovery::Recover,
            max_roots: Some(crate::engine::MAX_ROOTS_PER_DRIVE),
        },
        |_| false,
    ))
    .await?;
    if run.budget_exhausted {
        runtime.host.queued_work().schedule_drive(
            &request.session,
            crate::engine::drive_continuation_request(request),
        );
    }
    Ok(run.outcome)
}

/// Admission `ordinal` of `request`: one recorded `AdmitDrive` step through
/// `controller`, which must serve
/// [`drive_admission_scope`](crate::engine::drive_admission_scope) for the
/// request.
pub async fn admit_drive(
    runtime: &mut LashRuntime,
    controller: &ScopedEffectController<'_>,
    request: &DriveRequest,
    ordinal: u32,
) -> Result<AdmitVerdict, DriveAbort> {
    Box::pin(runtime.admit_drive_step(controller, request, ordinal)).await
}

/// Emit admission `ordinal`'s journaled `AdmitDrive` step through
/// `controller`, which must serve the request's
/// [`drive_admission_scope`](crate::engine::drive_admission_scope). `store`
/// is the session's history store, or `None` when the session could not be
/// opened at all — deleted, or closed past admission — in which case the
/// step's recorded body is the retirement itself (FIG-3630).
async fn emit_admission_step(
    controller: &ScopedEffectController<'_>,
    request: &DriveRequest,
    ordinal: u32,
    store: Option<Arc<dyn crate::store::RuntimePersistence>>,
    stores: Arc<dyn crate::SessionStoreFactory>,
) -> Result<AdmitVerdict, DriveAbort> {
    let scope = drive_admission_scope(&request.session, &request.request);
    let invocation = RuntimeEffectInvocation::new(
        EffectAddress::new(
            scope.scope().clone(),
            drive_admission_replay_key(&request.request, ordinal),
        )
        .map_err(|error| DriveAbort::Refused(RuntimeError::from(error)))?,
        RuntimeAttribution::for_session(request.session.clone()),
        format!("drive-admission-{ordinal}"),
    );
    let admit_request = AdmitRequest {
        session: request.session.clone(),
        request: request.request.clone(),
        build_generation: request.build_generation.clone(),
    };
    controller
        .execute_effect(
            RuntimeEffectEnvelope::new(
                invocation,
                RuntimeEffectCommand::AdmitDrive {
                    request: Box::new(admit_request.clone()),
                },
            ),
            RuntimeEffectLocalExecutor::owned_runner(
                Box::new(admission::AdmitDriveRunner {
                    store,
                    stores,
                    request: admit_request,
                    ordinal,
                }),
                None,
            ),
        )
        .await
        .and_then(crate::RuntimeEffectOutcome::into_admit_drive)
        .map_err(|error| controller_abort(None, error))
}

/// Admission `ordinal` of `request` for a session whose store could not be
/// opened: its close or tombstone already committed. The journaled
/// `AdmitDrive` step still has to be emitted — an attempt that stopped
/// short of it diverges from the `run` command an earlier attempt journaled
/// at this position (ADR 0104 O1) — and its recorded body answers the
/// session's retirement, which every redrive of the invocation decodes.
/// `controller` serves the request's
/// [`drive_admission_scope`](crate::engine::drive_admission_scope).
#[doc(hidden)]
pub async fn admit_drive_retired(
    controller: &ScopedEffectController<'_>,
    request: &DriveRequest,
    ordinal: u32,
    stores: Arc<dyn crate::SessionStoreFactory>,
) -> Result<AdmitVerdict, DriveAbort> {
    emit_admission_step(controller, request, ordinal, None, stores).await
}

/// Run `admitted`'s root for a session whose store could not be opened: its
/// close or tombstone already committed (FIG-3881). The root's start marker
/// and seal are still emitted — an attempt that stopped short of them would
/// diverge from the journal an earlier attempt of the run recorded (ADR 0104
/// O1) — and the seal's recorded body answers the session's retirement, which
/// every redrive of the run decodes. A seal an earlier attempt recorded
/// answers what it answered then: a superseded or lost admission is the
/// refused root it was. `controller` serves the root's
/// [`drive_root_scope`](crate::engine::drive_root_scope).
#[doc(hidden)]
pub async fn run_admitted_root_retired(
    controller: &ScopedEffectController<'_>,
    admitted: Admitted,
) -> Result<RootOutcome, DriveAbort> {
    let scope = controller.admitted_scope().clone();
    let verdict = Box::pin(mark_and_seal_root(controller, &scope, &admitted, None)).await?;
    retired_root_outcome(&admitted, verdict)
}

/// What a root answers whose session retired before it ran: the refused root
/// its seal recorded, or, for a seal that recorded the admission sealed, the
/// retirement. A root sealed before its session's close is one the close
/// ended and whose execution it released, so no run of it goes on.
fn retired_root_outcome(
    admitted: &Admitted,
    verdict: crate::engine::SealVerdict,
) -> Result<RootOutcome, DriveAbort> {
    match verdict {
        crate::engine::SealVerdict::Sealed(_) => Err(DriveAbort::Refused(
            RuntimeError::new(
                RuntimeErrorCode::SessionDeleted,
                format!(
                    "root `{}` of session `{}` was sealed before its session retired; its execution ended with the session's close",
                    admitted.root(),
                    admitted.session()
                ),
            )
            .with_cause(crate::RuntimeErrorCause::SessionDeleted {
                session_id: admitted.session().clone(),
            }),
        )),
        verdict => Ok(RootOutcome::Refused {
            root: admitted.root().clone(),
            verdict,
        }),
    }
}

/// Run `admitted`'s root to its terminal through `controller`, which must
/// serve [`drive_root_scope`](crate::engine::drive_root_scope) for the root:
/// the recorded `SealDriveAdmission` step, then the root's turns (a frame
/// switch's follow-on turns included) and their commits.
pub async fn run_admitted_root(
    runtime: &mut LashRuntime,
    controller: &ScopedEffectController<'_>,
    admitted: Admitted,
) -> Result<RootOutcome, DriveAbort> {
    run_admitted_root_with(runtime, controller, admitted, DriveSinks::default()).await
}

/// [`run_admitted_root`], publishing every turn to `sinks`.
#[doc(hidden)]
pub async fn run_admitted_root_with(
    runtime: &mut LashRuntime,
    controller: &ScopedEffectController<'_>,
    admitted: Admitted,
    sinks: DriveSinks<'_>,
) -> Result<RootOutcome, DriveAbort> {
    Box::pin(runtime.run_engine_root(controller, admitted, &sinks))
        .await
        .map(|run| run.outcome)
}

/// Discard what a failed root attempt left on `runtime` (FIG-3825): a driver
/// that runs several roots on one runtime calls it before the next root, so
/// that root starts from the durable session as a redrive in a fresh process
/// does, without reopening the runtime. A dropped attempt's guard discards
/// the same as it drops (FIG-3984).
#[doc(hidden)]
pub fn discard_root_residue(runtime: &mut LashRuntime) {
    runtime.discard_root_residue();
}

/// What one admitted root's run left behind in this process: how it ended,
/// the physical turns it assembled when they ran here, and the accepted
/// inputs its recorded claim drove.
///
/// The facade's settled-root mailbox is filled from it (FIG-3600 S5b): a
/// handle waiting on one of `driven_inputs` answers with the turn as it ran,
/// instead of rebuilding a thinner report from the store.
#[doc(hidden)]
pub struct RootReport {
    pub outcome: RootOutcome,
    pub run: Option<AgentFrameRun>,
    pub driven_inputs: Vec<crate::InputId>,
}

impl From<RootRun> for RootReport {
    fn from(run: RootRun) -> Self {
        Self {
            outcome: run.outcome,
            run: run.run,
            driven_inputs: run.driven_inputs,
        }
    }
}

/// [`run_admitted_root_with`], answering the root's [`RootReport`].
#[doc(hidden)]
pub async fn run_admitted_root_reporting(
    runtime: &mut LashRuntime,
    controller: &ScopedEffectController<'_>,
    admitted: Admitted,
    sinks: DriveSinks<'_>,
) -> Result<RootReport, DriveAbort> {
    Box::pin(runtime.run_engine_root(controller, admitted, &sinks))
        .await
        .map(RootReport::from)
}

/// The root a physical turn belongs to, and its ordinal within the root: a
/// root's turns are the root itself, then `{root}:agent-frame:{n}`
/// ([`PhysicalTurn::derive_turn_id`](crate::store::PhysicalTurn::derive_turn_id)).
#[must_use]
pub fn root_of_physical_turn(turn: &TurnId) -> (TurnId, u64) {
    if let Some((root, ordinal)) = turn.as_str().rsplit_once(":agent-frame:")
        && let Ok(ordinal) = ordinal.parse::<u64>()
        && ordinal > 0
    {
        return (TurnId::from(root), ordinal);
    }
    (turn.clone(), 0)
}

/// Physical turn `ordinal` of `root`.
#[must_use]
pub fn physical_turn_of(root: &TurnId, ordinal: u64) -> TurnId {
    crate::store::PhysicalTurn::derive_turn_id(root, ordinal)
}

/// The disposition of a runtime error that ends a drive attempt, by its cause
/// (FIG-3575). A live fault recorded nothing, so the engine retries the
/// attempt, whether or not the identical call is declared safe to repeat:
/// its retry is the redrive that repairs it (ADR 0104 O3, FIG-3897). A
/// refusal that parks a root parks it, and an outcome is refused.
pub(crate) fn drive_abort(root: Option<&TurnId>, error: RuntimeError) -> DriveAbort {
    match (error.turn_failure_cause(), root) {
        (crate::TurnFailureCause::Parked, Some(root)) => DriveAbort::Parked {
            root: root.clone(),
            error: Box::new(error),
        },
        (crate::TurnFailureCause::LiveFault, _) => DriveAbort::Retry(error),
        (crate::TurnFailureCause::Parked | crate::TurnFailureCause::Outcome, _) => {
            DriveAbort::Refused(error)
        }
    }
}

/// Whether an engine retries the root attempt that failed with `error`: a
/// live fault, unless it is a superseded commit (FIG-4010). An engine's retry
/// replays the admission base and the drive fence its journal recorded
/// (FIG-3682), so a commit refused because the head moved under the root, or
/// because its fence was superseded, meets the same refusal on every retry
/// and can never commit. The root ends in the attempt that met it, with the
/// superseded commit as its typed refusal; the redrive that reloads the head
/// is a new root.
pub(crate) fn engine_retries(error: &RuntimeError) -> bool {
    error.turn_failure_cause() == crate::TurnFailureCause::LiveFault
        && error.code != RuntimeErrorCode::StoreCommitSuperseded
}

/// A controller for one step of a drive under `admitted`: the drive's own
/// controller when it already serves that scope, a rescope of it when it can
/// build itself for another scope, and otherwise one the runtime's effect
/// host lends for the scope. A host never lends a drive its handler's
/// controller: the engine's session drive is the only executor (D5).
fn step_controller<'a>(
    controller: &ScopedEffectController<'a>,
    host: &'a dyn crate::EffectHost,
    admitted: crate::AdmittedScope,
) -> Result<ScopedEffectController<'a>, RuntimeError> {
    if controller.execution_scope() == admitted.scope() {
        return Ok(controller.clone());
    }
    if controller.is_scope_bound() {
        return controller.rescope(admitted);
    }
    host.scoped(admitted)
}

fn controller_abort(root: Option<&TurnId>, error: RuntimeEffectControllerError) -> DriveAbort {
    drive_abort(root, error.into_runtime_error())
}

/// One engine attempt of a root on the resident runtime, from its entry
/// until it returns or the engine drops it (FIG-3984).
///
/// Restate stops polling a handler that suspends at an await, so an attempt
/// can end where it awaited and nothing after that await runs. The resident
/// runtime then still holds what the attempt did to it: its sealed root,
/// the turn index its admission recorded, and a
/// resident session holding a code cell that returned but was never
/// settled. A guard dropped before its attempt returned discards all of it,
/// and invalidates the resident session so the next use reloads the durable
/// session: the engine's next attempt replays the journal from its start
/// and must start from that session (FIG-3982), and a host's read of the
/// runtime in between must not see the dropped attempt's state.
///
/// An attempt dropped after its root's terminal commit, suspended in the
/// commit's host delivery, has already adopted that commit: the resident
/// session is the durable head, and its code cells were settled before the
/// commit captured them. The guard discards only the attempt's own fields
/// then, and the committed state stays adopted (FIG-4027).
struct EngineAttempt<'r> {
    runtime: &'r mut LashRuntime,
    returned: bool,
}

impl<'r> EngineAttempt<'r> {
    fn enter(runtime: &'r mut LashRuntime) -> Self {
        debug_assert!(
            !runtime.engine_retries_root,
            "an engine attempt entered inside another one"
        );
        runtime.engine_retries_root = true;
        Self {
            runtime,
            returned: false,
        }
    }
}

impl Drop for EngineAttempt<'_> {
    fn drop(&mut self) {
        let runtime = &mut *self.runtime;
        runtime.engine_retries_root = false;
        if self.returned {
            return;
        }
        let committed = runtime
            .drive_root
            .as_ref()
            .is_some_and(|root| root.terminal_written);
        if committed {
            runtime.discard_attempt_fields();
        } else {
            runtime.discard_root_residue();
        }
    }
}

impl LashRuntime {
    /// The drive loop: admit, seal and run roots until admission stops, or
    /// until `done` says the root just run is the one the caller waited for.
    /// A follow-on recovery is run or declined as `follow_on` says.
    pub(crate) async fn drive_until(
        &mut self,
        controller: &ScopedEffectController<'_>,
        request: &DriveRequest,
        sinks: &DriveSinks<'_>,
        live: Option<(&crate::InputId, &crate::TurnInput)>,
        limits: DriveLimits,
        mut done: impl FnMut(&RootRun) -> bool,
    ) -> Result<DriveRun, DriveAbort> {
        let mut runs: Vec<RootRun> = Vec::new();
        let mut rules = DriveLoop::new();
        let mut ordinal = 0_u32;
        let mut declined_follow_on = false;
        let mut budget_exhausted = false;
        let stop = loop {
            let admitted = match Box::pin(self.admit_drive_step(controller, request, ordinal))
                .await?
            {
                AdmitVerdict::Admit(admitted) => admitted,
                AdmitVerdict::Idle => break DriveStop::Idle,
                AdmitVerdict::Parked(park) => break DriveStop::Parked(park),
                AdmitVerdict::SubstrateLost { root } => break DriveStop::SubstrateLost { root },
                AdmitVerdict::RootTerminal { root, kind, commit } => {
                    break DriveStop::RootTerminal { root, kind, commit };
                }
            };
            if limits.follow_on == FollowOnRecovery::Decline
                && matches!(
                    admitted.work(),
                    crate::engine::AdmittedWork::FollowOn { .. }
                )
            {
                declined_follow_on = true;
                break DriveStop::Idle;
            }
            ordinal = ordinal.checked_add(1).ok_or_else(|| {
                DriveAbort::Refused(RuntimeError::new(
                    RuntimeErrorCode::QueuedWork,
                    "a drive exhausted its admission ordinals",
                ))
            })?;
            if let Err(stop) = rules.before(&admitted) {
                break stop;
            }
            let work = admitted.work().clone();
            let run =
                Box::pin(self.run_admitted_root_step(controller, admitted, sinks, live)).await?;
            let stop = rules.after(&work, &run.outcome);
            let finished = done(&run);
            let root = run.outcome.root().clone();
            runs.push(run);
            if let Some(stop) = stop {
                break stop;
            }
            if finished {
                break DriveStop::Yielded { root };
            }
            if limits.max_roots.is_some_and(|max| runs.len() >= max) {
                budget_exhausted = true;
                break DriveStop::Yielded { root };
            }
        };
        let outcome = DriveOutcome {
            ran: runs.iter().map(|run| run.outcome.clone()).collect(),
            stop,
        };
        Ok(DriveRun {
            outcome,
            runs,
            declined_follow_on,
            budget_exhausted,
        })
    }

    pub(crate) async fn admit_drive_step(
        &mut self,
        controller: &ScopedEffectController<'_>,
        request: &DriveRequest,
        ordinal: u32,
    ) -> Result<AdmitVerdict, DriveAbort> {
        if request.session != self.state.session_id {
            return Err(DriveAbort::Refused(RuntimeError::new(
                RuntimeErrorCode::ExecutionScopeAdmissionRefused,
                format!(
                    "drive request for session `{}` reached the runtime of session `{}`",
                    request.session, self.state.session_id
                ),
            )));
        }
        let store = self.drive_store()?;
        // A generation this build cannot run is refused typed before
        // anything is admitted (FIG-3619): the recorded step's first read is
        // that same gate, so no unrecorded read precedes it here. The body
        // records the resident head as the admission's view of the session;
        // the root's recorded claim, taken under the lease on a head
        // refreshed there, is the head the root runs on (FIG-3682). The
        // session's own retirement is not refused here either: the journaled
        // step below is the durable answer a redrive replays, and
        // `admit_drive_retired` emits the same step when the engine could
        // not open a store for the retired session at all (FIG-3630,
        // ADR 0104 O1).
        let scope = drive_admission_scope(&request.session, &request.request);
        let host = Arc::clone(&self.host.core.control.effect_host);
        let admission_controller = step_controller(controller, host.as_ref(), scope.clone())
            .map_err(DriveAbort::Refused)?;
        emit_admission_step(
            &admission_controller,
            request,
            ordinal,
            Some(store),
            self.host.core.session_store_factory(),
        )
        .await
    }

    /// Run `admitted`'s root as an engine's attempt of its own: the engine
    /// retries the attempt on a live fault (FIG-3897).
    ///
    /// An engine also ends an attempt by dropping it where it stands:
    /// Restate stops polling a handler that suspends at an await, or whose
    /// attempt failed. The attempt's [`EngineAttempt`] guard discards what
    /// the dropped attempt left on the resident runtime as it is dropped
    /// (FIG-3984), so whoever locks the runtime next, the engine's next
    /// attempt or a host's read, finds it as a redrive in a fresh process
    /// would (FIG-3982).
    ///
    /// An attempt that ends with a refusal no retry changes ends its root in
    /// the store before it returns (FIG-4018): the engine records that
    /// refusal as the run's outcome and never runs the root again, so
    /// nothing else would end it, and the session's next admission would
    /// name it again instead of the work behind it.
    async fn run_engine_root(
        &mut self,
        controller: &ScopedEffectController<'_>,
        admitted: Admitted,
        sinks: &DriveSinks<'_>,
    ) -> Result<RootRun, DriveAbort> {
        let root = evidence_root(&admitted);
        let mut attempt = EngineAttempt::enter(self);
        let run = Box::pin(
            attempt
                .runtime
                .run_admitted_root_step(controller, admitted, sinks, None),
        )
        .await;
        attempt.returned = true;
        drop(attempt);
        let run = run.map_err(|abort| match abort {
            DriveAbort::Retry(error) if !engine_retries(&error) => DriveAbort::Refused(error),
            abort => abort,
        });
        if let Err(DriveAbort::Refused(error)) = &run
            && !error.is_retryable()
        {
            self.end_refused_root(&root, error).await?;
        }
        run
    }

    /// Write the end of `root`, whose run met `refusal`, to the store.
    ///
    /// The write is the refused run's own, made before the engine records
    /// the run's outcome, so the recorded outcome always has its terminal
    /// behind it and the engine's lost-root recovery, which ends only runs
    /// that recorded nothing, never writes a second one. It is an
    /// idempotent store write rather than a recorded step, like the commit
    /// and the park (ADR 0105 §9): a root that already has terminal
    /// evidence is left as it is, so a replay that meets the refusal again
    /// writes nothing more. A store that cannot write it fails the attempt
    /// as a live fault, and the engine's retry meets the refusal again.
    async fn end_refused_root(
        &self,
        root: &TurnId,
        refusal: &RuntimeError,
    ) -> Result<(), DriveAbort> {
        let store = self.drive_store()?;
        match store
            .end_refused_root(
                &self.state.session_id,
                root,
                refusal,
                self.host.core.clock.timestamp_ms(),
            )
            .await
        {
            Ok(Some(_)) => {
                tracing::info!(
                    session_id = %self.state.session_id,
                    root = %root,
                    code = %refusal.code,
                    event = "root.refused_ended",
                    "a root whose run met a refusal no retry changes is ended"
                );
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(error) => Err(DriveAbort::Retry(
                crate::runtime::runtime_error_from_store_commit(error),
            )),
        }
    }

    /// Discard what an earlier root's attempt left on this runtime (its
    /// sealed run, its admitted turn index and the resident session state it
    /// touched), so the next root starts from the
    /// durable session exactly as a redrive in a fresh process does.
    fn discard_root_residue(&mut self) {
        self.discard_attempt_fields();
        self.invalidate_resident_session_state();
    }

    /// Discard the fields a root's attempt keeps only while it runs: its
    /// sealed run and its admitted turn index.
    fn discard_attempt_fields(&mut self) {
        self.drive_root = None;
        self.admitted_turn_index = None;
    }

    /// Seal `admitted`, then run its root.
    ///
    /// `live` carries the live `TurnContext` of an in-process caller whose
    /// accepted input the root may drive (a child session turn's process
    /// correlation and lineage): it cannot cross the durable boundary, so it
    /// is re-attached when the root's admission drives that input.
    pub(crate) async fn run_admitted_root_step(
        &mut self,
        controller: &ScopedEffectController<'_>,
        admitted: Admitted,
        sinks: &DriveSinks<'_>,
        live: Option<(&crate::InputId, &crate::TurnInput)>,
    ) -> Result<RootRun, DriveAbort> {
        let store = self.drive_store()?;
        let root = admitted.root().clone();
        // A root runs under its own turn scope (FIG-3607 contract 4), except
        // under a caller whose scope is a process or a runtime operation: a
        // process-backed child turn keeps the process scope it was admitted
        // under, and so does an operation's turn.
        let scope = match controller.execution_scope() {
            crate::ExecutionScope::Process { .. }
            | crate::ExecutionScope::RuntimeOperation { .. } => controller.admitted_scope().clone(),
            _ => drive_root_scope(admitted.session(), &root),
        };
        let host = Arc::clone(&self.host.core.control.effect_host);
        let root_controller = step_controller(controller, host.as_ref(), scope.clone())
            .map_err(DriveAbort::Refused)?;
        let marked = Box::pin(mark_and_seal_root(
            &root_controller,
            &scope,
            &admitted,
            Some(store),
        ))
        .await;
        let verdict = marked?;
        if !matches!(verdict, crate::engine::SealVerdict::Sealed(_)) {
            return Ok(RootRun {
                outcome: RootOutcome::Refused { root, verdict },
                run: None,
                driven_inputs: Vec::new(),
                empty_drain: None,
            });
        }
        let crate::engine::SealVerdict::Sealed(fence) = verdict else {
            unreachable!("a refused seal returned above");
        };
        let run = DriveRootRun::sealed(&admitted, fence.clone());
        let evidence_root = run.root.clone();
        let outer = self.drive_root.replace(Box::new(run));
        let result = match admitted.work().clone() {
            crate::engine::AdmittedWork::Input { head } => {
                Box::pin(self.run_root(
                    &root_controller,
                    &admitted,
                    &crate::store::AdmittedHead::Input(head),
                    sinks,
                    live,
                    &fence,
                ))
                .await
            }
            crate::engine::AdmittedWork::Queued { head } => {
                Box::pin(self.run_root(
                    &root_controller,
                    &admitted,
                    &crate::store::AdmittedHead::Batch(head),
                    sinks,
                    None,
                    &fence,
                ))
                .await
            }
            crate::engine::AdmittedWork::Commands { .. } => {
                Box::pin(self.run_commands_root(&root_controller, &admitted, &fence)).await
            }
            crate::engine::AdmittedWork::FollowOn {
                follow_on,
                attempts,
            } => {
                Box::pin(self.run_follow_on_root(
                    &root_controller,
                    &admitted,
                    &follow_on,
                    attempts,
                    sinks,
                ))
                .await
            }
        };
        let ran = std::mem::replace(&mut self.drive_root, outer);
        // A committed root's report is handed over before its scope closes
        // (FIG-3979): the close is a recorded step, and the terminal
        // transaction armed its `ScopeClose` obligation, so an execution
        // that dies between the two still closes the scope on its redrive
        // or through the obligation's relay.
        if let Ok(run) = &result
            && let RootOutcome::Committed { root, .. } = &run.outcome
            && let Some(turn) = run.run.as_ref().and_then(AgentFrameRun::final_turn)
        {
            sinks
                .settled
                .settled(
                    self,
                    SettledRoot {
                        root,
                        turn,
                        driven_inputs: &run.driven_inputs,
                    },
                )
                .await;
        }
        // The root ended here: its final commit wrote its evidence, so its
        // scope closes (FIG-3607 item 7). A root that did not end holds its
        // scope open, and a host that owns no scopes has nothing to close.
        if ran.is_some_and(|ran| ran.terminal_written)
            && self.host.core.control.scope_close.owns_scopes()
        {
            Box::pin(self.close_root_scope(&root_controller, admitted.session(), &evidence_root))
                .await?;
        }
        result
    }

    /// The recorded `CloseRootScope` step of `root`, after its terminal
    /// evidence: under the root's scope at
    /// [`drive_close_root_replay_key`](crate::engine::drive_close_root_replay_key),
    /// so a redrive of a root that already closed replays the close, and one
    /// that crashed before it closes the root again.
    async fn close_root_scope(
        &self,
        root_controller: &ScopedEffectController<'_>,
        session: &crate::SessionId,
        root: &TurnId,
    ) -> Result<(), DriveAbort> {
        let store = self.drive_store()?;
        let invocation = RuntimeEffectInvocation::new(
            EffectAddress::new(
                root_controller.execution_scope().clone(),
                crate::engine::drive_close_root_replay_key(root),
            )
            .map_err(|error| DriveAbort::Refused(RuntimeError::from(error)))?,
            RuntimeAttribution::for_turn_admission(session.clone(), root.clone()),
            format!("{root}.drive-close"),
        );
        root_controller
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    invocation,
                    RuntimeEffectCommand::CloseRootScope { root: root.clone() },
                ),
                RuntimeEffectLocalExecutor::owned_runner(
                    Box::new(close::CloseRootScopeRunner {
                        store,
                        session: session.clone(),
                        root: root.clone(),
                        sink: Arc::clone(&self.host.core.control.scope_close),
                        relay: Arc::new(scope_close::ScopeCloseRelay::over_backend(
                            self.host.core.backend(),
                            self.host.core.session_store_factory(),
                            Arc::clone(&self.host.core.control.scope_close),
                        )),
                        clock: Arc::clone(&self.host.core.clock),
                    }),
                    None,
                ),
            )
            .await
            .and_then(crate::RuntimeEffectOutcome::into_close_root_scope)
            .map(|_| ())
            .map_err(|error| controller_abort(Some(root), error))
    }

    fn drive_store(&self) -> Result<Arc<dyn crate::store::RuntimePersistence>, DriveAbort> {
        self.session
            .as_ref()
            .and_then(|session| session.history_store())
            .ok_or_else(|| {
                DriveAbort::Refused(RuntimeError::new(
                    RuntimeErrorCode::QueuedWork,
                    "a session drive requires a durable session store",
                ))
            })
    }
}

/// Draw this execution's start marker in the root's own journal, then seal
/// the admission with it (ADR 0105 §2, L-S8). `store` is the session's
/// history store, or `None` when the engine could not open the session at
/// all — its close or tombstone already committed — in which case the seal's
/// recorded body is the retirement itself (FIG-3881).
async fn mark_and_seal_root(
    root_controller: &ScopedEffectController<'_>,
    scope: &crate::AdmittedScope,
    admitted: &Admitted,
    store: Option<Arc<dyn crate::store::RuntimePersistence>>,
) -> Result<crate::engine::SealVerdict, DriveAbort> {
    let root = admitted.root().clone();
    // The execution's start marker, drawn in the root's own journal before
    // the seal: a retry replays it, an execution that cannot read the
    // journal draws another, and the seal refuses that one (L-S8).
    let start = RuntimeEffectInvocation::new(
        EffectAddress::new(scope.scope().clone(), drive_root_start_replay_key(admitted))
            .map_err(|error| DriveAbort::Refused(RuntimeError::from(error)))?,
        RuntimeAttribution::for_turn_admission(admitted.session().clone(), root.clone()),
        format!("{root}.drive-root-start"),
    );
    let root_start = root_controller
        .execute_effect(
            RuntimeEffectEnvelope::new(
                start,
                RuntimeEffectCommand::DrawRootStart { root: root.clone() },
            ),
            RuntimeEffectLocalExecutor::owned_runner(
                Box::new(crate::runtime::root_start::DrawRootStartRunner { root: root.clone() }),
                None,
            ),
        )
        .await
        .and_then(crate::RuntimeEffectOutcome::into_draw_root_start)
        .map_err(|error| controller_abort(Some(&root), error))?;
    let invocation = RuntimeEffectInvocation::new(
        EffectAddress::new(scope.scope().clone(), drive_seal_replay_key(admitted))
            .map_err(|error| DriveAbort::Refused(RuntimeError::from(error)))?,
        RuntimeAttribution::for_turn_admission(admitted.session().clone(), root.clone()),
        format!("{root}.drive-seal"),
    );
    let verdict = root_controller
        .execute_effect(
            RuntimeEffectEnvelope::new(
                invocation,
                RuntimeEffectCommand::SealDriveAdmission {
                    admitted: Box::new(admitted.clone()),
                },
            ),
            RuntimeEffectLocalExecutor::owned_runner(
                Box::new(admission::SealDriveRunner {
                    store,
                    admitted: admitted.clone(),
                    root_start,
                }),
                None,
            ),
        )
        .await
        .and_then(crate::RuntimeEffectOutcome::into_seal_drive_admission)
        .map_err(|error| controller_abort(Some(&root), error))?;
    Ok(verdict)
}
