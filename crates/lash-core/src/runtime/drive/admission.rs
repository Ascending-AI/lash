//! The recorded bodies of a drive's admission (FIG-3600, ADR 0105 §2): the
//! `AdmitDrive` runner, which decides what the drive runs next and mints its
//! root, and the `SealDriveAdmission` runner, which raises the session's drive
//! epoch for that admission. Both run only inside an engine's recorded step;
//! a replay decodes their verdicts and never runs them.
//!
//! A store that did not answer is the attempt's fault, never a verdict: the
//! runners mark it with derivation retry authority, so an engine runs the
//! step again instead of recording it.

use std::sync::Arc;

use crate::engine::{
    AdmissionId, AdmitRequest, AdmitVerdict, Admitted, AdmittedWork, DriveRequestId, ParkId,
    ParkRef, SealVerdict,
};
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::store::{DriveEpochSeal, SessionHeadRef};
use crate::{
    RuntimeEffectCommand, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectOutcome, RuntimeErrorCode, SessionId, StoreError, TurnId,
};

/// The id one admission is keyed by: its drive request and its ordinal
/// within that request. A redrive of the request names the same admissions,
/// so its seals are idempotent; a new request never reuses one.
pub(super) fn admission_id(request: &DriveRequestId, ordinal: u32) -> AdmissionId {
    AdmissionId::new(format!("{}#{ordinal}", request.as_str()))
}

/// A store fault inside an admission step: the attempt's, never the step's
/// outcome. A session-state generation refusal keeps its typed code.
fn store_fault(context: &str, error: StoreError) -> RuntimeEffectControllerError {
    let mut fault =
        RuntimeEffectControllerError::from(crate::runtime::runtime_error_from_store_commit(error));
    fault.message = format!("{context}: {}", fault.message);
    fault.retryable_uncommitted_derivation()
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

/// The first execution of one `AdmitDrive` step.
///
/// Everything it reads is live store state, which is why it runs only inside
/// the recorded step: the verdict it returns is what every replay decodes.
pub(in crate::runtime) struct AdmitDriveRunner {
    pub(in crate::runtime) store: Arc<dyn crate::store::RuntimePersistence>,
    pub(in crate::runtime) request: AdmitRequest,
    pub(in crate::runtime) ordinal: u32,
    /// The head the root would be admitted on, as the drive refreshed it
    /// right before the step. Its generation is read in the body.
    pub(in crate::runtime) base: SessionHeadRef,
    /// The root's turn index: the next one after `base`.
    pub(in crate::runtime) turn_index: u64,
    /// Decides which queued work is due: a batch made available later is
    /// not work yet.
    pub(in crate::runtime) clock: Arc<dyn crate::Clock>,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for AdmitDriveRunner {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectCommand::AdmitDrive { request } = &envelope.command else {
            return Err(executor_mismatch("drive admission", &envelope));
        };
        if **request != self.request {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "drive admission executor was bound to another drive request",
            ));
        }
        let verdict = self.admit().await?;
        Ok(RuntimeEffectOutcome::AdmitDrive {
            verdict: Box::new(verdict),
        })
    }
}

impl AdmitDriveRunner {
    async fn admit(self) -> Result<AdmitVerdict, RuntimeEffectControllerError> {
        let session_id = &self.request.session;
        // FIG-3619: the session-state generation gate. A generation this
        // build cannot run is refused before anything is admitted.
        let generation = self
            .store
            .read_session_state_version()
            .await
            .map_err(|error| store_fault("session-state generation gate", error))?;
        // FIG-3571 phase 2 (ruling B7): the turn-generation stamp check
        // belongs here, after the generation gate and before the parked-root
        // check, and nowhere else.

        // A parked root blocks the session until it is resolved (FIG-3659).
        // A store with no park ledger holds no park.
        let park = match self.store.load_turn_park(session_id).await {
            Ok(park) => park,
            Err(StoreError::UnsupportedStoreOperation { .. }) => None,
            Err(error) => return Err(store_fault("parked-root check", error)),
        };
        if let Some(park) = park {
            return Ok(AdmitVerdict::Parked(ParkRef {
                session: session_id.clone(),
                root: park.turn_id,
                park: ParkId::new(park.park_id.to_string()),
            }));
        }

        // FIG-3542 (S4): the pending follow-on recovery runs here, at drive
        // admission, before any other work is admitted.

        let Some((root, work)) = self.next_root().await? else {
            return Ok(AdmitVerdict::Idle);
        };
        let epoch = self
            .store
            .drive_epoch(session_id)
            .await
            .map_err(|error| store_fault("drive epoch read", error))?;
        Ok(AdmitVerdict::Admit(
            crate::engine::admission_body::admitted(
                session_id.clone(),
                root,
                self.request.request.clone(),
                admission_id(&self.request.request, self.ordinal),
                epoch.epoch,
                SessionHeadRef {
                    generation,
                    ..self.base
                },
                self.turn_index,
                work,
            ),
        ))
    }

    /// The work this admission drives next, and the root it runs under.
    ///
    /// An unfinished queued run owns the session until it ends, so it is
    /// resumed first. Then the head of the accepted next-turn input, unless a
    /// session command was enqueued before it: commands are applied by the
    /// queued drain, in order. Then any other queued work. The root of an
    /// input is its host id (its source key) when it has one, else its input
    /// id; an input bound to an aborted turn resumes that turn (FIG-3589).
    async fn next_root(
        &self,
    ) -> Result<Option<(TurnId, AdmittedWork)>, RuntimeEffectControllerError> {
        let session_id = &self.request.session;
        if let Some(run) = self
            .store
            .pending_queued_run(session_id)
            .await
            .map_err(|error| store_fault("unfinished queued run read", error))?
        {
            return Ok(Some((TurnId::from(run.scope.id()), AdmittedWork::Queued)));
        }
        let ordering = self
            .store
            .pending_session_work_ordering(session_id)
            .await
            .map_err(|error| store_fault("pending work ordering read", error))?;
        if !ordering.session_command_precedes_turn_input()
            && let Some((root, head)) = self.next_input_root(session_id).await?
        {
            return Ok(Some((root, AdmittedWork::Input { head })));
        }
        let queued = self
            .store
            .list_pending_queued_work(session_id)
            .await
            .map_err(|error| store_fault("pending queued work read", error))?;
        let now = self.clock.timestamp_ms();
        if queued.iter().any(|batch| batch.available_at_ms <= now) {
            return Ok(Some((
                queued_root(&admission_id(&self.request.request, self.ordinal)),
                AdmittedWork::Queued,
            )));
        }
        // A command enqueued before every input was the only reason to skip
        // the input lane: with no queued work left, the input is next.
        Ok(self
            .next_input_root(session_id)
            .await?
            .map(|(root, head)| (root, AdmittedWork::Input { head })))
    }

    async fn next_input_root(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<(TurnId, crate::InputId)>, RuntimeEffectControllerError> {
        let open = self
            .store
            .list_pending_turn_inputs(session_id)
            .await
            .map_err(|error| store_fault("pending turn input read", error))?;
        let head = open
            .iter()
            .filter(|read| read.input.state == crate::TurnInputState::DeferredNextTurn)
            .min_by_key(|read| read.input.enqueue_seq);
        Ok(head.map(|read| {
            let root = match &read.status {
                crate::PendingTurnInputReadStatus::TurnBound { turn_id, .. } => turn_id.clone(),
                _ => input_root(&read.input),
            };
            (root, read.input.input_id.clone())
        }))
    }
}

/// The root of a drive that starts with `input`: the host's id for it (its
/// source key) when it has one, else its input id (FIG-3600, ruling Q4).
pub(in crate::runtime) fn input_root(input: &crate::PendingTurnInput) -> TurnId {
    TurnId::from(
        input
            .source_key
            .as_deref()
            .unwrap_or_else(|| input.input_id.as_str()),
    )
}

/// The root a fresh queued run is admitted under: named by its admission, so
/// no two admissions share a run and a redrive of one names the same run.
fn queued_root(admission: &AdmissionId) -> TurnId {
    TurnId::from(format!("drive-run:{}", admission.as_str()))
}

/// The first execution of one `SealDriveAdmission` step: the drive-epoch
/// compare-and-set, keyed by the admission nonce, so a retried body answers
/// the fence it already raised (ADR 0105 L-S3, L-S4).
pub(in crate::runtime) struct SealDriveRunner {
    pub(in crate::runtime) store: Arc<dyn crate::store::RuntimePersistence>,
    pub(in crate::runtime) admitted: Admitted,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for SealDriveRunner {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectCommand::SealDriveAdmission { admitted } = &envelope.command else {
            return Err(executor_mismatch("drive seal", &envelope));
        };
        if **admitted != self.admitted {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "drive seal executor was bound to another admission",
            ));
        }
        let seal = self
            .store
            .seal_drive_epoch(
                self.admitted.session(),
                self.admitted.admission(),
                self.admitted.observed_epoch(),
            )
            .await
            .map_err(|error| store_fault("drive epoch seal", error))?;
        let verdict = match seal {
            DriveEpochSeal::Sealed(fence) => SealVerdict::Sealed(fence),
            DriveEpochSeal::Superseded { epoch } => SealVerdict::Superseded { epoch },
        };
        Ok(RuntimeEffectOutcome::SealDriveAdmission {
            verdict: Box::new(verdict),
        })
    }
}
