//! The journaled initial drive set of an accepted turn input (ADR 0069 §6).
//!
//! Acceptance and checkpoint claims are journaled effects; the claim a direct
//! turn takes right after its acceptance is one too. The local runner here is
//! the only code that reads pending rows to decide what an accepted turn
//! drives, and it runs only on a first execution: a replaying engine returns
//! the journaled [`AcceptedTurnInputDrive`](crate::AcceptedTurnInputDrive)
//! instead, so the drive never depends on rows `vacuum()` may have pruned.

use super::*;
use crate::TurnId;
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;

/// Everything the first execution of `ClaimAcceptedTurnInput` needs, captured
/// at the drive site. None of it enters the effect envelope: the fence and
/// owner change with every lease generation, and the envelope must not.
pub(super) struct AcceptedTurnInputDriveRunner {
    pub(super) store: Arc<dyn crate::store::RuntimePersistence>,
    pub(super) fence: crate::SessionExecutionLeaseAuthority,
    pub(super) owner: crate::LeaseOwnerIdentity,
    pub(super) accepted: crate::PendingTurnInput,
    /// The runtime's turn-input claim bound
    /// ([`QueuedWorkBatchingConfig::max_turn_input_claim`](crate::QueuedWorkBatchingConfig::max_turn_input_claim)).
    pub(super) max_inputs: usize,
    /// The resident head the turn is admitted on, as the accept phase
    /// refreshed it under the lease. Its generation is read in the body.
    pub(super) base: crate::store::SessionHeadRef,
    /// The admitted turn's index: the next one after `base`.
    pub(super) turn_index: usize,
    pub(super) trace: DriveTrace,
}

/// Trace attribution for the claim decisions the runner makes.
pub(super) struct DriveTrace {
    pub(super) sink: Option<Arc<dyn lash_trace::TraceSink>>,
    pub(super) base: lash_trace::TraceContext,
    pub(super) clock: Arc<dyn crate::Clock>,
    pub(super) session_id: crate::SessionId,
    pub(super) turn_index: usize,
    pub(super) turn_id: TurnId,
}

impl DriveTrace {
    fn emit(&self, name: &str, payload: serde_json::Value) {
        crate::trace::emit_trace(
            &self.sink,
            &self.base,
            lash_trace::TraceContext::default()
                .for_session(self.session_id.clone())
                .for_turn_index(self.turn_index)
                .for_turn(self.turn_id.clone()),
            lash_trace::TraceEvent::Custom {
                name: name.to_string(),
                payload,
            },
            self.clock.as_ref(),
        );
    }
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for AcceptedTurnInputDriveRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::ClaimAcceptedTurnInput { input_id } = envelope.command
        else {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "accepted-turn-input drive executor cannot execute {} command",
                    envelope.command.kind().as_str()
                ),
            ));
        };
        if input_id != self.accepted.input_id {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "accepted-turn-input drive executor was bound to `{}` but asked to drive `{input_id}`",
                    self.accepted.input_id
                ),
            ));
        }
        // A store failure here is journaled with the effect under a durable
        // engine, so it must not carry a retryable code: every retry of the
        // invocation would replay the same failure. Like the acceptance
        // write, it surfaces as a store-commit failure.
        let drive = self.drive().await.map_err(|err| {
            crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::StoreCommitFailed,
                format!("accepted turn input drive failed: {err}"),
            )
        })?;
        Ok(crate::RuntimeEffectOutcome::ClaimAcceptedTurnInput { drive })
    }
}

impl AcceptedTurnInputDriveRunner {
    /// Decide what this acceptance drives, against live rows.
    ///
    /// A claim that reaches the accepted row drives every row it took. One that
    /// does not is handed straight back, and the accepted row is read without
    /// mutating it: still open means it waits behind more earlier admissions
    /// than one claim absorbs, so it stays queued for the drain to answer in
    /// order; bound to this turn means an earlier execution of it aborted, and
    /// this redrive re-takes the set that execution drove (FIG-3589); held
    /// means another driver has it; absent means it was settled, cancelled, or
    /// pruned. The last two cede. Nothing here ever drops, withdraws, or
    /// re-admits a row.
    async fn drive(self) -> Result<crate::AcceptedTurnInputDrive, crate::StoreError> {
        if let Some(claim) = self.claim_admitted_through_acceptance().await? {
            if claim
                .inputs
                .iter()
                .any(|pending| pending.input_id == self.accepted.input_id)
            {
                self.trace.emit(
                    "turn_input.claimed",
                    serde_json::json!({
                        "claim_id": &claim.claim_id,
                        "input_ids": claim
                            .inputs
                            .iter()
                            .map(|input| input.input_id.clone())
                            .collect::<Vec<_>>(),
                    }),
                );
                return self.admit(claim).await;
            }
            // A claim that reached rows but not this turn's own acceptance is
            // claim-pinning up to `max_inputs` rows this caller will never
            // drive. Hand it straight back instead of leaving those
            // rows stalled until the FIG-1573 backstop repairs them.
            self.trace.emit(
                "turn_input.claim_abandoned",
                serde_json::json!({
                    "claim_id": &claim.claim_id,
                    "accepted_input_id": &self.accepted.input_id,
                    "input_ids": claim
                        .inputs
                        .iter()
                        .map(|input| input.input_id.clone())
                        .collect::<Vec<_>>(),
                    "reason": "claim_missed_own_acceptance",
                }),
            );
            if let Err(abandon_err) = self
                .store
                .abandon_turn_input_claims(std::slice::from_ref(&claim))
                .await
            {
                tracing::warn!(
                    error = %abandon_err,
                    claim_id = %claim.claim_id,
                    claim_count = claim.inputs.len(),
                    "failed to abandon a turn-input claim that missed its own acceptance"
                );
            }
        }
        let open = self
            .store
            .list_pending_turn_inputs(&self.accepted.session_id)
            .await?;
        let own_row = open
            .iter()
            .find(|read| read.input.input_id == self.accepted.input_id);
        Ok(match own_row {
            Some(read) => match &read.status {
                crate::PendingTurnInputReadStatus::Pending => {
                    // A row bound to an aborted turn waits for that turn's
                    // redrive, not for the drain, so it is not ahead.
                    let ahead = open
                        .iter()
                        .filter(|earlier| {
                            earlier.input.state == crate::TurnInputState::DeferredNextTurn
                                && earlier.input.enqueue_seq < self.accepted.enqueue_seq
                                && !matches!(
                                    earlier.status,
                                    crate::PendingTurnInputReadStatus::TurnBound { .. }
                                )
                        })
                        .count();
                    crate::AcceptedTurnInputDrive::Queued {
                        ahead: u64::try_from(ahead).unwrap_or(u64::MAX),
                    }
                }
                // An earlier execution of this same turn drove the row and
                // aborted, binding it here: this redrive re-takes that set.
                crate::PendingTurnInputReadStatus::TurnBound { turn_id, .. }
                    if *turn_id == self.trace.turn_id =>
                {
                    match self.reclaim_bound_drive().await? {
                        Some(claim) => self.admit(claim).await?,
                        None => crate::AcceptedTurnInputDrive::Refused {
                            refusal: crate::AcceptedTurnInputRefusal::HeldByLiveClaim,
                        },
                    }
                }
                // Held under the live lease generation, or any status this
                // read cannot prove drivable: another claim of this lane owns
                // the row.
                _ => crate::AcceptedTurnInputDrive::Refused {
                    refusal: crate::AcceptedTurnInputRefusal::HeldByLiveClaim,
                },
            },
            None => crate::AcceptedTurnInputDrive::Refused {
                refusal: crate::AcceptedTurnInputRefusal::SettledOrRemoved,
            },
        })
    }

    /// Admit the turn that drives `claim`: record the head it is admitted on
    /// and its turn index with the claim, and have the store retain that head
    /// until the session's next admission (FIG-3682).
    ///
    /// Both ride the journaled outcome, so a replay rebuilds the turn's input
    /// state from the recorded head and addresses its effects under the
    /// recorded index, even after the turn's own commit moved the live head.
    async fn admit(
        &self,
        claim: crate::TurnInputClaim,
    ) -> Result<crate::AcceptedTurnInputDrive, crate::StoreError> {
        let base = crate::store::SessionHeadRef {
            generation: self.store.read_session_state_version().await?,
            ..self.base.clone()
        };
        self.store.retain_admission_base(&self.fence, &base).await?;
        Ok(crate::AcceptedTurnInputDrive::Claimed {
            claim: Box::new(claim),
            base,
            turn_index: self.turn_index as u64,
        })
    }

    /// Re-take the rows an earlier execution of this same turn drove and then
    /// aborted on (FIG-3589).
    ///
    /// The aborted execution bound its drive claim to this turn id, so no
    /// other claim can take those rows, and the ordinary claim above skipped
    /// the accepted row. A redrive that replays a journaled drive never gets
    /// here; this is the redrive whose effect host did not journal the drive,
    /// and it drives exactly the bound set, which always holds its own
    /// accepted row: any cancel of a bound row releases the rest.
    async fn reclaim_bound_drive(
        &self,
    ) -> Result<Option<crate::TurnInputClaim>, crate::StoreError> {
        let Some(claim) = self
            .store
            .reclaim_turn_bound_inputs(
                &self.accepted.session_id,
                &self.fence,
                &self.owner,
                &self.trace.turn_id,
            )
            .await?
        else {
            return Ok(None);
        };
        if !claim
            .inputs
            .iter()
            .any(|pending| pending.input_id == self.accepted.input_id)
        {
            // Unreachable while cancels release whole bound claims; hand the
            // rows back rather than drive a set without this turn's input.
            self.store.abandon_turn_input_claim(&claim).await?;
            return Ok(None);
        }
        self.trace.emit(
            "turn_input.bound_drive_reclaimed",
            serde_json::json!({
                "claim_id": &claim.claim_id,
                "input_ids": claim
                    .inputs
                    .iter()
                    .map(|input| input.input_id.clone())
                    .collect::<Vec<_>>(),
            }),
        );
        Ok(Some(claim))
    }

    /// Claim the queued next-turn rows this acceptance is allowed to drive.
    ///
    /// **The claim window closes at the acceptance (FIG-3078).** A turn drives
    /// exactly the admitted rows whose `enqueue_seq` is at or before the row
    /// its own acceptance minted, so a row admitted *after* the acceptance
    /// (the multi-tab `next_turn` that lands between admission and the claim)
    /// waits for the next turn instead of joining this one.
    ///
    /// The bound is enforced by re-claiming the admitted prefix rather than by
    /// dropping rows from a held claim: every backend claims lowest
    /// `enqueue_seq` first, so a claim capped at the prefix length takes
    /// exactly that prefix, and the late rows go back to `deferred_next_turn`
    /// for the next drain instead of sitting claim-pinned until the FIG-1573
    /// backstop repairs them.
    async fn claim_admitted_through_acceptance(
        &self,
    ) -> Result<Option<crate::TurnInputClaim>, crate::StoreError> {
        let session_id = &self.accepted.session_id;
        let Some(claim) = self
            .store
            .claim_next_turn_inputs(session_id, &self.fence, &self.owner, self.max_inputs)
            .await?
        else {
            return Ok(None);
        };
        let admitted_through = self.accepted.enqueue_seq;
        let late = claim
            .inputs
            .iter()
            .filter(|input| input.enqueue_seq > admitted_through)
            .map(|input| input.input_id.clone())
            .collect::<Vec<_>>();
        if late.is_empty() {
            return Ok(Some(claim));
        }
        let admitted = claim.inputs.len() - late.len();
        self.trace.emit(
            "turn_input.claim_window_closed",
            serde_json::json!({
                "claim_id": &claim.claim_id,
                "accepted_input_id": &self.accepted.input_id,
                "admitted_through_enqueue_seq": admitted_through,
                "admitted_input_count": admitted,
                "deferred_input_ids": &late,
            }),
        );
        self.store.abandon_turn_input_claim(&claim).await?;
        if admitted == 0 {
            // Every claimable row came after this acceptance, so its own row
            // is not claimable: the caller reads it and decides.
            return Ok(None);
        }
        self.store
            .claim_next_turn_inputs(session_id, &self.fence, &self.owner, admitted)
            .await
    }
}
