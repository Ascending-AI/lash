//! In-memory [`TurnInputStore`](crate::store::TurnInputStore) implementation
//! for [`InMemorySessionStore`].
//!
//! Split from `runtime/in_memory_store.rs` to keep it under the file-size
//! budget. This is a trait impl on the parent module's type, so no public
//! path changes.

use super::{InMemoryPendingTurnInput, InMemorySessionStore};
use lash_sansio::sync::MutexExt;

impl InMemoryPendingTurnInput {
    fn claim_diagnostics(&self) -> Option<crate::PendingTurnInputClaimDiagnostics> {
        (self.claim.id().is_some() || matches!(self.input.state, crate::TurnInputState::Accepted))
            .then(|| crate::PendingTurnInputClaimDiagnostics {
                state: self.input.state,
                claim_id: self.claim.id(),
                claim_owner: self.claim.owner(),
                claim_session_lease_generation: self.claim.diagnostic_generation(),
                claim_fencing_token: self.claim.fencing_token,
            })
    }

    pub(super) fn clear_claim(&mut self) {
        self.claim.release();
    }

    fn cancel_outcome(&mut self, claim_is_live: bool) -> crate::PendingTurnInputCancelOutcome {
        match self.input.state {
            crate::TurnInputState::Cancelled => {
                crate::PendingTurnInputCancelOutcome::AlreadyCancelled(self.input.clone())
            }
            crate::TurnInputState::Completed => {
                crate::PendingTurnInputCancelOutcome::AlreadyCompleted(self.input.clone())
            }
            crate::TurnInputState::Accepted => {
                crate::PendingTurnInputCancelOutcome::AlreadyClaimed {
                    input: self.input.clone(),
                    claim: self.claim_diagnostics(),
                }
            }
            crate::TurnInputState::PendingActive | crate::TurnInputState::DeferredNextTurn => {
                if self.claim.token().is_some() && claim_is_live {
                    crate::PendingTurnInputCancelOutcome::AlreadyClaimed {
                        input: self.input.clone(),
                        claim: self.claim_diagnostics(),
                    }
                } else {
                    self.input.state = crate::TurnInputState::Cancelled;
                    self.clear_claim();
                    crate::PendingTurnInputCancelOutcome::Cancelled(self.input.clone())
                }
            }
        }
    }
}

fn find_pending_turn_input_index(
    pending: &[InMemoryPendingTurnInput],
    session_id: &str,
    target: &crate::PendingTurnInputCancelTarget,
) -> Option<usize> {
    pending.iter().position(|entry| {
        entry.input.session_id == session_id
            && match target {
                crate::PendingTurnInputCancelTarget::InputId(input_id) => {
                    entry.input.input_id == *input_id
                }
                crate::PendingTurnInputCancelTarget::SourceKey(source_key) => {
                    entry.input.source_key.as_deref() == Some(source_key.as_str())
                }
            }
    })
}

#[async_trait::async_trait]
impl crate::store::TurnInputStore for InMemorySessionStore {
    async fn record_turn_cancel_request(
        &self,
        request: crate::TurnCancelRequest,
    ) -> Result<crate::TurnCancelRequestRecord, crate::store::StoreError> {
        self.ensure_session_not_deleted(&request.address.session_id)?;
        let _transaction = self.write_transaction.lock_recover();
        let mut requests = self.turn_cancel_requests.lock_recover();
        let record = requests
            .entry(request.address.turn_id.clone())
            .or_insert_with(|| crate::TurnCancelRequestRecord {
                request: request.clone(),
                outcome: None,
            });
        // First writer wins, except that a stronger mode escalates the durable
        // request; the repair outcome accumulated so far stays attached.
        if request.mode.is_stronger_than(record.request.mode) {
            record.request = request;
        }
        Ok(record.clone())
    }

    async fn turn_cancel_request(
        &self,
        address: &crate::TurnAddress,
    ) -> Result<Option<crate::TurnCancelRequestRecord>, crate::store::StoreError> {
        self.ensure_session_not_deleted(&address.session_id)?;
        Ok(self
            .turn_cancel_requests
            .lock_recover()
            .get(&address.turn_id)
            .cloned())
    }

    async fn enqueue_pending_turn_input(
        &self,
        draft: crate::PendingTurnInputDraft,
    ) -> Result<crate::PendingTurnInput, crate::store::StoreError> {
        let enqueued_at_ms = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.ensure_session_not_deleted(&draft.session_id)?;
        let mut pending = self.pending_turn_inputs.lock_recover();
        if let Some(source_key) = draft.source_key.as_deref()
            && let Some(existing) = pending.iter().find(|entry| {
                entry.input.session_id == draft.session_id
                    && entry.input.source_key.as_deref() == Some(source_key)
            })
        {
            if !draft
                .submitted_content_matches(&existing.input)
                .map_err(|err| {
                    crate::store::StoreError::Backend(format!(
                        "failed to compare pending turn input submission: {err}"
                    ))
                })?
            {
                return Err(
                    crate::store::StoreError::PendingTurnInputSourceKeyConflict {
                        session_id: draft.session_id.clone(),
                        source_key: source_key.to_string(),
                        existing_input_id: existing.input.input_id.clone(),
                    },
                );
            }
            return Ok(existing.input.clone());
        }
        let mut next_seq = self.pending_turn_input_next_seq.lock_recover();
        *next_seq = crate::StoreError::checked_monotonic_increment(
            "turn_input_enqueue_sequence",
            *next_seq,
        )?;
        let input_id = draft
            .input_id
            .unwrap_or_else(|| format!("recording-ti-{next_seq}"));
        let state = draft.ingress.initial_state();
        let stored = crate::PendingTurnInput {
            input_id,
            session_id: draft.session_id,
            enqueue_seq: *next_seq,
            source_key: draft.source_key,
            ingress: draft.ingress,
            state,
            enqueued_at_ms,
            input: draft.input,
        };
        pending.push(InMemoryPendingTurnInput {
            input: stored.clone(),
            claim: super::ClaimHold::with_fencing_token(0),
        });
        pending.sort_by_key(|entry| entry.input.enqueue_seq);
        Ok(stored)
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::PendingTurnInput>, crate::store::StoreError> {
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        let live_generation = self.live_session_lease_generation(session_id, now);
        let mut inputs = self
            .pending_turn_inputs
            .lock_recover()
            .iter()
            .filter(|entry| {
                entry.input.session_id == session_id
                    && matches!(
                        entry.input.state,
                        crate::TurnInputState::PendingActive
                            | crate::TurnInputState::DeferredNextTurn
                    )
                    && (!entry.claim.live_under(live_generation))
            })
            .map(|entry| entry.input.clone())
            .collect::<Vec<_>>();
        inputs.sort_by_key(|input| input.enqueue_seq);
        Ok(inputs)
    }

    async fn list_turn_input_applications(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::TurnInputApplication>, crate::store::StoreError> {
        let mut commits = self
            .runtime_turn_commits
            .lock_recover()
            .iter()
            .filter(|((stored_session_id, _), _)| stored_session_id == session_id)
            .map(|((_, turn_id), record)| {
                (
                    record.result.head_revision,
                    turn_id.clone(),
                    record.result.turn_input_applications.clone(),
                )
            })
            .collect::<Vec<_>>();
        commits.sort_by(|left, right| (left.0, left.1.as_str()).cmp(&(right.0, right.1.as_str())));
        Ok(commits
            .into_iter()
            .flat_map(|(_, _, applications)| applications)
            .collect())
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &str,
        targets: &[crate::PendingTurnInputCancelTarget],
    ) -> Result<Vec<crate::PendingTurnInputCancelReceipt>, crate::store::StoreError> {
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        let live_generation = self.live_session_lease_generation(session_id, now);
        let mut pending = self.pending_turn_inputs.lock_recover();
        let mut results = Vec::with_capacity(targets.len());
        for target in targets {
            let outcome = match find_pending_turn_input_index(&pending, session_id, target) {
                Some(index) => {
                    let claim_is_live = pending[index].claim.live_under(live_generation);
                    pending[index].cancel_outcome(claim_is_live)
                }
                None => crate::PendingTurnInputCancelOutcome::NotFound,
            };
            results.push(crate::PendingTurnInputCancelReceipt {
                target: target.clone(),
                outcome,
            });
        }
        Ok(results)
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &str,
        anchor: &crate::PendingTurnInputCancelTarget,
    ) -> Result<crate::PendingTurnInputSuffixCancelOutcome, crate::store::StoreError> {
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        let live_generation = self.live_session_lease_generation(session_id, now);
        let mut pending = self.pending_turn_inputs.lock_recover();
        let Some(anchor_seq) = find_pending_turn_input_index(&pending, session_id, anchor)
            .map(|index| pending[index].input.enqueue_seq)
        else {
            return Ok(crate::PendingTurnInputSuffixCancelOutcome::AnchorNotFound {
                anchor: anchor.clone(),
            });
        };
        pending.sort_by_key(|entry| entry.input.enqueue_seq);
        let outcomes = pending
            .iter_mut()
            .filter(|entry| entry.input.session_id == session_id)
            .filter(|entry| entry.input.enqueue_seq >= anchor_seq)
            .map(|entry| {
                let claim_is_live = entry.claim.live_under(live_generation);
                entry.cancel_outcome(claim_is_live)
            })
            .collect::<Vec<_>>();
        Ok(crate::PendingTurnInputSuffixCancelOutcome::Outcomes {
            anchor: anchor.clone(),
            outcomes,
        })
    }

    async fn claim_active_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        turn_id: &crate::TurnId,
        checkpoint: crate::CheckpointKind,
        max_inputs: usize,
    ) -> Result<Option<crate::TurnInputClaim>, crate::store::StoreError> {
        self.claim_pending_turn_inputs_in_memory(
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            crate::TurnInputClaimMode::ActiveTurn {
                turn_id: turn_id.clone(),
                checkpoint,
            },
        )
    }

    async fn claim_next_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<crate::TurnInputClaim>, crate::store::StoreError> {
        self.claim_pending_turn_inputs_in_memory(
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            crate::TurnInputClaimMode::NextTurn,
        )
    }

    async fn abandon_turn_input_claim(
        &self,
        claim: &crate::TurnInputClaim,
    ) -> Result<(), crate::store::StoreError> {
        let mut pending = self.pending_turn_inputs.lock_recover();
        for entry in pending.iter_mut() {
            if entry.input.session_id == claim.session_id
                && entry.claim.owned_by(&claim.claim_id, &claim.lease_token)
            {
                #[cfg(any(test, feature = "testing"))]
                self.abandoned_turn_input_claim_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if matches!(entry.input.state, crate::TurnInputState::Accepted) {
                    match &claim.mode {
                        crate::TurnInputClaimMode::ActiveTurn { .. } => {
                            entry.input.state = crate::TurnInputState::PendingActive;
                        }
                        crate::TurnInputClaimMode::NextTurn => {
                            entry.input.state = crate::TurnInputState::DeferredNextTurn;
                        }
                    }
                }
                entry.claim.release();
            }
        }
        Ok(())
    }

    async fn defer_orphaned_active_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        scope: crate::OrphanedTurnInputScope<'_>,
    ) -> Result<crate::TurnCancelInputOutcome, crate::store::StoreError> {
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        // Inside the write transaction, exactly like the claim path: a fence
        // validated outside it could be displaced before the repair writes.
        self.verify_session_execution_lease(session_id, session_execution_lease, now)?;
        let mut pending = self.pending_turn_inputs.lock_recover();
        let mut requests = self.turn_cancel_requests.lock_recover();
        let mut outcome = crate::TurnCancelInputOutcome::default();
        for entry in pending.iter_mut() {
            if entry.input.session_id != session_id
                || !crate::store_backend_support::orphaned_active_turn_input_is_repairable(
                    scope,
                    session_execution_lease.fencing_token,
                    entry.input.state,
                    &entry.input.ingress,
                    entry.claim.token().is_some(),
                    entry.claim.generation().unwrap_or(0),
                )
            {
                continue;
            }
            let turn_id = entry
                .input
                .ingress
                .active_turn_id()
                .expect("repairable input is active-turn scoped")
                .clone();
            let disposition = requests
                .get(&turn_id)
                .map_or(crate::TurnCancelDisposition::Defer, |record| {
                    record.request.undelivered
                });
            let affected = crate::TurnCancelAffectedInput {
                input_id: entry.input.input_id.clone(),
                payload: entry.input.input.clone(),
                disposition,
            };
            match disposition {
                crate::TurnCancelDisposition::Defer => {
                    entry.input.state = crate::TurnInputState::DeferredNextTurn;
                    entry.input.ingress = crate::TurnInputIngress::NextTurn;
                }
                crate::TurnCancelDisposition::Drop => {
                    entry.input.state = crate::TurnInputState::Cancelled;
                }
            }
            entry.clear_claim();
            if let Some(record) = requests.get_mut(&turn_id) {
                record
                    .outcome
                    .get_or_insert_with(crate::TurnCancelInputOutcome::default)
                    .affected_inputs
                    .push(affected.clone());
            }
            outcome.affected_inputs.push(affected);
        }
        Ok(outcome)
    }
}
