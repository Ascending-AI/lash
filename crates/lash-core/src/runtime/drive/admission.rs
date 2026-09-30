//! The recorded bodies of a drive's admission (FIG-3600, ADR 0105 §2): the
//! `AdmitDrive` runner, which decides what the drive runs next and mints its
//! root, and the `SealDriveAdmission` runner, which raises the session's drive
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
    AdmissionId, AdmitRequest, AdmitVerdict, Admitted, AdmittedWork, DriveRequestId, ParkRef,
    SealVerdict,
};
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::store::DriveEpochSeal;
use crate::{
    RuntimeEffectCommand, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectOutcome, RuntimeErrorCode, StoreError, TurnId,
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
    /// The session's history store, or `None` when the engine could not open
    /// it at all — the session's tombstone already committed — in which case
    /// the step's recorded body is the retirement itself.
    pub(in crate::runtime) store: Option<crate::store::SessionStore>,
    /// The deployment's control-intent ledger: a park names its redrive by
    /// intent id, and whether that redrive is settled lives here (D15).
    pub(in crate::runtime) stores: Arc<dyn crate::DeploymentStore>,
    pub(in crate::runtime) request: AdmitRequest,
    pub(in crate::runtime) ordinal: u32,
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
        // An engine whose attempt opens no store — the session was deleted
        // between an earlier attempt's journaled step and this redrive —
        // still emits this step, and its recorded body is the retirement
        // itself: a settled fact, not a fault a rerun could answer.
        let Some(store) = self.store.clone() else {
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
        // belongs here, after the generation gate and before the parked-root
        // check, and nowhere else.

        // A closing session admits nothing (FIG-3600 S7): its close ended
        // every root and raised the epoch past every admission. A store with
        // no drive epoch holds no close; the admission below still needs one.
        let stored_epoch = match store.drive_epoch().await {
            Ok(epoch) if epoch.closing.is_some() || epoch.control_pending => {
                return Ok(AdmitVerdict::Idle);
            }
            Ok(epoch) => Ok(epoch),
            Err(
                error @ (StoreError::DriveEpochUnavailable { .. }
                | StoreError::UnsupportedStoreOperation { .. }),
            ) => Err(error),
            Err(error) => return Err(store_fault("session close check", error)),
        };

        // A parked root blocks the session until it is resolved (FIG-3659).
        // A store with no park ledger holds no park.
        let park = match store.load_turn_park().await {
            Ok(park) => park,
            Err(StoreError::UnsupportedStoreOperation { .. }) => None,
            Err(error) => return Err(store_fault("parked-root check", error)),
        };
        if let Some(park) = park.as_ref()
            && park.resume_intent.is_none()
        {
            return Ok(AdmitVerdict::Parked(ParkRef {
                session: session_id.clone(),
                root: park.turn_id.clone(),
                park: park.park_id,
            }));
        }
        // D15: a park that names a redrive intent is being resolved. While
        // that intent is unsettled — pending, or failed retryably with its
        // engine half still owed — a new turn input is refused retryably,
        // never interleaved with the redrive and never a recorded verdict.
        // A ledger that cannot name intents holds no unsettled redrive.
        let redrive = park.as_ref().and_then(|park| park.resume_intent);
        let redrive_unsettled = match redrive {
            Some(intent) => match self.stores.load_intent(intent).await {
                Ok(intent) => intent.is_some_and(|intent| intent.state.is_open()),
                Err(StoreError::UnsupportedStoreOperation { .. }) => false,
                Err(error) => return Err(store_fault("redrive intent read", error)),
            },
            None => false,
        };

        let Some((root, work)) = self.next_root(&store, redrive_unsettled).await? else {
            return Ok(AdmitVerdict::Idle);
        };
        // The command lane drains first (ADR 0101 §4): commands, queued work
        // and an owed follow-on are admitted while the redrive is unsettled;
        // a turn input and the parked root itself wait for it. The refusal is
        // the attempt's — never a recorded verdict — so the engine's retry
        // re-decides admission after the redrive settles.
        let parked_root = park.as_ref().is_some_and(|park| park.turn_id == root);
        if redrive_unsettled && (parked_root || matches!(work, AdmittedWork::Input { .. })) {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::SessionRedriveUnsettled,
                format!(
                    "session `{session_id}` admits no turn input while the parked \
                     root names unsettled redrive intent `{intent}`",
                    intent = redrive.map(|id| id.to_string()).unwrap_or_default()
                ),
            )
            .retryable_uncommitted_derivation());
        }
        // A head input whose root already ended is answered from the root's
        // evidence, never run again (ADR 0105 L-S6, FIG-3600 S7): acceptance
        // keeps such an input out, so this is the defensive answer.
        if matches!(work, AdmittedWork::Input { .. })
            && let Some(terminal) = store
                .root_terminal(&root)
                .await
                .map_err(|error| store_fault("root terminal read", error))?
        {
            return Ok(AdmitVerdict::RootTerminal {
                commit: terminal.commit().cloned(),
                kind: terminal.kind,
                root,
            });
        }
        let epoch = stored_epoch.map_err(|error| store_fault("drive epoch read", error))?;
        Ok(AdmitVerdict::Admit(
            crate::engine::admission_body::admitted(
                session_id.clone(),
                root,
                self.request.request.clone(),
                admission_id(&self.request.request, self.ordinal),
                epoch.epoch,
                self.request.build_generation.clone(),
                work,
            ),
        ))
    }

    /// The work this admission drives next, and the root it runs under.
    ///
    /// A follow-on the head owes comes first (ADR 0101 §3, FIG-3542): while
    /// it is owed every other admission is blocked, so it is recovered before
    /// anything else is admitted. It always belongs to the unfinished root (a
    /// frame switch's commit ends no root), and its recovery's final commit
    /// ends that root. Then the session's unfinished root, which owns the
    /// session until it ends, resumed under its own id (FIG-3927). Then the
    /// command lane: open session commands apply before any turn-lane
    /// work (ADR 0101 §4). Then the turn lane in `enqueue_seq` order across
    /// both admission tables, with no kind priority (ADR 0101 §5): the head
    /// next-turn input, or the queued turn work pending before it. The root
    /// of an input is its host id (its source key) when it has one, else its
    /// input id; a queued-work head's root and a command root are named by
    /// this admission. While the parked root's redrive is unsettled the
    /// command lane drains ahead of that root, which waits for its redrive.
    async fn next_root(
        &self,
        store: &crate::store::SessionStore,
        redrive_unsettled: bool,
    ) -> Result<Option<(TurnId, AdmittedWork)>, RuntimeEffectControllerError> {
        if let Some(owed) = store
            .load_pending_follow_on()
            .await
            .map_err(|error| store_fault("pending follow-on read", error))?
        {
            return Ok(Some((
                owed.recovery_root(),
                AdmittedWork::FollowOn {
                    follow_on: owed.follow_on_turn_id,
                    attempts: owed.attempts,
                },
            )));
        }
        let unfinished = store
            .unfinished_root()
            .await
            .map_err(|error| store_fault("unfinished root read", error))?
            .map(|unfinished| {
                let work = match unfinished.head {
                    crate::store::AdmittedHead::Input(head) => AdmittedWork::Input { head },
                    crate::store::AdmittedHead::Batch(head) => AdmittedWork::Queued { head },
                };
                (unfinished.root, work)
            });
        if unfinished.is_some() && !redrive_unsettled {
            return Ok(unfinished);
        }
        let admission = admission_id(&self.request.request, self.ordinal);
        let ordering = store
            .pending_session_work_ordering()
            .await
            .map_err(|error| store_fault("pending work ordering read", error))?;
        if let Some(command) = ordering.session_command {
            return Ok(Some((
                commands_root(&admission),
                AdmittedWork::Commands {
                    head: command.enqueue_seq,
                },
            )));
        }
        if unfinished.is_some() {
            return Ok(unfinished);
        }
        let queued = store
            .list_queued_work()
            .await
            .map_err(|error| store_fault("open queued work read", error))?;
        let open = store
            .list_pending_turn_inputs()
            .await
            .map_err(|error| store_fault("pending turn input read", error))?;
        match lash_core_execution::runtime::turn_lane_head(&open, &queued) {
            None => Ok(None),
            Some(lash_core_execution::runtime::TurnLaneHead::Queued(head)) => Ok(Some((
                queued_root(&admission),
                AdmittedWork::Queued {
                    head: head.batch_id.clone(),
                },
            ))),
            Some(lash_core_execution::runtime::TurnLaneHead::Input(head)) => {
                // A root the input is bound to drives it: the root whose
                // admission took it, or the new root a fork bound it to
                // (FIG-3600 S7). Then its host id.
                let bound = store
                    .root_binding(&head.input.input_id)
                    .await
                    .map_err(|error| store_fault("input root binding read", error))?;
                Ok(Some((
                    lash_core_execution::runtime::head_input_root(head, bound),
                    AdmittedWork::Input {
                        head: head.input.input_id.clone(),
                    },
                )))
            }
        }
    }
}

/// The root a queued-work head is admitted under: named by its admission, so
/// no two admissions share a root and a redrive of one names the same root.
fn queued_root(admission: &AdmissionId) -> TurnId {
    TurnId::from(format!("drive-run:{}", admission.as_str()))
}

/// The root an admission of the command lane applies it under.
fn commands_root(admission: &AdmissionId) -> TurnId {
    TurnId::from(format!("drive-commands:{}", admission.as_str()))
}

/// The first execution of one `SealDriveAdmission` step: the drive-epoch
/// compare-and-set, keyed by the admission nonce, so a retried body answers
/// the fence it already raised (ADR 0105 L-S3, L-S4). It stores the start
/// marker the root's execution drew, so another execution of the same
/// admission is answered `SubstrateLost` (L-S8).
pub(in crate::runtime) struct SealDriveRunner {
    /// The session's history store, or `None` when the engine could not open
    /// it at all — the session's close or tombstone already committed — in
    /// which case the step's recorded body is the retirement itself
    /// (FIG-3881).
    pub(in crate::runtime) store: Option<crate::store::SessionStore>,
    pub(in crate::runtime) admitted: Admitted,
    pub(in crate::runtime) root_start: crate::engine::RootStartNonce,
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
        let Some(store) = self.store.as_ref() else {
            return Err(store_fault(
                "session store open",
                StoreError::SessionDeleted {
                    session_id: self.admitted.session().clone(),
                },
            ));
        };
        let seal = store
            .seal_drive_epoch(
                self.admitted.admission(),
                self.admitted.observed_epoch(),
                &self.root_start,
            )
            .await
            .map_err(|error| store_fault("drive epoch seal", error))?;
        let verdict = match seal {
            DriveEpochSeal::Sealed(fence) => SealVerdict::Sealed(fence),
            DriveEpochSeal::Superseded { epoch } => SealVerdict::Superseded { epoch },
            DriveEpochSeal::ExecutionLost => SealVerdict::SubstrateLost {
                root: self.admitted.root().clone(),
            },
        };
        Ok(RuntimeEffectOutcome::SealDriveAdmission {
            verdict: Box::new(verdict),
        })
    }
}
