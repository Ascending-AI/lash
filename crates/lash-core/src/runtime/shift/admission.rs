//! The recorded body of atomic root admission. Read-only proposals precede
//! the transaction; selection and composition are revalidated exactly before
//! any seal, binding, or retained receipt becomes visible.

use std::sync::Arc;

use crate::engine::{
    AdmissionId, AdmitRequest, AdmitVerdict, Admitted, AdmittedWork, ParkRef, ShiftRequestId,
};
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::store::ShiftEpochSeal;
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
    pub(in crate::runtime) run_scope: Option<crate::AdmittedScope>,
    pub(in crate::runtime) materializer: Option<Arc<dyn super::ShiftAdmissionMaterializer>>,
    pub(in crate::runtime) tracing: crate::trace::TraceRuntime,
    pub(in crate::runtime) live: Option<Arc<crate::trace::LiveStep>>,
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
    fn bind_live_step(&mut self, live: Arc<crate::trace::LiveStep>) {
        self.live = Some(live);
    }

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

        let identity = admission_id(&self.request.request, self.ordinal);
        if let Some(mut receipt) = store
            .read_shift_admission(&identity)
            .await
            .map_err(|error| store_fault("root admission read", error))?
        {
            if receipt.run_start != self.request.run_start {
                receipt.seal = ShiftEpochSeal::ExecutionLost;
            }
            return Ok(AdmitVerdict::Admit(self.mint(receipt)));
        }
        let preparation = store
            .prepare_shift_admission(&identity, &self.executor)
            .await
            .map_err(|error| match error {
                StoreError::RunHeldByAnotherExecutor { run, recorded, .. } => {
                    held_by_another_executor(session_id, &run, &recorded)
                }
                error => store_fault("root selection preparation", error),
            })?;
        if preparation.epoch.closing.is_some() || preparation.epoch.control_pending {
            return Ok(AdmitVerdict::Idle);
        }
        if let Some(fault) = &preparation.epoch.fault {
            return Err(RuntimeEffectControllerError::from(
                fault.record.runtime_error(),
            ));
        }
        let park = preparation.park.as_ref();
        if let Some(park) = park
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
        let redrive = park.and_then(|park| park.resume_intent);
        let redrive_unsettled = match redrive {
            Some(intent) => match self.stores.load_intent(intent).await {
                Ok(intent) => intent.is_some_and(|intent| intent.engine_half_owed()),
                Err(StoreError::UnsupportedStoreOperation { .. }) => false,
                Err(error) => return Err(store_fault("redrive intent read", error)),
            },
            None => false,
        };

        let Some(selection) = preparation.selection.as_ref() else {
            return Ok(AdmitVerdict::Idle);
        };
        let run = selection.run.clone();
        let work = selection.work.clone();
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
        let parked_run = park.is_some_and(|park| park.turn_id == run);
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
        let prepared = if matches!(
            work,
            AdmittedWork::Input { .. } | AdmittedWork::Queued { .. }
        ) {
            let materializer = self.materializer.as_ref().ok_or_else(|| {
                RuntimeEffectControllerError::new(
                    RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                    "root lacks its admission materializer",
                )
            })?;
            let request = materializer
                .request(
                    &store,
                    selection,
                    &self.request.build_generation,
                    &preparation,
                    self.executor.clone(),
                    &self.run_scope.clone().unwrap_or_else(|| {
                        crate::engine::shift_run_scope(session_id, &selection.run)
                    }),
                )
                .await?;
            Some(
                store
                    .prepare_run_admission(&request)
                    .await
                    .map_err(|error| store_fault("root composition preparation", error))?
                    .ok_or_else(|| {
                        RuntimeEffectControllerError::new(
                            RuntimeErrorCode::SessionExecutionLaneBusy,
                            "root composition changed during preparation",
                        )
                        .retryable_uncommitted_derivation()
                    })?,
            )
        } else {
            None
        };
        let candidate = prepared.as_ref().and_then(|prepared| {
            prepared.admission.trace.as_ref().map(|scope| {
                prepared
                    .request
                    .trace_scopes
                    .propose(&scope.scope, &scope.cause)
            })
        });
        let anchor = candidate
            .as_ref()
            .map_or(crate::TraceAnchor::Untraced, |candidate| candidate.anchor());
        let result = store
            .commit_shift_admission(
                &crate::store::ShiftAdmissionWrite {
                    session_id: session_id.clone(),
                    admission: identity,
                    run_start: self.request.run_start.clone(),
                    executor: self.executor.clone(),
                    preparation,
                    run: prepared,
                },
                &anchor,
            )
            .await;
        if let Some(candidate) = candidate {
            candidate.settle(match &result {
                Ok(receipt) if receipt.run_admission.as_ref().is_some_and(|answer| matches!(answer, crate::store::RunAdmissionAnswer::Admitted { admission, .. } if admission.recorded_by_this_call)) => crate::TraceCandidateOutcome::Selected,
                Ok(_) => crate::TraceCandidateOutcome::Reused,
                Err(_) => crate::TraceCandidateOutcome::Refused,
            });
        }
        let receipt = result.map_err(|error| store_fault("atomic root admission", error))?;
        if let (Some(live), Some(crate::store::RunAdmissionAnswer::Admitted { admission, .. })) =
            (&self.live, &receipt.run_admission)
            && admission.recorded_by_this_call
        {
            self.tracing
                .body(admission.trace.clone(), live)
                .observe(|| {
                    let causes = admission
                        .queued
                        .as_ref()
                        .map(|queued| queued.materialize_queued_checkpoint_work().turn_causes)
                        .unwrap_or_default();
                    (
                        lash_trace::TraceContext::default()
                            .for_session(session_id.clone())
                            .for_turn_index(admission.turn_index as usize)
                            .for_turn(receipt.selection.run.clone()),
                        lash_trace::TraceEvent::Custom {
                            name: "ingress.admitted".to_string(),
                            payload: crate::runtime::turn_loop::ingress_admitted_trace_payload(
                                &receipt.selection.run,
                                crate::store::RUN_ADMISSION_STEP,
                                crate::AdmissionBoundary::Idle,
                                admission.inputs.as_deref(),
                                admission.queued.as_deref(),
                                &causes,
                            ),
                        },
                    )
                });
        }
        if let ShiftEpochSeal::HeldByAnotherExecutor { run, recorded } = &receipt.seal {
            return Err(held_by_another_executor(session_id, run, recorded));
        }
        Ok(AdmitVerdict::Admit(self.mint(receipt)))
    }

    fn mint(&self, receipt: crate::store::ShiftAdmissionReceipt) -> Admitted {
        crate::engine::admission_body::admitted(
            self.request.session.clone(),
            self.request.request.clone(),
            admission_id(&self.request.request, self.ordinal),
            self.request.build_generation.clone(),
            receipt,
        )
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
