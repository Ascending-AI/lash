//! The recorded bodies of a shift's admission (FIG-3600, ADR 0105 §2): the
//! `AdmitShift` runner, which decides what the shift runs next and mints its
//! run, and the `SealShiftAdmission` runner, which raises the session's shift
//! epoch for that admission. Both run only inside an engine's recorded step;
//! a replay decodes their verdicts and never runs them.
//!
//! A store that did not answer is the attempt's fault, never a verdict: the
//! runners mark it with derivation retry authority, so an engine runs the
//! step again instead of recording it. The session's own retirement is the
//! exception: it is a settled fact the step records like a verdict, never a
//! derivation a rerun could answer differently (FIG-3630).

use std::sync::Arc;

use crate::engine::{
    AdmissionId, AdmitRequest, AdmitVerdict, Admitted, AdmittedWork, ParkRef, SealRefusal,
    SealVerdict, ShiftRequestId,
};
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::store::{ShiftEpochSeal, StoredShiftEpoch};
use crate::{
    RuntimeEffectCommand, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectOutcome, RuntimeErrorCode, StoreError, TurnId,
};

/// The id one admission is keyed by: its shift request and its ordinal
/// within that request. A redrive of the request names the same admissions,
/// so its seals are idempotent; a new request never reuses one.
pub(super) fn admission_id(request: &ShiftRequestId, ordinal: u32) -> AdmissionId {
    AdmissionId::new(format!("{}#{ordinal}", request.as_str()))
}

/// A store fault inside an admission step: the attempt's, never the step's
/// outcome. A session-state generation refusal keeps its typed code.
pub(super) fn store_fault(context: &str, error: StoreError) -> RuntimeEffectControllerError {
    let mut fault =
        RuntimeEffectControllerError::from(crate::runtime::runtime_error_from_store_commit(error));
    fault.message = format!("{context}: {}", fault.message);
    fault.retryable_uncommitted_derivation()
}

/// Record corrupt stored data an admission met as the session's fault
/// (ADR 0109 §9), then answer `error`: the admission may follow a run whose
/// answer is already published, so no sender is left to hear the refusal. A
/// session already faulted keeps its first fault, so the refusal a standing
/// fault makes records nothing new. A fault that could not be recorded is the
/// attempt's, so the engine runs the step again.
async fn record_fault(
    stores: Arc<dyn crate::DeploymentStore>,
    clock: Arc<dyn crate::Clock>,
    session: &crate::SessionId,
    error: RuntimeEffectControllerError,
) -> RuntimeEffectControllerError {
    if error.code != RuntimeErrorCode::RuntimeStoreCorrupt {
        return error;
    }
    let record = crate::store::SessionFaultRecord::new(
        crate::store::SessionFaultOrigin::DriveAdmission,
        &error.clone().into_runtime_error(),
    );
    match stores
        .record_session_fault(session, &record, clock.timestamp_ms())
        .await
    {
        Ok(_) | Err(StoreError::UnsupportedStoreOperation { .. }) => error,
        Err(unrecorded) => store_fault("session fault record", unrecorded),
    }
}

fn executor_mismatch(
    expected: &str,
    envelope: &RuntimeEffectEnvelope,
) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
        format!(
            "{expected} executor cannot execute {} command",
            envelope.command.kind().as_str()
        ),
    )
}

/// The first execution of one `AdmitShift` step.
///
/// Everything it reads is live store state, which is why it runs only inside
/// the recorded step: the verdict it returns is what every replay decodes.
pub(in crate::runtime) struct AdmitShiftRunner {
    /// The session's history store and the host clock a fault it records is
    /// stamped with, or `None` when the engine could not open the store at
    /// all — the session's tombstone already committed — in which case the
    /// step's recorded body is the retirement itself.
    pub(in crate::runtime) store: Option<(crate::store::SessionStore, Arc<dyn crate::Clock>)>,
    /// The deployment's control-intent ledger: a park names its redrive by
    /// intent id, and whether that redrive is settled lives here (D15).
    pub(in crate::runtime) stores: Arc<dyn crate::DeploymentStore>,
    pub(in crate::runtime) request: AdmitRequest,
    pub(in crate::runtime) ordinal: u32,
    /// The drain this admission hands over for, when its shift named one
    /// (FIG-4639).
    pub(in crate::runtime) drain: Option<DrainRead>,
    /// The execution that runs the runs this shift admits: an unfinished
    /// run recorded under an executor that excludes it is left to that
    /// executor (FIG-4765).
    pub(in crate::runtime) executor: crate::store::RunExecutor,
}

/// The drain mark an admission reads before it admits (ADR 0106 §1): the
/// store's marks, and the build generation its shift's invocation is pinned
/// to.
pub(in crate::runtime) struct DrainRead {
    pub(in crate::runtime) marks: Arc<dyn crate::store::generation_drain::GenerationDrainStore>,
    pub(in crate::runtime) generation: crate::engine::BuildGeneration,
}

impl DrainRead {
    /// Whether an operator marked the generation draining: the same mark the
    /// recovery leader's hand-over duty and the drain status read.
    async fn marked(&self) -> Result<bool, RuntimeEffectControllerError> {
        Ok(self
            .marks
            .draining_generations()
            .await
            .map_err(|error| store_fault("generation drain mark read", error))?
            .iter()
            .any(|marked| marked.generation == self.generation))
    }
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for AdmitShiftRunner {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectCommand::AdmitShift { request } = &envelope.command else {
            return Err(executor_mismatch("shift admission", &envelope));
        };
        if **request != self.request {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "shift admission executor was bound to another shift request",
            ));
        }
        let session = self.request.session.clone();
        let stores = Arc::clone(&self.stores);
        let clock = self.store.as_ref().map(|(_, clock)| Arc::clone(clock));
        let verdict = match (self.admit().await, clock) {
            (Ok(verdict), _) => verdict,
            (Err(error), Some(clock)) => {
                return Err(record_fault(stores, clock, &session, error).await);
            }
            (Err(error), None) => return Err(error),
        };
        Ok(RuntimeEffectOutcome::AdmitShift {
            verdict: Box::new(verdict),
        })
    }
}

impl AdmitShiftRunner {
    async fn admit(self) -> Result<AdmitVerdict, RuntimeEffectControllerError> {
        let session_id = &self.request.session;
        // An engine whose attempt opens no store — the session was deleted
        // between an earlier attempt's journaled step and this redrive —
        // still emits this step, and its recorded body is the retirement
        // itself: a settled fact, not a fault a rerun could answer.
        let Some((store, _)) = self.store.clone() else {
            return Err(store_fault(
                "session store open",
                StoreError::SessionDeleted {
                    session_id: session_id.clone(),
                },
            ));
        };
        // FIG-3619: the session-state generation gate. A generation this
        // build cannot run is refused before anything is admitted.
        store
            .read_session_state_version()
            .await
            .map_err(|error| store_fault("session-state generation gate", error))?;
        // FIG-3571 phase 2 (ruling B7): the turn-generation stamp check
        // belongs here, after the generation gate and before the parked-run
        // check, and nowhere else.

        // A closing session admits nothing (FIG-3600 S7): its close ended
        // every run and raised the epoch past every admission. A store with
        // no shift epoch holds no close; the admission below still needs one.
        let stored_epoch = match store.shift_epoch().await {
            Ok(epoch) if epoch.closing.is_some() || epoch.control_pending => {
                return Ok(AdmitVerdict::Idle);
            }
            // A faulted session admits nothing (ADR 0109 §9): the shift is
            // refused with the fault's own code and cause until an operator
            // clears it.
            Ok(StoredShiftEpoch {
                fault: Some(fault), ..
            }) => {
                return Err(RuntimeEffectControllerError::from(
                    fault.record.runtime_error(),
                ));
            }
            Ok(epoch) => Ok(epoch),
            Err(
                error @ (StoreError::ShiftEpochUnavailable { .. }
                | StoreError::UnsupportedStoreOperation { .. }),
            ) => Err(error),
            Err(error) => return Err(store_fault("session close check", error)),
        };

        // A parked run blocks the session until it is resolved (FIG-3659).
        // A store with no park ledger holds no park.
        let park = match store.load_turn_park().await {
            Ok(park) => park,
            Err(StoreError::UnsupportedStoreOperation { .. }) => None,
            Err(error) => return Err(store_fault("parked-run check", error)),
        };
        if let Some(park) = park.as_ref()
            && park.resume_intent.is_none()
        {
            return Ok(AdmitVerdict::Parked(ParkRef {
                session: session_id.clone(),
                run: park.turn_id.clone(),
                park: park.park_id,
            }));
        }
        // D15: a park that names a redrive intent is being resolved. While
        // that intent's engine half is still owed, a new turn input is
        // refused retryably,
        // never interleaved with the redrive and never a recorded verdict.
        // A ledger that cannot name intents holds no unsettled redrive.
        let redrive = park.as_ref().and_then(|park| park.resume_intent);
        let redrive_unsettled = match redrive {
            Some(intent) => match self.stores.load_intent(intent).await {
                Ok(intent) => intent.is_some_and(|intent| intent.engine_half_owed()),
                Err(StoreError::UnsupportedStoreOperation { .. }) => false,
                Err(error) => return Err(store_fault("redrive intent read", error)),
            },
            None => false,
        };

        let Some((run, work)) = self.next_run(&store).await? else {
            return Ok(AdmitVerdict::Idle);
        };
        // A shift whose build is draining admits no further run (FIG-4639):
        // the work found here is the newest build's. The mark is read only
        // once there is work to hand over, and the verdict records it, so a
        // replay hands over where this execution did.
        if let Some(drain) = &self.drain
            && drain.marked().await?
        {
            return Ok(AdmitVerdict::Draining {
                generation: drain.generation.clone(),
            });
        }
        // The parked run is bound to the session and owns its head
        // (FIG-4202): while its redrive is unsettled, nothing is admitted
        // ahead of it, session commands included, and a turn input waits for
        // it too. The refusal is the attempt's — never a recorded verdict —
        // so the engine's retry re-decides admission after the redrive
        // settles.
        let parked_run = park.as_ref().is_some_and(|park| park.turn_id == run);
        if redrive_unsettled && (parked_run || matches!(work, AdmittedWork::Input { .. })) {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::SessionRedriveUnsettled,
                format!(
                    "session `{session_id}` admits no turn input while the parked \
                     run names unsettled redrive intent `{intent}`",
                    intent = redrive.map(|id| id.to_string()).unwrap_or_default()
                ),
            )
            .retryable_uncommitted_derivation());
        }
        // A head input whose run already ended is answered from the run's
        // evidence, never run again (ADR 0105 L-S6, FIG-3600 S7): acceptance
        // keeps such an input out, so this is the defensive answer.
        if matches!(work, AdmittedWork::Input { .. })
            && let Some(terminal) = store
                .run_terminal(&run)
                .await
                .map_err(|error| store_fault("run terminal read", error))?
        {
            return Ok(AdmitVerdict::RunTerminal {
                commit: terminal.commit().cloned(),
                kind: terminal.kind(),
                run,
            });
        }
        let epoch = stored_epoch.map_err(|error| store_fault("shift epoch read", error))?;
        Ok(AdmitVerdict::Admit(
            crate::engine::admission_body::admitted(
                session_id.clone(),
                run,
                self.request.request.clone(),
                admission_id(&self.request.request, self.ordinal),
                epoch.epoch,
                self.request.build_generation.clone(),
                work,
            ),
        ))
    }

    /// Refuse the attempt while the session's unfinished run is another
    /// executor's to run (FIG-4765).
    ///
    /// The run's recorded executor decides who runs it. When an engine
    /// holds that executor's run, the engine redrives it and answers for it
    /// if it is lost, so another engine-held run, an acceptor executing its
    /// child session's turn inline included, admits nothing beside it,
    /// whatever ingress claim lapsed in between. A session shift or queue
    /// drain no engine holds resumes the run as before, under the shift
    /// fence. The refusal is the attempt's, never a recorded verdict: the
    /// engine's retry re-decides admission once that execution has ended
    /// the run. The seal makes the same decision in its own transaction
    /// ([`held_by_another_executor`]), for a run sealed since this read.
    fn leave_to_recorded_executor(
        &self,
        unfinished: Option<&crate::store::UnfinishedRun>,
    ) -> Result<(), RuntimeEffectControllerError> {
        match unfinished {
            Some(held) if held.executor.excludes(&self.executor) => Err(held_by_another_executor(
                &self.request.session,
                &held.run,
                &held.executor,
            )),
            _ => Ok(()),
        }
    }

    /// The work this admission executes next, and the run it runs under.
    ///
    /// A follow-on the head owes comes first (ADR 0101 §3, FIG-3542): while
    /// it is owed every other admission is blocked, so it is recovered before
    /// anything else is admitted. It always belongs to the unfinished run (a
    /// frame switch's commit ends no run), and its recovery's final commit
    /// ends that run. Then the session's unfinished run, which owns the
    /// session until it ends, resumed under its own id (FIG-3927). Both are
    /// left to the run's recorded executor when it excludes this shift's
    /// (FIG-4765). Then the
    /// command lane: open session commands apply before any turn-lane
    /// work (ADR 0101 §4). Then the turn lane in `enqueue_seq` order across
    /// both admission tables, with no kind priority (ADR 0101 §5): the head
    /// next-turn input, or the queued turn work pending before it. The run
    /// of an input is its host id (its source key) when it has one, else its
    /// input id; a queued-work head's run and a command run are named by
    /// this admission. An unfinished run owns the session head, so no
    /// command applies while it is bound, a parked run awaiting its
    /// redrive included (FIG-4202).
    async fn next_run(
        &self,
        store: &crate::store::SessionStore,
    ) -> Result<Option<(TurnId, AdmittedWork)>, RuntimeEffectControllerError> {
        if let Some(owed) = store
            .load_pending_follow_on()
            .await
            .map_err(|error| store_fault("pending follow-on read", error))?
        {
            // The follow-on belongs to the unfinished run, and so does its
            // recovery.
            self.leave_to_recorded_executor(
                store
                    .unfinished_run()
                    .await
                    .map_err(|error| store_fault("unfinished run read", error))?
                    .as_ref(),
            )?;
            return Ok(Some((
                owed.recovery_run(),
                AdmittedWork::FollowOn {
                    follow_on: owed.follow_on_turn_id,
                    attempts: owed.attempts,
                },
            )));
        }
        let unfinished = store
            .unfinished_run()
            .await
            .map_err(|error| store_fault("unfinished run read", error))?;
        self.leave_to_recorded_executor(unfinished.as_ref())?;
        let unfinished = unfinished.map(|unfinished| {
            let work = match unfinished.head {
                crate::store::AdmittedHead::Input(head) => AdmittedWork::Input { head },
                crate::store::AdmittedHead::Batch(head) => AdmittedWork::Queued { head },
            };
            (unfinished.run, work)
        });
        if unfinished.is_some() {
            return Ok(unfinished);
        }
        let admission = admission_id(&self.request.request, self.ordinal);
        let ordering = store
            .pending_session_work_ordering()
            .await
            .map_err(|error| store_fault("pending work ordering read", error))?;
        let queued = store
            .list_queued_work()
            .await
            .map_err(|error| store_fault("open queued work read", error))?;
        if let Some(command) = ordering.session_command {
            // A host task at the head of the lane is a tool-bearing
            // operation: it runs as its own Run, named by the operation, so
            // a redrive admits the same run (K8, binding Q2). Every other
            // command applies in the lane's command run.
            if let Some(operation) = leading_operation(&queued, command.enqueue_seq) {
                return Ok(Some((
                    crate::tool_run::OperationRun {
                        session_id: self.request.session.clone(),
                        operation_id: operation.to_string(),
                    }
                    .run_id(),
                    AdmittedWork::Operation { operation },
                )));
            }
            return Ok(Some((
                commands_run(&admission),
                AdmittedWork::Commands {
                    head: command.enqueue_seq,
                },
            )));
        }
        let open = store
            .list_pending_turn_inputs()
            .await
            .map_err(|error| store_fault("pending turn input read", error))?;
        match lash_core_execution::runtime::turn_lane_head(&open, &queued) {
            None => Ok(None),
            Some(lash_core_execution::runtime::TurnLaneHead::Queued(head)) => Ok(Some((
                queued_run(&admission),
                AdmittedWork::Queued {
                    head: head.batch_id.clone(),
                },
            ))),
            Some(lash_core_execution::runtime::TurnLaneHead::Input(head)) => {
                // A run the input is bound to executes it: the run whose
                // admission took it, or the new run a fork bound it to
                // (FIG-3600 S7). Then its host id.
                let bound = store
                    .run_binding(&head.input.input_id)
                    .await
                    .map_err(|error| store_fault("input run binding read", error))?;
                Ok(Some((
                    lash_core_execution::runtime::head_input_run(head, bound),
                    AdmittedWork::Input {
                        head: head.input.input_id.clone(),
                    },
                )))
            }
        }
    }
}

/// The refusal of an admission or seal whose run `recorded` runs: the
/// attempt's, retried until that executor has ended the run (FIG-4765,
/// FIG-4814).
fn held_by_another_executor(
    session_id: &crate::SessionId,
    run: &TurnId,
    recorded: &crate::store::RunExecutor,
) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        RuntimeErrorCode::SessionRunPending,
        format!(
            "session `{session_id}` admits nothing beside run `{run}`, which is run by \
             {recorded:?}; that execution ends the run"
        ),
    )
    .retryable_uncommitted_derivation()
}

/// The run a queued-work head is admitted under: named by its admission, so
/// no two admissions share a run and a redrive of one names the same run.
fn queued_run(admission: &AdmissionId) -> TurnId {
    TurnId::prefixed("shift-run:", admission.as_str())
}

/// The run an admission of the command lane applies it under.
fn commands_run(admission: &AdmissionId) -> TurnId {
    TurnId::prefixed("shift-commands:", admission.as_str())
}

/// The host operation the command lane's leading open command at
/// `enqueue_seq` is, when it is one: a host's plugin task.
fn leading_operation(
    queued: &[crate::QueuedWorkBatch],
    enqueue_seq: u64,
) -> Option<crate::BatchId> {
    queued
        .iter()
        .find(|batch| batch.enqueue_seq == enqueue_seq && batch.terminal.is_none())
        .filter(|batch| {
            matches!(
                &batch.payload,
                crate::QueuedWorkPayload::SessionCommand { command }
                    if matches!(**command, crate::SessionCommand::RunPluginTask { .. })
            )
        })
        .map(|batch| batch.batch_id.clone())
}

/// The first execution of one `SealShiftAdmission` step: the shift-epoch
/// compare-and-set, keyed by the admission nonce, so a retried body answers
/// the fence it already raised (ADR 0105 L-S3, L-S4). It stores the start
/// marker the run's execution drew, so another execution of the same
/// admission is answered `SubstrateLost` (L-S8), and in the same transaction
/// the executor that executes the run (FIG-4814). A run another engine-held
/// executor holds is left to it: the step raises nothing and fails
/// retryably, recording no verdict, until that executor has ended the run.
pub(in crate::runtime) struct SealShiftRunner {
    /// The session's history store, or `None` when the engine could not open
    /// it at all — the session's close or tombstone already committed — in
    /// which case the step's recorded body is the retirement itself
    /// (FIG-3881).
    pub(in crate::runtime) store: Option<crate::store::SessionStore>,
    pub(in crate::runtime) admitted: Admitted,
    pub(in crate::runtime) run_start: crate::engine::RunStartNonce,
    /// The execution that runs the admitted run.
    pub(in crate::runtime) executor: crate::store::RunExecutor,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for SealShiftRunner {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectCommand::SealShiftAdmission { admitted } = &envelope.command else {
            return Err(executor_mismatch("shift seal", &envelope));
        };
        if **admitted != self.admitted {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "shift seal executor was bound to another admission",
            ));
        }
        let Some(store) = self.store.as_ref() else {
            return Err(store_fault(
                "session store open",
                StoreError::SessionDeleted {
                    session_id: self.admitted.session().clone(),
                },
            ));
        };
        let seal = store
            .seal_shift_epoch(
                self.admitted.admission(),
                self.admitted.observed_epoch(),
                &self.run_start,
                Some(&crate::store::RunHold {
                    run: self.admitted.run().clone(),
                    executor: self.executor.clone(),
                }),
            )
            .await
            .map_err(|error| store_fault("shift epoch seal", error))?;
        let verdict = match seal {
            ShiftEpochSeal::Sealed(fence) => SealVerdict::Sealed(fence),
            ShiftEpochSeal::Superseded { epoch } => {
                SealVerdict::Refused(SealRefusal::Superseded { epoch })
            }
            ShiftEpochSeal::ExecutionLost => SealVerdict::Refused(SealRefusal::ExecutionLost),
            ShiftEpochSeal::HeldByAnotherExecutor { run, recorded } => {
                return Err(held_by_another_executor(
                    self.admitted.session(),
                    &run,
                    &recorded,
                ));
            }
        };
        Ok(RuntimeEffectOutcome::SealShiftAdmission {
            verdict: Box::new(verdict),
        })
    }
}
