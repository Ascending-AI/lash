//! The session shift: every turn runs through one shift body whose admission
//! is a recorded step (FIG-3600, ADR 0104 O1/O2/O6, ADR 0105 §2).
//!
//! A shift of one session loops: a recorded `AdmitShift` step decides what
//! runs next (an unfinished run it resumes, a follow-on the head owes, or
//! the queue prefix it takes, minting the run), a recorded `SealShiftAdmission` step raises the
//! session's shift epoch for that admission, and the run's turns run to
//! their terminal commit. It stops when admission answers anything but an
//! admitted run. What the shift decides is recorded: a redrive replays the
//! admission and the seal, and the run replays its recorded admission and the
//! head that admission ran on (FIG-3682).
//!
//! The run's admission body repairs orphaned inputs before it binds the admitted
//! head. A separate `InspectAdmittedHead` step records whether that head is
//! ready, overtaken, or divergent. A redrive reads both outcomes from its journal
//! and issues no second repair. The inspection's body is the shift's one live
//! head check, and it runs only when that step is the attempt's live
//! frontier; a redrive honours the recorded verdict at every position
//! (FIG-4058), and the turn's fenced commit meets a head that moved since as
//! a typed refusal. The check cannot select new work or change the admission's
//! recorded base (ADR 0105 §2). A follow-on recovery run records its
//! decision instead, `RecoverFollowOn`, whose body raises the recovery count
//! and retains the head the follow-on's turn runs on, and whose answer, with
//! that head and the turn's index, the run executes on every replay (FIG-4361,
//! FIG-4380).
//! Rule 6 of the substrate lint pins direct store calls and the orphan-repair
//! helper in the shift.
//!
//! The shift epoch fences admission and repairs admissions left by older epochs.
//! Commit CAS still protects the session head from stale writes.
//!
//! The shift names no engine. An engine that runs shifts in process hands
//! [`work_session`] one controller, and the shift rescopes it per step
//! ([`shift_admission_scope`], [`shift_run_scope`]). An engine that splits
//! the shift over its own handlers calls [`admit_shift`] from its
//! per-session handler and [`execute_admitted_run`] from its per-run handler.
//!
//! [`shift_admission_scope`]: crate::engine::shift_admission_scope
//! [`shift_run_scope`]: crate::engine::shift_run_scope

mod admission;
#[cfg(test)]
mod attempt_drop_tests;
mod close;
mod control;
pub mod ingress;
mod interval;
mod lanes;
mod parent_end_relay;
mod park;
mod reconcile;
pub mod relay;
mod relays;
mod run;
pub mod scope_close;
mod turn_config;

pub use control::{ControlIntentRelay, intent_shift_request};
pub use ingress::{FIRST_INGRESS_ATTEMPT, IngressRelay, ingress_shift_request};
pub use interval::{RECOVERY_TICK, RecoveryInterval};
pub use lanes::{LanesTick, RelayLanes};
pub use parent_end_relay::ParentEndRelay;
pub use park::{StoreParkRecovery, run_park_recorded};
pub use reconcile::{
    DrainHandOverCursor, DrainHandOverPass, ReconcileParts, ReconcileProcesses, TurnHandOverPass,
    drain_hand_over_slot, reconcile_once, turn_hand_over_slot,
};
pub use relays::{
    ObligationRelayUnavailable, RelayNeed, RelayParts, RelaySupply, obligation_relays,
};
pub use scope_close::{ScopeCloseRelay, deliver_scope_close};
pub(crate) use turn_config::llm_profile_unconfigured;

use std::sync::Arc;

use crate::engine::{
    AdmitRequest, AdmitVerdict, Admitted, RunOutcome, ShiftAbort, ShiftLoop, ShiftOutcome,
    ShiftRequest, ShiftStop, shift_admission_replay_key, shift_admission_scope, shift_run_scope,
    shift_run_start_replay_key, shift_seal_replay_key,
};
use crate::runtime::LashRuntime;
use crate::{
    AgentFrameRun, EffectAddress, EventSink, LocalTurnStop, RuntimeAttribution,
    RuntimeEffectCommand, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectInvocation, RuntimeError, RuntimeErrorCode, ScopedEffectController,
    TurnActivitySink, TurnId,
};

/// Where the turns of a shift publish, and the host-local stop they honour.
///
/// An engine-run shift publishes to its session's observation sinks; the
/// facade's in-process entries publish to the caller's too. Nothing a turn
/// publishes decides anything.
#[derive(Clone)]
pub struct ShiftSinks<'a> {
    pub events: &'a dyn EventSink,
    pub turn_events: &'a dyn TurnActivitySink,
    pub local_stop: LocalTurnStop,
    /// Where each run this shift commits hands its report over.
    pub settled: &'a dyn RunSettledSink,
}

impl Default for ShiftSinks<'_> {
    fn default() -> Self {
        Self {
            events: &crate::runtime::NOOP_EVENT_SINK,
            turn_events: &crate::runtime::NOOP_TURN_ACTIVITY_SINK,
            local_stop: LocalTurnStop::default(),
            settled: &NoopRunSettledSink,
        }
    }
}

/// A run this runtime ran to its final commit: its final physical turn as
/// it ran, and the accepted inputs its admission drove.
pub struct SettledRun<'r> {
    pub run: &'r TurnId,
    pub turn: &'r crate::AssembledTurn,
    pub executed_inputs: &'r [crate::InputId],
}

/// Where a shift hands a committed run's report over (FIG-3979): once its
/// final commit and that commit's `TurnPersisted` delivery are done, before
/// the run's recorded scope close. The runtime passed is the one that ran
/// the run, holding its commit. Nothing a sink does decides anything, and
/// the shift waits for it before it closes the scope or hands the close to
/// the engine, so a sink returns at once.
#[async_trait::async_trait]
pub trait RunSettledSink: Send + Sync {
    async fn settled(&self, runtime: &LashRuntime, run: SettledRun<'_>);
}

/// A sink that takes no report.
pub struct NoopRunSettledSink;

#[async_trait::async_trait]
impl RunSettledSink for NoopRunSettledSink {
    async fn settled(&self, _runtime: &LashRuntime, _run: SettledRun<'_>) {}
}

/// The admitted run a runtime is running, and the fence its seal raised
/// (FIG-3600 S7, ADR 0105 §2).
///
/// Every commit of one of the run's physical turns presents the fence, so a
/// successor's seal refuses it; the commit of its final physical turn writes
/// the run's terminal evidence in its own transaction.
#[derive(Clone, Debug)]
pub(crate) struct RunExecution {
    /// The logical run the evidence names. A follow-on recovery's run is
    /// the recovery's; its evidence names the run that owed the follow-on.
    run: TurnId,
    pub(crate) fence: crate::store::ShiftFence,
    /// The drain generation stamped on the journal the run executes on: the
    /// admitting build's (FIG-3795 S9, FIG-4742). A park this run writes
    /// records it, so the drain routes the run's resume to the build its
    /// journal belongs to.
    journal_generation: crate::engine::BuildGeneration,
    /// Whether the run's terminal evidence is durable: a commit of this run
    /// wrote it, or answered the receipt of the commit that did. It gates the
    /// recorded scope-close step, so it is a durable fact every execution of
    /// the run reads alike, never whether this execution was the writer
    /// (FIG-3893).
    terminal_written: bool,
}

impl RunExecution {
    fn sealed(admitted: &Admitted, fence: crate::store::ShiftFence) -> Self {
        Self {
            run: evidence_run(admitted),
            fence,
            journal_generation: admitted.admitted_generation().clone(),
            terminal_written: false,
        }
    }

    /// What the commit of physical turn `turn` presents: the fence, the run
    /// whose admitted rows it settles, and the run's terminal evidence when
    /// the turn ends the run. `None` for a turn that is not one of the
    /// run's physical turns.
    ///
    /// A turn ends its run when it finishes or stops and leaves nothing
    /// owed: no follow-on on the head, and no withheld work a follow-on turn
    /// executes (`owes_follow_on`).
    pub(crate) fn commit_facts(
        &self,
        turn: &TurnId,
        outcome: &crate::TurnOutcome,
        owes_follow_on: bool,
    ) -> Option<ShiftCommit> {
        let commit = crate::store::TurnCommitId::of_physical_turn(&self.run, turn)?;
        // A frame switch ends no run; its run goes on in the next physical
        // turn.
        let ended = crate::store::RunCommittedOutcome::of_turn_outcome(outcome);
        Some(ShiftCommit {
            fence: self.fence.clone(),
            run: self.run.clone(),
            terminal: ended.filter(|_| !owes_follow_on).map(|outcome| {
                crate::store::RunTerminalWrite {
                    run: self.run.clone(),
                    commit,
                    turn: turn.clone(),
                    outcome,
                }
            }),
        })
    }

    /// The logical run this execution's evidence and park name.
    pub(crate) fn run(&self) -> &TurnId {
        &self.run
    }

    /// The drain generation stamped on the journal this execution's run executes on.
    pub(crate) fn journal_generation(&self) -> &crate::engine::BuildGeneration {
        &self.journal_generation
    }

    pub(crate) fn mark_terminal_written(&mut self) {
        self.terminal_written = true;
    }
}

/// The logical run whose terminal evidence `admitted`'s run writes: the
/// admitted run, except for a follow-on recovery, whose evidence names the
/// run that owed the follow-on.
fn evidence_run(admitted: &Admitted) -> TurnId {
    match admitted.work() {
        crate::engine::AdmittedWork::FollowOn { follow_on, .. } => {
            crate::store::PhysicalTurn::split_turn_id(follow_on).0
        }
        crate::engine::AdmittedWork::Input { .. }
        | crate::engine::AdmittedWork::Queued { .. }
        | crate::engine::AdmittedWork::Commands { .. } => admitted.run().clone(),
    }
}

/// What a run's physical-turn commit presents (FIG-3600 S7, FIG-3927).
#[derive(Clone, Debug)]
pub(crate) struct ShiftCommit {
    /// The fence of the shift admission the run executes under.
    pub(crate) fence: crate::store::ShiftFence,
    /// The run whose admitted rows the commit settles.
    pub(crate) run: TurnId,
    /// The run's terminal evidence, when the turn ends the run.
    pub(crate) terminal: Option<crate::store::RunTerminalWrite>,
}

/// One admitted run's execution, with the physical turns it assembled.
pub(crate) struct ExecutedRun {
    pub(crate) outcome: RunOutcome,
    /// The run's physical turns, when its turns ran in this process.
    pub(crate) run: Option<AgentFrameRun>,
    /// The accepted inputs the run's recorded admission drove.
    pub(crate) executed_inputs: Vec<crate::InputId>,
    /// A follow-on recovery run that ran no turn here: why its drain ran
    /// nothing.
    pub(crate) empty_drain: Option<crate::runtime::turn_loop::EmptyQueuedDrainReason>,
}

/// Whether a shift recovers the follow-on the session head owes when
/// admission names it (ADR 0101 §3, FIG-3542).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FollowOnRecovery {
    /// Run the recovery run: every shift an engine runs, and a queued
    /// drain.
    Recover,
    /// Stop before it: a direct turn's own shift, whose input waits behind
    /// the follow-on and is answered queued. The session's shift recovers
    /// the follow-on and then answers the input.
    Decline,
}

/// The bounds a shift loop runs under: whether it recovers the follow-on
/// the session head owes, and how many runs one invocation runs before it
/// yields to a continuation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ShiftLimits {
    pub(crate) follow_on: FollowOnRecovery,
    pub(crate) max_runs: Option<usize>,
    /// The loop is an acceptor's shift of its child session's turn, inline
    /// in its parent's execution, which an engine holds: the runs it admits
    /// record it as their acceptor (FIG-4765, FIG-4814).
    pub(crate) acceptor: bool,
}

/// How a shift loop ended, with the runs it ran.
pub(crate) struct ShiftLoopEnd {
    pub(crate) outcome: ShiftOutcome,
    pub(crate) runs: Vec<ExecutedRun>,
    /// The shift stopped at an admitted follow-on recovery it declined.
    pub(crate) declined_follow_on: bool,
    pub(crate) budget_exhausted: bool,
}

/// Shift `request` on `runtime`'s session to a stop: admit, seal and run
/// runs until admission answers something other than an admitted run.
///
/// `controller` is rescoped for every step: admission ordinal `n` runs under
/// [`shift_admission_scope`](crate::engine::shift_admission_scope) at
/// [`shift_admission_replay_key`](crate::engine::shift_admission_replay_key),
/// and each admitted run under
/// [`shift_run_scope`](crate::engine::shift_run_scope).
pub async fn work_session(
    runtime: &mut LashRuntime,
    controller: &ScopedEffectController<'_>,
    request: &ShiftRequest,
) -> Result<ShiftOutcome, ShiftAbort> {
    work_session_with(runtime, controller, request, ShiftSinks::default()).await
}

/// [`work_session`], publishing every turn to `sinks`.
#[doc(hidden)]
pub async fn work_session_with(
    runtime: &mut LashRuntime,
    controller: &ScopedEffectController<'_>,
    request: &ShiftRequest,
    sinks: ShiftSinks<'_>,
) -> Result<ShiftOutcome, ShiftAbort> {
    let ended = Box::pin(runtime.work_until(
        controller,
        request,
        &sinks,
        None,
        ShiftLimits {
            follow_on: FollowOnRecovery::Recover,
            max_runs: Some(crate::engine::MAX_RUNS_PER_SHIFT),
            acceptor: false,
        },
        |_| false,
    ))
    .await?;
    if ended.budget_exhausted {
        runtime.host.queued_work().schedule_shift(
            &request.session,
            crate::engine::shift_continuation_request(request),
        );
    }
    Ok(ended.outcome)
}

/// Admission `ordinal` of `request`: one recorded `AdmitShift` step through
/// `controller`, which must serve
/// [`shift_admission_scope`](crate::engine::shift_admission_scope) for the
/// request. `draining` is the build generation whose drain the admission
/// hands over for
/// ([`SessionShifts::admit`](crate::runtime::work::SessionShifts::admit)).
///
/// While the session's unfinished run is recorded under an executor that
/// excludes the run's own execution, the step admits nothing and fails retryably
/// with [`RuntimeErrorCode::SessionRunPending`], recording no verdict
/// (FIG-4765).
pub async fn admit_shift(
    runtime: &mut LashRuntime,
    controller: &ScopedEffectController<'_>,
    request: &ShiftRequest,
    ordinal: u32,
    draining: Option<&crate::engine::BuildGeneration>,
) -> Result<AdmitVerdict, ShiftAbort> {
    // An engine that admits through this step executes each run as the run's
    // own run ([`execute_admitted_run`]).
    Box::pin(runtime.admit_shift_step(
        controller,
        request,
        ordinal,
        draining,
        crate::store::RunExecutor::Run,
    ))
    .await
}

/// [`admit_shift`] on no runtime of the session: the step reads only
/// `store`, the session's history store, and `host`'s control-intent ledger
/// and drain marks. Nothing here takes a runtime's writer, so an admission,
/// a replayed one included, never waits for a run that is running on one
/// (FIG-4755).
#[doc(hidden)]
pub async fn admit_shift_on_store(
    host: &crate::RuntimeHostConfig,
    store: crate::store::SessionStore,
    controller: &ScopedEffectController<'_>,
    request: &ShiftRequest,
    admitting_generation: &crate::engine::BuildGeneration,
    ordinal: u32,
    draining: Option<&crate::engine::BuildGeneration>,
) -> Result<AdmitVerdict, ShiftAbort> {
    // An engine that admits on the store executes each run as the run's own
    // run.
    admit_on_store(
        host,
        store,
        controller,
        request,
        AdmissionAuthority {
            generation: admitting_generation,
            executor: crate::store::RunExecutor::Run,
        },
        ordinal,
        draining,
    )
    .await
}

struct AdmissionAuthority<'a> {
    generation: &'a crate::engine::BuildGeneration,
    executor: crate::store::RunExecutor,
}

/// [`admit_shift_on_store`] for `authority`, the execution that runs the
/// runs the shift admits.
async fn admit_on_store(
    host: &crate::RuntimeHostConfig,
    store: crate::store::SessionStore,
    controller: &ScopedEffectController<'_>,
    request: &ShiftRequest,
    authority: AdmissionAuthority<'_>,
    ordinal: u32,
    draining: Option<&crate::engine::BuildGeneration>,
) -> Result<AdmitVerdict, ShiftAbort> {
    // A generation this build cannot run is refused typed before anything
    // is admitted (FIG-3619): the recorded step's first read is that same
    // gate, so no unrecorded read precedes it here. The session's own
    // retirement is not refused here either: the journaled step below is
    // the durable answer a redrive replays, and `admit_shift_retired` emits
    // the same step when the engine could not open a store for the retired
    // session at all (FIG-3630, ADR 0104 O1).
    let scope = shift_admission_scope(&request.session, &request.request);
    let admission_controller =
        step_controller(controller, host.control.effect_host.as_ref(), scope)
            .map_err(ShiftAbort::Refused)?;
    emit_admission_step(
        &admission_controller,
        request,
        authority,
        ordinal,
        Some((store, Arc::clone(&host.clock))),
        host.session_store_factory(),
        draining.map(|generation| admission::DrainRead {
            marks: host.backend().generation_drain(),
            generation: generation.clone(),
        }),
    )
    .await
}

/// Emit admission `ordinal`'s journaled `AdmitShift` step through
/// `controller`, which must serve the request's
/// [`shift_admission_scope`](crate::engine::shift_admission_scope). `store`
/// is the session's history store and the host clock a fault it records is
/// stamped with, or `None` when the session could not be opened at all —
/// deleted, or closed past admission — in which case the step's recorded
/// body is the retirement itself (FIG-3630).
async fn emit_admission_step(
    controller: &ScopedEffectController<'_>,
    request: &ShiftRequest,
    authority: AdmissionAuthority<'_>,
    ordinal: u32,
    store: Option<(crate::store::SessionStore, Arc<dyn crate::Clock>)>,
    stores: Arc<dyn crate::DeploymentStore>,
    drain: Option<admission::DrainRead>,
) -> Result<AdmitVerdict, ShiftAbort> {
    let scope = shift_admission_scope(&request.session, &request.request);
    let invocation = RuntimeEffectInvocation::new(
        EffectAddress::new(
            scope.scope().clone(),
            shift_admission_replay_key(&request.request, ordinal),
        )
        .map_err(|error| ShiftAbort::Refused(RuntimeError::from(error)))?,
        RuntimeAttribution::for_session(request.session.clone()),
        format!("shift-admission-{ordinal}"),
    );
    let admit_request = AdmitRequest {
        session: request.session.clone(),
        request: request.request.clone(),
        build_generation: authority.generation.clone(),
    };
    controller
        .execute_effect(
            RuntimeEffectEnvelope::new(
                invocation,
                RuntimeEffectCommand::AdmitShift {
                    request: Box::new(admit_request.clone()),
                },
            ),
            lash_core_execution::core_internal::owned_runner_executor(
                Box::new(admission::AdmitShiftRunner {
                    store,
                    stores,
                    request: admit_request,
                    ordinal,
                    drain,
                    executor: authority.executor,
                }),
                None,
            ),
        )
        .await
        .and_then(crate::RuntimeEffectOutcome::into_admit_shift)
        .map_err(|error| controller_abort(None, error))
}

/// Admission `ordinal` of `request` for a session whose store could not be
/// opened: its close or tombstone already committed. The journaled
/// `AdmitShift` step still has to be emitted — an attempt that stopped
/// short of it diverges from the `run` command an earlier attempt journaled
/// at this position (ADR 0104 O1) — and its recorded body answers the
/// session's retirement, which every redrive of the invocation decodes.
/// `controller` serves the request's
/// [`shift_admission_scope`](crate::engine::shift_admission_scope).
#[doc(hidden)]
pub async fn admit_shift_retired(
    controller: &ScopedEffectController<'_>,
    request: &ShiftRequest,
    admitting_generation: &crate::engine::BuildGeneration,
    ordinal: u32,
    stores: Arc<dyn crate::DeploymentStore>,
) -> Result<AdmitVerdict, ShiftAbort> {
    // A retired session reads no run, so the executor decides nothing.
    emit_admission_step(
        controller,
        request,
        AdmissionAuthority {
            generation: admitting_generation,
            executor: crate::store::RunExecutor::Run,
        },
        ordinal,
        None,
        stores,
        None,
    )
    .await
}

/// Run `admitted`'s run for a session whose store could not be opened: its
/// close or tombstone already committed (FIG-3881). The run's start marker
/// and seal are still emitted — an attempt that stopped short of them would
/// diverge from the journal an earlier attempt of the run recorded (ADR 0104
/// O1) — and the seal's recorded body answers the session's retirement, which
/// every redrive of the run decodes. A seal an earlier attempt recorded
/// answers what it answered then: a superseded or lost admission is the
/// refused run it was. A run it recorded sealed goes on headless, through
/// the recorded steps an earlier attempt may have journaled after the seal:
/// an input- or queued-headed run's admission and head inspection, a
/// command run's reads of its command lane (FIG-4346), and a follow-on
/// recovery run's decision (FIG-4361). `controller` serves the run's
/// [`shift_run_scope`](crate::engine::shift_run_scope).
#[doc(hidden)]
pub async fn execute_admitted_run_retired(
    controller: &ScopedEffectController<'_>,
    admitted: Admitted,
) -> Result<RunOutcome, ShiftAbort> {
    let scope = controller.admitted_scope().clone();
    // No store is open, so the seal names no executor to one.
    let verdict = Box::pin(mark_and_seal_run(
        controller,
        &scope,
        &admitted,
        None,
        crate::store::RunExecutor::Run,
    ))
    .await?;
    if let crate::engine::SealVerdict::Refused(refusal) = verdict {
        return Ok(RunOutcome::Refused {
            run: admitted.run().clone(),
            refusal,
        });
    }
    let headless = run::HeadlessRun::Retired;
    match admitted.work().clone() {
        crate::engine::AdmittedWork::Input { head } => {
            Box::pin(run::execute_headless_run(
                controller,
                &admitted,
                &crate::store::AdmittedHead::Input(head),
                headless,
            ))
            .await
        }
        crate::engine::AdmittedWork::Queued { head } => {
            Box::pin(run::execute_headless_run(
                controller,
                &admitted,
                &crate::store::AdmittedHead::Batch(head),
                headless,
            ))
            .await
        }
        crate::engine::AdmittedWork::Commands { .. } => {
            Box::pin(run::execute_headless_commands_run(
                controller, &admitted, headless,
            ))
            .await
        }
        crate::engine::AdmittedWork::FollowOn {
            follow_on,
            attempts,
        } => {
            Box::pin(run::execute_headless_follow_on_run(
                controller,
                &admitted,
                &run::FollowOnWork {
                    turn: &follow_on,
                    attempts,
                },
                headless,
            ))
            .await
        }
    }
}

/// Run `admitted`'s run to its terminal through `controller`, which must
/// serve [`shift_run_scope`](crate::engine::shift_run_scope) for the run:
/// the recorded `SealShiftAdmission` step, then the run's turns (a frame
/// switch's follow-on turns included) and their commits, then, for a run
/// that ended, its recorded scope close in the same journal.
pub async fn execute_admitted_run(
    runtime: &mut LashRuntime,
    controller: &ScopedEffectController<'_>,
    admitted: Admitted,
) -> Result<RunOutcome, ShiftAbort> {
    execute_admitted_run_with(runtime, controller, admitted, ShiftSinks::default()).await
}

/// [`execute_admitted_run`], publishing every turn to `sinks`.
#[doc(hidden)]
pub async fn execute_admitted_run_with(
    runtime: &mut LashRuntime,
    controller: &ScopedEffectController<'_>,
    admitted: Admitted,
    sinks: ShiftSinks<'_>,
) -> Result<RunOutcome, ShiftAbort> {
    Box::pin(runtime.execute_engine_run(controller, admitted, &sinks, RunClose::Inline))
        .await
        .map(|executed| executed.outcome)
}

/// [`execute_admitted_run_with`] for an engine that closes the run's scope in
/// a journal of its own (FIG-4035): the execution returns once the run's report
/// is handed over, naming in [`RunEnd::owed_close`] the run whose close
/// the engine then runs through [`close_admitted_run`]. The session's next
/// admission never waits on that close.
///
/// [`RunEnd::owed_close`]: crate::engine::RunEnd::owed_close
#[doc(hidden)]
pub async fn execute_admitted_run_owing_close(
    runtime: &mut LashRuntime,
    controller: &ScopedEffectController<'_>,
    admitted: Admitted,
    sinks: ShiftSinks<'_>,
) -> crate::engine::RunEnd {
    let mut owed_close = None;
    let result = Box::pin(runtime.execute_engine_run(
        controller,
        admitted,
        &sinks,
        RunClose::Owed(&mut owed_close),
    ))
    .await
    .map(|executed| executed.outcome);
    crate::engine::RunEnd { result, owed_close }
}

/// Close the lifetime scope of `run` of `session` that a run's execution owed
/// ([`execute_admitted_run_owing_close`]): the run's recorded `CloseRunScope`
/// step through `controller`, which serves the
/// [`shift_run_scope`](crate::engine::shift_run_scope) of the admitted
/// run whose execution owed it. It needs no runtime of the session, only `host`'s
/// catalog, scope owner and obligation ledger, so it runs beside the
/// session's next run.
#[doc(hidden)]
pub async fn close_admitted_run(
    host: &crate::RuntimeHostConfig,
    controller: &ScopedEffectController<'_>,
    session: &crate::SessionId,
    run: &TurnId,
) -> Result<(), ShiftAbort> {
    let terminals: Arc<dyn crate::store::RuntimeStore> = host.session_store_factory();
    Box::pin(emit_close_step(host, terminals, controller, session, run)).await
}

/// Discard what a failed run attempt left on `runtime` (FIG-3825): a `SessionShifts`
/// that runs several runs on one runtime calls it before the next run, so
/// that run starts from the durable session as a redrive in a fresh process
/// does, without reopening the runtime. A dropped attempt's guard discards
/// the same as it drops (FIG-3984).
#[doc(hidden)]
pub fn discard_run_residue(runtime: &mut LashRuntime) {
    runtime.discard_run_residue();
}

/// What one admitted run's execution left behind in this process: how it ended,
/// the physical turns it assembled when they ran here, and the accepted
/// inputs its recorded admission drove.
///
/// The facade's settled-run mailbox is filled from it (FIG-3600 S5b): a
/// handle waiting on one of `executed_inputs` answers with the turn as it ran,
/// instead of rebuilding a thinner report from the store.
#[doc(hidden)]
pub struct RunReport {
    pub outcome: RunOutcome,
    pub run: Option<AgentFrameRun>,
    pub executed_inputs: Vec<crate::InputId>,
}

impl From<ExecutedRun> for RunReport {
    fn from(executed: ExecutedRun) -> Self {
        Self {
            outcome: executed.outcome,
            run: executed.run,
            executed_inputs: executed.executed_inputs,
        }
    }
}

/// [`execute_admitted_run_with`], answering the run's [`RunReport`].
#[doc(hidden)]
pub async fn execute_admitted_run_reporting(
    runtime: &mut LashRuntime,
    controller: &ScopedEffectController<'_>,
    admitted: Admitted,
    sinks: ShiftSinks<'_>,
) -> Result<RunReport, ShiftAbort> {
    Box::pin(runtime.execute_engine_run(controller, admitted, &sinks, RunClose::Inline))
        .await
        .map(RunReport::from)
}

/// The disposition of a runtime error that ends a shift attempt, by its cause
/// (FIG-3575). A live fault recorded nothing, so the engine retries the
/// attempt, whether or not the identical call is declared safe to repeat:
/// its retry is the redrive that repairs it (ADR 0104 O3, FIG-3897). A
/// refusal that parks a run parks it, and an outcome is refused.
pub(crate) fn shift_abort(run: Option<&TurnId>, error: RuntimeError) -> ShiftAbort {
    match (error.turn_failure_cause(), run) {
        (crate::TurnFailureCause::Parked, Some(run)) => ShiftAbort::Parked {
            run: run.clone(),
            error: Box::new(error),
        },
        (crate::TurnFailureCause::LiveFault, _) => ShiftAbort::Retry(error),
        (crate::TurnFailureCause::Parked | crate::TurnFailureCause::Outcome, _) => {
            ShiftAbort::Refused(error)
        }
    }
}

/// Whether an engine retries the run attempt that failed with `error`: a
/// live fault, unless it is a superseded commit (FIG-4010). An engine's retry
/// replays the admission base and the shift fence its journal recorded
/// (FIG-3682), so a commit refused because the head moved under the run, or
/// because its fence was superseded, meets the same refusal on every retry
/// and can never commit. The run ends in the attempt that met it, with the
/// superseded commit as its typed refusal; the redrive that reloads the head
/// is a new run.
pub(crate) fn engine_retries(error: &RuntimeError) -> bool {
    error.turn_failure_cause() == crate::TurnFailureCause::LiveFault
        && error.code != RuntimeErrorCode::StoreCommitSuperseded
}

/// A controller for one step of a shift under `admitted`: the shift's own
/// controller when it already serves that scope, a rescope of it when it can
/// build itself for another scope, and otherwise one the runtime's effect
/// host lends for the scope. A host never lends a shift its handler's
/// controller: the engine's session shift is the only executor (D5).
pub(crate) fn step_controller<'a>(
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

fn controller_abort(run: Option<&TurnId>, error: RuntimeEffectControllerError) -> ShiftAbort {
    shift_abort(run, error.into_runtime_error())
}

/// One engine attempt of a run on the resident runtime, from its entry
/// until it returns or the engine drops it (FIG-3984).
///
/// Restate stops polling a handler that suspends at an await, so an attempt
/// can end where it awaited and nothing after that await runs. The resident
/// runtime then still holds what the attempt did to it: its sealed run,
/// the turn index its admission recorded, and a
/// resident session holding a code cell that returned but was never
/// settled. A guard dropped before its attempt returned discards all of it,
/// and invalidates the resident session so the next use reloads the durable
/// session: the engine's next attempt replays the journal from its start
/// and must start from that session (FIG-3982), and a host's read of the
/// runtime in between must not see the dropped attempt's state.
///
/// An attempt dropped after its run's terminal commit, suspended in the
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
            !runtime.engine_retries_run,
            "an engine attempt entered inside another one"
        );
        runtime.engine_retries_run = true;
        Self {
            runtime,
            returned: false,
        }
    }
}

impl Drop for EngineAttempt<'_> {
    fn drop(&mut self) {
        let runtime = &mut *self.runtime;
        runtime.engine_retries_run = false;
        if self.returned {
            return;
        }
        let committed = runtime
            .shift_run
            .as_ref()
            .is_some_and(|run| run.terminal_written);
        if committed {
            runtime.discard_attempt_fields();
        } else {
            runtime.discard_run_residue();
        }
    }
}

impl LashRuntime {
    /// The shift loop: admit, seal and run executes until admission stops, or
    /// until `done` says the run just run is the one the caller waited for.
    /// A follow-on recovery is run or declined as `follow_on` says.
    pub(crate) async fn work_until(
        &mut self,
        controller: &ScopedEffectController<'_>,
        request: &ShiftRequest,
        sinks: &ShiftSinks<'_>,
        live: Option<(&crate::InputId, &crate::TurnInput)>,
        limits: ShiftLimits,
        mut done: impl FnMut(&ExecutedRun) -> bool,
    ) -> Result<ShiftLoopEnd, ShiftAbort> {
        let mut runs: Vec<ExecutedRun> = Vec::new();
        let mut rules = ShiftLoop::new();
        let mut ordinal = 0_u32;
        let mut declined_follow_on = false;
        let mut budget_exhausted = false;
        // Every run this loop admits runs inline, in the execution of the
        // shift's controller: no engine execution of the run holds it (FIG-4403).
        // An acceptor's execution is itself a run an engine holds; a session
        // shift's or queue drain's is not (FIG-4814).
        let scope = controller.execution_scope().clone();
        let executor = if limits.acceptor {
            crate::store::RunExecutor::Acceptor { scope }
        } else {
            crate::store::RunExecutor::Inline { scope }
        };
        let stop = loop {
            // This loop's shift is pinned to no build an engine drains: its
            // admissions name no drain, and none answers `Draining`.
            let admitted = match Box::pin(self.admit_shift_step(
                controller,
                request,
                ordinal,
                None,
                executor.clone(),
            ))
            .await?
            {
                AdmitVerdict::Admit(admitted) => admitted,
                AdmitVerdict::Draining { generation } => {
                    return Err(ShiftAbort::Refused(RuntimeError::new(
                        RuntimeErrorCode::QueuedWork,
                        format!(
                            "admission answered the drain of generation `{generation}` to a \
                             shift that named none"
                        ),
                    )));
                }
                AdmitVerdict::Idle => break ShiftStop::Idle,
                AdmitVerdict::Parked(park) => break ShiftStop::Parked(park),
                AdmitVerdict::SubstrateLost { run } => break ShiftStop::SubstrateLost { run },
                AdmitVerdict::RunTerminal { run, kind, commit } => {
                    break ShiftStop::RunTerminal { run, kind, commit };
                }
            };
            if limits.follow_on == FollowOnRecovery::Decline
                && matches!(
                    admitted.work(),
                    crate::engine::AdmittedWork::FollowOn { .. }
                )
            {
                declined_follow_on = true;
                break ShiftStop::Idle;
            }
            ordinal = ordinal.checked_add(1).ok_or_else(|| {
                ShiftAbort::Refused(RuntimeError::new(
                    RuntimeErrorCode::QueuedWork,
                    "a shift exhausted its admission ordinals",
                ))
            })?;
            if let Err(stop) = rules.before(&admitted) {
                break stop;
            }
            let work = admitted.work().clone();
            let executed = Box::pin(self.execute_admitted_run_step(
                controller,
                admitted,
                sinks,
                live,
                RunClose::Inline,
                executor.clone(),
            ))
            .await?;
            let stop = rules.after(&work, &executed.outcome);
            let finished = done(&executed);
            let run = executed.outcome.run().clone();
            runs.push(executed);
            if let Some(stop) = stop {
                break stop;
            }
            if finished {
                break ShiftStop::Yielded { run };
            }
            if limits.max_runs.is_some_and(|max| runs.len() >= max) {
                budget_exhausted = true;
                break ShiftStop::Yielded { run };
            }
        };
        let outcome = ShiftOutcome {
            ran: runs
                .iter()
                .map(|executed| executed.outcome.clone())
                .collect(),
            stop,
        };
        Ok(ShiftLoopEnd {
            outcome,
            runs,
            declined_follow_on,
            budget_exhausted,
        })
    }

    pub(crate) async fn admit_shift_step(
        &mut self,
        controller: &ScopedEffectController<'_>,
        request: &ShiftRequest,
        ordinal: u32,
        draining: Option<&crate::engine::BuildGeneration>,
        executor: crate::store::RunExecutor,
    ) -> Result<AdmitVerdict, ShiftAbort> {
        if request.session != self.state.session_id {
            return Err(ShiftAbort::Refused(RuntimeError::new(
                RuntimeErrorCode::ExecutionScopeAdmissionRefused,
                format!(
                    "shift request for session `{}` reached the runtime of session `{}`",
                    request.session, self.state.session_id
                ),
            )));
        }
        // The body records the store's head as the admission's view of the
        // session; the run's recorded admission, taken under the lease on a
        // head refreshed there, is the head the run executes on (FIG-3682).
        let store = self.shift_store()?;
        let admitting_generation = self
            .host
            .core
            .backend()
            .build_generation()
            .map_err(|error| ShiftAbort::Refused(error.into()))?
            .clone();
        Box::pin(admit_on_store(
            &self.host.core,
            store,
            controller,
            request,
            AdmissionAuthority {
                generation: &admitting_generation,
                executor,
            },
            ordinal,
            draining,
        ))
        .await
    }

    /// Run `admitted`'s run as an engine's attempt of its own: the engine
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
    /// A run that ends with a refusal no retry changes is ended in the
    /// store by [`Self::execute_admitted_run_step`], as on every shift path
    /// (FIG-4018, FIG-4200).
    async fn execute_engine_run(
        &mut self,
        controller: &ScopedEffectController<'_>,
        admitted: Admitted,
        sinks: &ShiftSinks<'_>,
        close: RunClose<'_>,
    ) -> Result<ExecutedRun, ShiftAbort> {
        let mut attempt = EngineAttempt::enter(self);
        let executed = Box::pin(attempt.runtime.execute_admitted_run_step(
            controller,
            admitted,
            sinks,
            None,
            close,
            crate::store::RunExecutor::Run,
        ))
        .await;
        attempt.returned = true;
        executed
    }

    /// Settle a sealed run whose execution ended with `abort`, on every shift
    /// path (FIG-4018, FIG-4200): the answer the execution returns, with the run
    /// ended in the store when that answer is a refusal no retry changes.
    ///
    /// A superseded commit is such a refusal wherever it is met, whether the
    /// commit or the recorded inspection met it: a redrive replays the
    /// admission base and shift fence its journal recorded (FIG-3682), so it
    /// meets the same moved head and can never commit (FIG-4010). It is
    /// answered `Refused`, as is any other abort an engine would not retry
    /// ([`engine_retries`]). A live fault, a park, and a retryable refusal
    /// are answered as they are, and nothing ends the run: a seal that
    /// ceded never reaches here, and a commit whose outcome is unknown is a
    /// live fault its redrive answers.
    async fn settle_run_abort(
        &self,
        execution: &mut RunExecution,
        abort: ShiftAbort,
    ) -> ShiftAbort {
        let abort = match abort {
            ShiftAbort::Retry(error) if !engine_retries(&error) => ShiftAbort::Refused(error),
            abort => abort,
        };
        let ShiftAbort::Refused(refusal) = &abort else {
            return abort;
        };
        if refusal.is_retryable() {
            return abort;
        }
        match Box::pin(self.end_refused_run(execution, refusal)).await {
            Ok(()) => abort,
            Err(fault) => fault,
        }
    }

    /// Write the end of `run`'s run, whose execution met `refusal`, to the store.
    ///
    /// The write is the refused run's own, made before the run returns, so
    /// an engine's recorded outcome always has its terminal behind it and
    /// the engine's lost-run recovery, which ends only runs that recorded
    /// nothing, never writes a second one. The run's evidence run is the
    /// one ended: a follow-on recovery's is the run that owed the
    /// follow-on. It is an idempotent store write rather than a recorded
    /// step, like the commit and the park (ADR 0105 §9): a run that already
    /// has terminal evidence is left as it is, so a replay that meets the
    /// refusal again writes nothing more.
    ///
    /// The write presents the execution's shift fence, and the store ends the run
    /// only while the run still owns it: a run a later admission superseded
    /// leaves the run to that admission's execution, which may be running
    /// it. A run whose terminal evidence is durable, written now or before,
    /// is marked so, and its scope close follows as a committed run's does.
    /// A store that cannot write it fails the attempt as a live fault, and a
    /// retry meets the refusal again.
    async fn end_refused_run(
        &self,
        execution: &mut RunExecution,
        refusal: &RuntimeError,
    ) -> Result<(), ShiftAbort> {
        let store = self.shift_store()?;
        let end = store
            .end_refused_run(
                &execution.fence,
                &execution.run,
                refusal,
                self.host.core.clock.timestamp_ms(),
            )
            .await
            .map_err(|error| {
                ShiftAbort::Retry(crate::runtime::runtime_error_from_store_commit(error))
            })?;
        match end {
            crate::store::RunEndOutcome::Ended(_) => {
                tracing::info!(
                    session_id = %self.state.session_id,
                    run = %execution.run,
                    code = %refusal.code,
                    event = "run.refused_ended",
                    "a run whose execution met a refusal no retry changes is ended"
                );
                execution.mark_terminal_written();
            }
            crate::store::RunEndOutcome::AlreadyEnded(_) => execution.mark_terminal_written(),
            crate::store::RunEndOutcome::Superseded => {
                tracing::info!(
                    session_id = %self.state.session_id,
                    run = %execution.run,
                    code = %refusal.code,
                    event = "run.refused_superseded",
                    "a refused execution a later admission superseded leaves its run to that admission"
                );
            }
            crate::store::RunEndOutcome::Unknown => {}
        }
        Ok(())
    }

    /// Discard what an earlier run's attempt left on this runtime (its
    /// sealed run, its admitted turn index and the resident session state it
    /// touched), so the next run starts from the
    /// durable session exactly as a redrive in a fresh process does.
    fn discard_run_residue(&mut self) {
        self.discard_attempt_fields();
        self.invalidate_resident_session_state();
    }

    /// Discard the fields a run's attempt keeps only while it runs: its
    /// sealed run and its admitted turn index.
    fn discard_attempt_fields(&mut self) {
        self.shift_run = None;
        self.admitted_turn_index = None;
    }

    /// Seal `admitted`, then run its run.
    ///
    /// Every shift path executes a run through here: the shift loop
    /// ([`Self::work_until`]: a session shift, a queued drain, a direct
    /// turn's accept, a child session's turn) and an engine's own attempt
    /// ([`Self::execute_engine_run`]). A sealed run whose execution ends with a
    /// refusal no retry changes is ended here, typed, before its scope-close
    /// bookkeeping ([`Self::settle_run_abort`]).
    ///
    /// `live` carries the live `TurnContext` of an in-process caller whose
    /// accepted input the run may execute (a child session turn's process
    /// correlation and lineage): it cannot cross the durable boundary, so it
    /// is re-attached when the run's admission executes that input. `close`
    /// says where the run's scope close runs once its terminal evidence is
    /// durable, and `executor` names the execution that executes the run, which
    /// the run's admission records (FIG-4403).
    pub(crate) async fn execute_admitted_run_step(
        &mut self,
        controller: &ScopedEffectController<'_>,
        admitted: Admitted,
        sinks: &ShiftSinks<'_>,
        live: Option<(&crate::InputId, &crate::TurnInput)>,
        close: RunClose<'_>,
        executor: crate::store::RunExecutor,
    ) -> Result<ExecutedRun, ShiftAbort> {
        let store = self.shift_store()?;
        let run = admitted.run().clone();
        // A run executes under its own turn scope (FIG-3607 contract 4), except
        // under a caller whose scope is a process or a runtime operation: a
        // process-backed child turn keeps the process scope it was admitted
        // under, and so does an operation's turn.
        let scope = match controller.execution_scope() {
            crate::ExecutionScope::Process { .. }
            | crate::ExecutionScope::RuntimeOperation { .. } => controller.admitted_scope().clone(),
            _ => shift_run_scope(admitted.session(), &run),
        };
        let host = Arc::clone(&self.host.core.control.effect_host);
        let run_controller = step_controller(controller, host.as_ref(), scope.clone())
            .map_err(ShiftAbort::Refused)?;
        let marked = Box::pin(mark_and_seal_run(
            &run_controller,
            &scope,
            &admitted,
            Some(store),
            executor.clone(),
        ))
        .await;
        let fence = match marked? {
            crate::engine::SealVerdict::Sealed(fence) => fence,
            crate::engine::SealVerdict::Refused(refusal) => {
                return Ok(ExecutedRun {
                    outcome: RunOutcome::Refused { run, refusal },
                    run: None,
                    executed_inputs: Vec::new(),
                    empty_drain: None,
                });
            }
        };
        let execution = RunExecution::sealed(&admitted, fence.clone());
        let evidence_run = execution.run.clone();
        let outer = self.shift_run.replace(Box::new(execution));
        let result = match admitted.work().clone() {
            crate::engine::AdmittedWork::Input { head } => {
                Box::pin(self.execute_run(
                    &run_controller,
                    &admitted,
                    &crate::store::AdmittedHead::Input(head),
                    sinks,
                    live,
                    &fence,
                    executor,
                ))
                .await
            }
            crate::engine::AdmittedWork::Queued { head } => {
                Box::pin(self.execute_run(
                    &run_controller,
                    &admitted,
                    &crate::store::AdmittedHead::Batch(head),
                    sinks,
                    None,
                    &fence,
                    executor,
                ))
                .await
            }
            crate::engine::AdmittedWork::Commands { .. } => {
                Box::pin(self.execute_commands_run(&run_controller, &admitted, &fence)).await
            }
            crate::engine::AdmittedWork::FollowOn {
                follow_on,
                attempts,
            } => {
                Box::pin(self.execute_follow_on_run(
                    &run_controller,
                    &admitted,
                    &run::FollowOnWork {
                        turn: &follow_on,
                        attempts,
                    },
                    sinks,
                    &fence,
                ))
                .await
            }
        };
        let mut ran = std::mem::replace(&mut self.shift_run, outer);
        // A refused run ends here, on every shift path, before the close
        // bookkeeping below reads whether its terminal is durable.
        let result = match (result, ran.as_deref_mut()) {
            (Err(abort), Some(execution)) => {
                Err(Box::pin(self.settle_run_abort(execution, abort)).await)
            }
            (result, _) => result,
        };
        // A committed run's report is handed over before its scope closes
        // (FIG-3979): the close is a recorded step, and the terminal
        // transaction armed its `ScopeClose` obligation, so an execution
        // that dies between the two still closes the scope on its redrive,
        // on the engine's close, or through the obligation's relay.
        if let Ok(executed) = &result
            && let RunOutcome::Committed { run, .. } = &executed.outcome
            && let Some(turn) = executed.run.as_ref().and_then(AgentFrameRun::final_turn)
            // A turn that ended at a segment boundary settled nothing: its
            // executed goes on in the continuation its commit owes (FIG-4739).
            && !matches!(turn.outcome, crate::TurnOutcome::SegmentBoundary { .. })
        {
            sinks
                .settled
                .settled(
                    self,
                    SettledRun {
                        run,
                        turn,
                        executed_inputs: &executed.executed_inputs,
                    },
                )
                .await;
        }
        // The run ended here: its final commit, or the end of its refused
        // run, wrote its evidence, so its scope closes (FIG-3607 item 7). A
        // run that did not end holds its scope open, and a host that owns no
        // scopes has nothing to close. The close runs under the run's own
        // controller, so a process-backed child's run closes in the process
        // scope it was admitted under.
        if ran.is_some_and(|ran| ran.terminal_written)
            && self.host.core.control.scope_close.owns_scopes()
        {
            match close {
                RunClose::Inline => {
                    let terminals = Arc::clone(self.shift_store()?.store());
                    Box::pin(emit_close_step(
                        &self.host.core,
                        terminals,
                        &run_controller,
                        admitted.session(),
                        &evidence_run,
                    ))
                    .await?;
                }
                RunClose::Owed(owed) => *owed = Some(evidence_run),
            }
        }
        result
    }

    fn shift_store(&self) -> Result<crate::store::SessionStore, ShiftAbort> {
        self.session
            .as_ref()
            .and_then(|session| session.history_store())
            .ok_or_else(|| {
                ShiftAbort::Refused(RuntimeError::new(
                    RuntimeErrorCode::QueuedWork,
                    "a session shift requires a durable session store",
                ))
            })
    }
}

/// Where a run's scope close runs once its terminal evidence is durable
/// (FIG-4035).
pub(crate) enum RunClose<'o> {
    /// As the run's own recorded step, after its report is handed over and
    /// before its run returns: the in-process shift admits nothing else in
    /// between.
    Inline,
    /// Owed to the engine, which runs it through [`close_admitted_run`] in
    /// a journal of its own: the run's execution names the run to close here and
    /// returns, so the session's next admission does not wait on the close.
    Owed(&'o mut Option<TurnId>),
}

/// The recorded `CloseRunScope` step of `run`, after its terminal
/// evidence: under `controller`'s run scope at
/// [`shift_close_run_replay_key`](crate::engine::shift_close_run_replay_key),
/// so a redrive of a run that already closed replays the close, and one
/// that crashed before it closes the run again. The body delivers the
/// run's `ScopeClose` obligation, which settles once however many
/// executions deliver it (ADR 0109 §3).
async fn emit_close_step(
    host: &crate::RuntimeHostConfig,
    terminals: Arc<dyn crate::store::RuntimeStore>,
    controller: &ScopedEffectController<'_>,
    session: &crate::SessionId,
    run: &TurnId,
) -> Result<(), ShiftAbort> {
    let invocation = RuntimeEffectInvocation::new(
        EffectAddress::new(
            controller.execution_scope().clone(),
            crate::engine::shift_close_run_replay_key(run),
        )
        .map_err(|error| ShiftAbort::Refused(RuntimeError::from(error)))?,
        RuntimeAttribution::for_turn_admission(session.clone(), run.clone()),
        format!("{run}.shift-close"),
    );
    controller
        .execute_effect(
            RuntimeEffectEnvelope::new(
                invocation,
                RuntimeEffectCommand::CloseRunScope { run: run.clone() },
            ),
            lash_core_execution::core_internal::owned_runner_executor(
                Box::new(close::CloseRunScopeRunner {
                    terminals,
                    session: session.clone(),
                    run: run.clone(),
                    sink: Arc::clone(&host.control.scope_close),
                    relay: Arc::new(
                        scope_close::ScopeCloseRelay::over_backend(
                            host.backend(),
                            host.session_store_factory(),
                            Arc::clone(&host.control.scope_close),
                        )
                        .with_policy(host.control.relay_policy()),
                    ),
                    clock: Arc::clone(&host.clock),
                }),
                None,
            ),
        )
        .await
        .and_then(crate::RuntimeEffectOutcome::into_close_run_scope)
        .map(|_| ())
        .map_err(|error| controller_abort(Some(run), error))
}

/// Draw this execution's start marker in the run's own journal, then seal
/// the admission with it (ADR 0105 §2, L-S8). `store` is the session's
/// history store, or `None` when the engine could not open the session at
/// all — its close or tombstone already committed — in which case the seal's
/// recorded body is the retirement itself (FIG-3881). `executor` is the
/// execution that executes the run, which the seal records in its own
/// transaction (FIG-4814).
async fn mark_and_seal_run(
    run_controller: &ScopedEffectController<'_>,
    scope: &crate::AdmittedScope,
    admitted: &Admitted,
    store: Option<crate::store::SessionStore>,
    executor: crate::store::RunExecutor,
) -> Result<crate::engine::SealVerdict, ShiftAbort> {
    let run = admitted.run().clone();
    // The execution's start marker, drawn in the run's own journal before
    // the seal: a retry replays it, an execution that cannot read the
    // journal draws another, and the seal refuses that one (L-S8).
    let start = RuntimeEffectInvocation::new(
        EffectAddress::new(scope.scope().clone(), shift_run_start_replay_key(admitted))
            .map_err(|error| ShiftAbort::Refused(RuntimeError::from(error)))?,
        RuntimeAttribution::for_turn_admission(admitted.session().clone(), run.clone()),
        format!("{run}.shift-run-start"),
    );
    let run_start = run_controller
        .execute_effect(
            RuntimeEffectEnvelope::new(
                start,
                RuntimeEffectCommand::DrawRunStart { run: run.clone() },
            ),
            lash_core_execution::core_internal::owned_runner_executor(
                Box::new(crate::runtime::run_start::DrawRunStartRunner { run: run.clone() }),
                None,
            ),
        )
        .await
        .and_then(crate::RuntimeEffectOutcome::into_draw_run_start)
        .map_err(|error| controller_abort(Some(&run), error))?;
    let invocation = RuntimeEffectInvocation::new(
        EffectAddress::new(scope.scope().clone(), shift_seal_replay_key(admitted))
            .map_err(|error| ShiftAbort::Refused(RuntimeError::from(error)))?,
        RuntimeAttribution::for_turn_admission(admitted.session().clone(), run.clone()),
        format!("{run}.shift-seal"),
    );
    let verdict = run_controller
        .execute_effect(
            RuntimeEffectEnvelope::new(
                invocation,
                RuntimeEffectCommand::SealShiftAdmission {
                    admitted: Box::new(admitted.clone()),
                },
            ),
            lash_core_execution::core_internal::owned_runner_executor(
                Box::new(admission::SealShiftRunner {
                    store,
                    admitted: admitted.clone(),
                    run_start,
                    executor,
                }),
                None,
            ),
        )
        .await
        .and_then(crate::RuntimeEffectOutcome::into_seal_shift_admission)
        .map_err(|error| controller_abort(Some(&run), error))?;
    Ok(verdict)
}
