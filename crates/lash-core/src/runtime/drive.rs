//! The session drive: every turn runs through one drive body whose admission
//! is a recorded step (FIG-3600, ADR 0104 O1/O2/O6, ADR 0105 §2).
//!
//! A drive of one session loops: a recorded `AdmitDrive` step decides what
//! runs next (an unfinished root it resumes, or the queue prefix it takes,
//! minting the root), a recorded `SealDriveAdmission` step raises the
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
//! replay re-evaluates them. Moving them into recorded steps, and teaching the
//! determinism lint to see store calls, is FIG-3824.
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
mod control;
mod reconcile;
mod root;

pub use control::apply_control_intent;
pub use reconcile::{ReconcileReport, reconcile_drive_request, reconcile_session_work};

use std::sync::Arc;

use crate::engine::{
    AdmitRequest, AdmitVerdict, Admitted, DriveAbort, DriveLoop, DriveOutcome, DriveRequest,
    DriveStop, RootOutcome, drive_admission_replay_key, drive_admission_scope, drive_root_scope,
    drive_seal_replay_key,
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

/// One admitted root's run, with the physical turns it assembled.
pub(crate) struct RootRun {
    pub(crate) outcome: RootOutcome,
    /// The root's physical turns, when its turns ran in this process.
    pub(crate) run: Option<AgentFrameRun>,
    /// The accepted inputs the root's recorded claim drove.
    pub(crate) driven_inputs: Vec<crate::InputId>,
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
    Box::pin(runtime.drive_until(controller, request, &sinks, None, |_| false))
        .await
        .map(|(outcome, _)| outcome)
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
/// host lends for the scope.
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
    pub(crate) async fn drive_until(
        &mut self,
        controller: &ScopedEffectController<'_>,
        request: &DriveRequest,
        sinks: &DriveSinks<'_>,
        live: Option<(&crate::InputId, &crate::TurnInput)>,
        mut done: impl FnMut(&RootRun) -> bool,
    ) -> Result<(DriveOutcome, Vec<RootRun>), DriveAbort> {
        let mut runs: Vec<RootRun> = Vec::new();
        let mut rules = DriveLoop::new();
        let mut ordinal = 0_u32;
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
        Ok((outcome, runs))
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
                        base: crate::store::SessionHeadRef {
                            // Read by the admission body.
                            generation: 0,
                            revision: self.state.head_revision,
                            leaf: self.state.session_graph.leaf_node_id.clone(),
                            checkpoint: self.state.checkpoint_ref.clone(),
                        },
                        // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                        turn_index: self.state.turn_index as u64 + 1,
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
        let invocation = RuntimeEffectInvocation::new(
            EffectAddress::new(scope.scope().clone(), drive_seal_replay_key(&admitted))
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
                    }),
                    None,
                ),
            )
            .await
            .and_then(crate::RuntimeEffectOutcome::into_seal_drive_admission)
            .map_err(|error| controller_abort(Some(&root), error))?;
        if !matches!(verdict, crate::engine::SealVerdict::Sealed(_)) {
            return Ok(RootRun {
                outcome: RootOutcome::Refused { root, verdict },
                run: None,
                driven_inputs: Vec::new(),
            });
        }
        match admitted.work().clone() {
            crate::engine::AdmittedWork::Input { head } => {
                Box::pin(self.run_input_root(&root_controller, &admitted, &head, sinks, live)).await
            }
            crate::engine::AdmittedWork::Queued => {
                Box::pin(self.run_queued_root(&root_controller, &admitted, sinks)).await
            }
        }
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
