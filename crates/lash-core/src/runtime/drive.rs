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
//! What it does not yet guarantee: between admission and the root's claim the
//! root still reads and writes live store state no step records. That covers
//! adopting the admitted head (the resident head after the lease refresh,
//! `committed_turn_exists`, the pending inputs), the orphaned-input repair
//! that runs before the claim, and the session-state version read ahead of
//! `AdmitDrive`. These are fenced by the session's execution lease, and a
//! replay re-evaluates them. Moving them into recorded steps is FIG-3824.
//! The substrate lint's rule 6 pins every direct store call in this module
//! and its children, tagged `RECORDED` when a recorded step's body makes it
//! and `FIG-3824` when the drive makes it outside any step, so a new
//! unrecorded read cannot land unseen. It cannot see a helper the drive
//! calls that reaches the store itself, such as the orphaned-input repair.
//!
//! Serialization is the SQL session execution lease's until S8: the drive
//! epoch the seal raises answers a stale admission `Superseded`, but claims
//! and commits do not check the [`DriveFence`](crate::store::DriveFence) yet.
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
mod close;
mod control;
mod reconcile;
mod root;
mod turn_config;

pub use control::apply_control_intent;
pub use reconcile::{ReconcileReport, reconcile_drive_request, reconcile_session_work};
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
}

impl Default for DriveSinks<'_> {
    fn default() -> Self {
        Self {
            events: &crate::runtime::NOOP_EVENT_SINK,
            turn_events: &crate::runtime::NOOP_TURN_ACTIVITY_SINK,
            local_stop: LocalTurnStop::default(),
        }
    }
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
    fence: crate::store::DriveFence,
    /// Whether a commit of this run wrote the root's terminal evidence.
    terminal_written: bool,
}

impl DriveRootRun {
    fn sealed(admitted: &Admitted, fence: crate::store::DriveFence) -> Self {
        let root = match admitted.work() {
            crate::engine::AdmittedWork::FollowOn { follow_on, .. } => {
                crate::store::QueuedRunPosition::split_turn_id(follow_on).0
            }
            crate::engine::AdmittedWork::Input { .. } | crate::engine::AdmittedWork::Queued => {
                admitted.root().clone()
            }
        };
        Self {
            root,
            fence,
            terminal_written: false,
        }
    }

    /// What the commit of physical turn `turn` presents: the fence, and the
    /// root's terminal evidence when the turn ends the root. `None` for a
    /// turn that is not one of the root's physical turns.
    ///
    /// A turn ends its root when it finishes or stops and leaves nothing
    /// owed: no follow-on on the head, and no withheld work a follow-on turn
    /// drives. A queued run ends its root when this commit settles it.
    pub(crate) fn commit_facts(
        &self,
        turn: &TurnId,
        outcome: &crate::TurnOutcome,
        ends: RootEnd,
    ) -> Option<(
        crate::store::DriveFence,
        Option<crate::store::RootTerminalWrite>,
    )> {
        let commit = crate::store::TurnCommitId::of_physical_turn(&self.root, turn)?;
        let (stop, terminal) = match outcome {
            crate::TurnOutcome::Finished(_) => (None, true),
            crate::TurnOutcome::Stopped(stop) => (Some(stop.clone()), true),
            crate::TurnOutcome::AgentFrameSwitch { .. } => (None, false),
        };
        let ends = match ends {
            RootEnd::Settles => true,
            RootEnd::Continues => false,
            RootEnd::Unless { owes_follow_on } => terminal && !owes_follow_on,
        };
        Some((
            self.fence.clone(),
            ends.then(|| crate::store::RootTerminalWrite {
                root: self.root.clone(),
                commit,
                turn: turn.clone(),
                stop,
            }),
        ))
    }

    pub(crate) fn mark_terminal_written(&mut self) {
        self.terminal_written = true;
    }

    /// Mark the evidence a queued run's settlement wrote for this root: a
    /// failed or empty settlement ends the root without a head commit, and
    /// its own transaction wrote the evidence (FIG-3600 S7).
    pub(crate) fn mark_settled(&mut self, settlement: &crate::store::QueuedRunCommit) {
        if self.root.as_str() == settlement.scope.id()
            && matches!(
                &settlement.progress,
                crate::store::QueuedRunProgress::Settle { terminal }
                    if crate::store::settled_queued_root_cause(terminal).is_some()
            )
        {
            self.terminal_written = true;
        }
    }
}

/// Whether a physical turn's commit ends its root.
#[derive(Clone, Copy, Debug)]
pub(crate) enum RootEnd {
    /// The commit settles the root's queued run.
    Settles,
    /// The commit advances the root's queued run to another turn.
    Continues,
    /// A turn of an input root ends it by its outcome, unless it leaves a
    /// follow-on owed.
    Unless { owes_follow_on: bool },
}

/// One admitted root's run, with the physical turns it assembled.
pub(crate) struct RootRun {
    pub(crate) outcome: RootOutcome,
    /// The root's physical turns, when its turns ran in this process.
    pub(crate) run: Option<AgentFrameRun>,
    /// The accepted inputs the root's recorded claim drove.
    pub(crate) driven_inputs: Vec<crate::InputId>,
    /// A queued root that ran no turn here: the settled run it replayed, or
    /// why its drain ran nothing.
    pub(crate) queued_drain: Option<crate::runtime::turn_loop::QueuedTurnDrain<()>>,
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

/// How a drive loop ended, with the roots it ran.
pub(crate) struct DriveRun {
    pub(crate) outcome: DriveOutcome,
    pub(crate) runs: Vec<RootRun>,
    /// The drive stopped at an admitted follow-on recovery it declined.
    pub(crate) declined_follow_on: bool,
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
    Box::pin(runtime.drive_until(
        controller,
        request,
        &sinks,
        None,
        FollowOnRecovery::Recover,
        |_| false,
    ))
    .await
    .map(|run| run.outcome)
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
    Box::pin(runtime.run_admitted_root_step(controller, admitted, &sinks, None))
        .await
        .map(|run| run.outcome)
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

/// [`drive_session_with`], also answering each root's [`RootReport`].
#[doc(hidden)]
pub async fn drive_session_reporting(
    runtime: &mut LashRuntime,
    controller: &ScopedEffectController<'_>,
    request: &DriveRequest,
    sinks: DriveSinks<'_>,
) -> Result<(DriveOutcome, Vec<RootReport>), DriveAbort> {
    Box::pin(runtime.drive_until(
        controller,
        request,
        &sinks,
        None,
        FollowOnRecovery::Recover,
        |_| false,
    ))
    .await
    .map(|run| {
        (
            run.outcome,
            run.runs.into_iter().map(RootReport::from).collect(),
        )
    })
}

/// [`run_admitted_root_with`], answering the root's [`RootReport`].
#[doc(hidden)]
pub async fn run_admitted_root_reporting(
    runtime: &mut LashRuntime,
    controller: &ScopedEffectController<'_>,
    admitted: Admitted,
    sinks: DriveSinks<'_>,
) -> Result<RootReport, DriveAbort> {
    Box::pin(runtime.run_admitted_root_step(controller, admitted, &sinks, None))
        .await
        .map(RootReport::from)
}

/// The root a physical turn belongs to, and its ordinal within the root: a
/// root's turns are the root itself, then `{root}:agent-frame:{n}`
/// ([`QueuedRunPosition::derive_turn_id`](crate::store::QueuedRunPosition::derive_turn_id)).
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
    crate::store::QueuedRunPosition::derive_turn_id(root, ordinal)
}

/// The disposition of a runtime error that ends a drive attempt.
pub(crate) fn drive_abort(root: Option<&TurnId>, error: RuntimeError) -> DriveAbort {
    if let Some(root) = root
        && error.turn_failure_cause() == crate::TurnFailureCause::Parked
    {
        return DriveAbort::Parked {
            root: root.clone(),
            error: Box::new(error),
        };
    }
    if error.is_retryable() {
        DriveAbort::Retry(error)
    } else {
        DriveAbort::Refused(error)
    }
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
        follow_on: FollowOnRecovery,
        mut done: impl FnMut(&RootRun) -> bool,
    ) -> Result<DriveRun, DriveAbort> {
        let mut runs: Vec<RootRun> = Vec::new();
        let mut rules = DriveLoop::new();
        let mut ordinal = 0_u32;
        let mut declined_follow_on = false;
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
            if follow_on == FollowOnRecovery::Decline
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
        };
        let outcome = DriveOutcome {
            ran: runs.iter().map(|run| run.outcome.clone()).collect(),
            stop,
        };
        Ok(DriveRun {
            outcome,
            runs,
            declined_follow_on,
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
        // anything is admitted (FIG-3619). The body records the resident head
        // as the admission's view of the session; the root's recorded claim,
        // taken under the lease on a head refreshed there, is the head the
        // root runs on (FIG-3682).
        store.read_session_state_version().await.map_err(|error| {
            drive_abort(None, crate::runtime::runtime_error_from_store_commit(error))
        })?;
        let scope = drive_admission_scope(&request.session, &request.request);
        let host = Arc::clone(&self.host.core.control.effect_host);
        let admission_controller = step_controller(controller, host.as_ref(), scope.clone())
            .map_err(DriveAbort::Refused)?;
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
        };
        admission_controller
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
                        request: admit_request,
                        ordinal,
                        clock: Arc::clone(&self.host.core.clock),
                    }),
                    None,
                ),
            )
            .await
            .and_then(crate::RuntimeEffectOutcome::into_admit_drive)
            .map_err(|error| controller_abort(None, error))
    }

    /// Seal `admitted`, then run its root.
    ///
    /// `live` carries the per-turn state of an in-process caller whose
    /// accepted input the root may drive (a protocol extension, live plugin
    /// inputs): it cannot cross the durable boundary, so it is re-attached
    /// when the root's claim drives that input.
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
        // An input root takes the session's lane before it marks itself
        // started: an execution the lane turns away has run nothing, so it
        // must not leave a start marker that refuses the execution that runs
        // the root after it (L-S8).
        let input_lease = match admitted.work() {
            crate::engine::AdmittedWork::Input { .. } => Some(
                self.claim_session_execution_lease()
                    .await
                    .map_err(|error| drive_abort(Some(&root), error))?,
            ),
            crate::engine::AdmittedWork::Queued | crate::engine::AdmittedWork::FollowOn { .. } => {
                None
            }
        };
        let marked =
            Box::pin(self.mark_and_seal_root(&root_controller, &scope, &admitted, store)).await;
        let verdict = match marked {
            Ok(verdict) => verdict,
            Err(abort) => {
                if let Some(lease) = input_lease.as_ref() {
                    self.release_root_lease(lease.as_ref()).await;
                }
                return Err(abort);
            }
        };
        if !matches!(verdict, crate::engine::SealVerdict::Sealed(_)) {
            if let Some(lease) = input_lease.as_ref() {
                self.release_root_lease(lease.as_ref()).await;
            }
            return Ok(RootRun {
                outcome: RootOutcome::Refused { root, verdict },
                run: None,
                driven_inputs: Vec::new(),
                queued_drain: None,
            });
        }
        let crate::engine::SealVerdict::Sealed(fence) = verdict else {
            unreachable!("a refused seal returned above");
        };
        let run = DriveRootRun::sealed(&admitted, fence);
        let evidence_root = run.root.clone();
        let outer = self.drive_root.replace(Box::new(run));
        let result = match admitted.work().clone() {
            crate::engine::AdmittedWork::Input { head } => {
                Box::pin(self.run_input_root(
                    &root_controller,
                    &admitted,
                    &head,
                    sinks,
                    live,
                    input_lease.flatten(),
                ))
                .await
            }
            crate::engine::AdmittedWork::Queued => {
                Box::pin(self.run_queued_root(&root_controller, &admitted, sinks)).await
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
        // The root ended here: its evidence is durable, so its scope closes
        // (FIG-3607 item 7), whether its final commit or a queued run's failed
        // settlement wrote that evidence. A root that did not end holds its
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
                    }),
                    None,
                ),
            )
            .await
            .and_then(crate::RuntimeEffectOutcome::into_close_root_scope)
            .map(|_| ())
            .map_err(|error| controller_abort(Some(root), error))
    }

    /// Draw this execution's start marker in the root's own journal, then seal
    /// the admission with it (ADR 0105 §2, L-S8).
    async fn mark_and_seal_root(
        &mut self,
        root_controller: &ScopedEffectController<'_>,
        scope: &crate::AdmittedScope,
        admitted: &Admitted,
        store: Arc<dyn crate::store::RuntimePersistence>,
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
                    Box::new(crate::runtime::root_start::DrawRootStartRunner {
                        root: root.clone(),
                    }),
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
