//! In-memory [`TurnInputStore`](crate::store::TurnInputStore) implementation
//! for [`InMemorySessionStore`].
//!
//! Split from `runtime/in_memory_store.rs` to keep it under the file-size
//! budget. This is a trait impl on the parent module's type, so no public
//! path changes.

use super::{InMemoryPendingTurnInput, InMemorySessionStore};
use crate::SessionId;
use lash_sansio::sync::MutexExt;

/// The in-memory store's turn-input settlement predicate.
///
/// One predicate, two regimes: the claim fields only strengthen it. A claimed
/// settlement requires the row to still carry that claim; an unclaimed
/// settlement requires it to still be unclaimed and unsettled
/// ([ADR 0069](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0069-durable-acceptance-is-the-sole-turn-ingress.md) §5).
pub(super) fn settlement_matches(
    entry: &InMemoryPendingTurnInput,
    completed: &crate::TurnInputCompletion,
) -> bool {
    entry.input.session_id == completed.session_id
        && completed.input_ids.contains(&entry.input.input_id)
        && match completed.claim.as_ref() {
            Some(claim) => entry.claim.owned_by(&claim.claim_id, &claim.lease_token),
            None => entry.claim.id().is_none() && !entry.input.state.is_terminal(),
        }
}

impl InMemoryPendingTurnInput {
    fn claim_diagnostics(&self) -> Option<crate::PendingTurnInputClaimDiagnostics> {
        self.claim
            .id()
            .is_some()
            .then(|| crate::PendingTurnInputClaimDiagnostics {
                state: self.input.state.clone(),
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
        match self.input.state.kind() {
            crate::TurnInputStateKind::Cancelled => {
                crate::PendingTurnInputCancelOutcome::AlreadyCancelled(self.input.clone())
            }
            crate::TurnInputStateKind::Completed => {
                crate::PendingTurnInputCancelOutcome::AlreadyCompleted(self.input.clone())
            }
            crate::TurnInputStateKind::PendingActive
            | crate::TurnInputStateKind::DeferredNextTurn
            | crate::TurnInputStateKind::Accepted => {
                if self.claim.token().is_some() && claim_is_live {
                    crate::PendingTurnInputCancelOutcome::AlreadyClaimed {
                        input: self.input.clone(),
                        claim: self.claim_diagnostics(),
                    }
                } else {
                    self.input.state = crate::TurnInputState::Cancelled(self.input.state.ingress());
                    self.clear_claim();
                    crate::PendingTurnInputCancelOutcome::Cancelled(self.input.clone())
                }
            }
        }
    }
}

fn find_pending_turn_input_index(
    pending: &[InMemoryPendingTurnInput],
    session_id: &SessionId,
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
    fn turn_cancellation_authority(
        &self,
    ) -> Option<
        std::sync::Arc<dyn lash_core_store::turn_control_binding::StoreTurnCancellationAuthority>,
    > {
        self.turn_cancellation_authority.clone()
    }

    async fn validate_turn_cancellation_binding(
        &self,
        session_id: &SessionId,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        binding_id: &str,
        admitted_scope: &crate::ExecutionScope,
    ) -> Result<(), crate::store::StoreError> {
        admitted_scope
            .validate()
            .map_err(|error| crate::StoreError::StoredDataCorrupt {
                record_kind: "TurnCancellationBinding",
                message: error.to_string(),
            })?;
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.ensure_session_not_deleted(session_id)?;
        self.verify_session_execution_lease(session_id, session_execution_lease, now)?;
        let admitted_physical_scope = admitted_scope
            .session_id()
            .is_none()
            .then(|| admitted_scope.clone());
        let mut selected = self.turn_cancellation_binding.lock_recover();
        match selected.as_ref() {
            None => *selected = Some((binding_id.to_string(), admitted_physical_scope.clone())),
            Some((expected, expected_scope))
                if expected == binding_id && expected_scope == &admitted_physical_scope => {}
            Some((expected, _)) => {
                return Err(crate::StoreError::TurnCancelBindingMismatch {
                    session_id: session_id.clone(),
                    expected: expected.to_string(),
                    presented: binding_id.to_string(),
                });
            }
        }
        Ok(())
    }

    async fn authorize_turn_cancel_closure(
        &self,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        authorization: &crate::TurnCancelClosureAuthorization,
    ) -> Result<crate::TurnCancelClosureAuthorizationOutcome, crate::store::StoreError> {
        authorization
            .validate()
            .map_err(|error| crate::StoreError::StoredDataCorrupt {
                record_kind: "TurnCancelClosureAuthorization",
                message: error.to_string(),
            })?;
        let _transaction = self.write_transaction.lock_recover();
        self.ensure_session_not_deleted(authorization.session_id())?;
        if authorization.session_id() != session_execution_lease.session_id
            || authorization.authorizing_fencing_token() != session_execution_lease.fencing_token
        {
            return Err(crate::StoreError::SessionExecutionLeaseExpired {
                session_id: authorization.session_id().clone(),
            });
        }
        if authorization.admitted_scope().session_id().is_none() {
            let scope_id = authorization
                .admitted_scope()
                .journal_identity()
                .map_err(|error| crate::StoreError::Backend(error.to_string()))?
                .key()
                .to_string();
            if self
                .retired_turn_cancel_scopes
                .lock_recover()
                .contains(&scope_id)
            {
                return Err(crate::StoreError::TurnCancelClosureScopeRetired { scope_id });
            }
        }
        let selected = self.turn_cancellation_binding.lock_recover();
        let admitted_physical_scope = authorization
            .admitted_scope()
            .session_id()
            .is_none()
            .then(|| authorization.admitted_scope().clone());
        if selected
            .as_ref()
            .map(|(binding, scope)| (binding.as_str(), scope))
            != Some((authorization.binding_id(), &admitted_physical_scope))
        {
            return Err(crate::StoreError::TurnCancelBindingMismatch {
                session_id: authorization.session_id().clone(),
                expected: selected
                    .as_ref()
                    .map(|(binding, scope)| format!("{binding} at {scope:?}"))
                    .unwrap_or_default(),
                presented: authorization.binding_id().to_string(),
            });
        }
        let mut pending = self.turn_cancel_closure_authorizations.lock_recover();
        match pending.get(authorization.turn_id()) {
            Some(existing) if existing == authorization => {
                Ok(crate::TurnCancelClosureAuthorizationOutcome::AdoptedExact)
            }
            Some(_) => Err(crate::StoreError::TurnCancelClosureConflict {
                session_id: authorization.session_id().clone(),
                turn_id: authorization.turn_id().clone(),
            }),
            None => {
                let requests = self.turn_cancel_requests.lock_recover();
                if snapshot(&requests, authorization.turn_id()) != *authorization.observed_intent()
                {
                    return Err(crate::StoreError::TurnCancelIntentChanged {
                        session_id: authorization.session_id().clone(),
                        turn_id: authorization.turn_id().clone(),
                    });
                }
                drop(requests);
                pending.insert(authorization.turn_id().clone(), authorization.clone());
                Ok(crate::TurnCancelClosureAuthorizationOutcome::Authorized)
            }
        }
    }

    async fn pending_turn_cancel_closures(
        &self,
        session_id: &SessionId,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        binding_id: &str,
        admitted_scope: &crate::ExecutionScope,
    ) -> Result<Vec<crate::TurnCancelClosureAuthorization>, crate::store::StoreError> {
        self.validate_turn_cancellation_binding(
            session_id,
            session_execution_lease,
            binding_id,
            admitted_scope,
        )
        .await?;
        Ok(self
            .turn_cancel_closure_authorizations
            .lock_recover()
            .values()
            .filter(|authorization| authorization.session_id() == session_id)
            .cloned()
            .collect())
    }

    async fn pending_turn_cancel_closure_pins(
        &self,
    ) -> Result<Vec<crate::TurnCancelClosureAuthorization>, crate::store::StoreError> {
        Ok(self
            .turn_cancel_closure_authorizations
            .lock_recover()
            .values()
            .cloned()
            .collect())
    }

    async fn turn_is_committed(
        &self,
        address: &crate::TurnAddress,
    ) -> Result<bool, crate::store::StoreError> {
        let operation_key =
            crate::OperationId::turn(&address.session_id, &address.turn_id, "final")
                .storage_key()?;
        Ok(self
            .runtime_turn_commits
            .lock_recover()
            .contains_key(&(address.session_id.clone(), operation_key)))
    }

    async fn record_turn_cancel_request(
        &self,
        request: crate::TurnCancelRequest,
    ) -> Result<crate::TurnCancelRequestRecord, crate::store::StoreError> {
        let _transaction = self.write_transaction.lock_recover();
        self.ensure_session_not_deleted(&request.address.session_id)?;
        let mut requests = self.turn_cancel_requests.lock_recover();
        let operation_key = crate::OperationId::turn(
            &request.address.session_id,
            &request.address.turn_id,
            "final",
        )
        .storage_key()?;
        if self
            .runtime_turn_commits
            .lock_recover()
            .contains_key(&(request.address.session_id.clone(), operation_key))
        {
            return Ok(crate::TurnCancelRequestRecord {
                request,
                outcome: None,
            });
        }
        let stored = requests
            .entry(request.address.turn_id.clone())
            .or_insert_with(|| super::InMemoryTurnCancelRequest {
                record: crate::TurnCancelRequestRecord {
                    request: request.clone(),
                    outcome: None,
                },
                intent_revision: 1,
            });
        // The first policy acceptor is immutable. A stronger same-policy
        // request advances the closure-CAS revision; its effective timing is
        // recorded by the settled gate. A request that disagrees about the
        // undelivered-input disposition is not an escalation: the gate refuses
        // it, so it leaves the row and its revision untouched.
        if request.escalates(&stored.record.request) {
            stored.intent_revision = crate::store::StoreError::checked_monotonic_increment(
                "turn_cancel_intent_revision",
                stored.intent_revision,
            )?;
        }
        Ok(stored.record.clone())
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
            .map(|stored| stored.record.clone()))
    }

    async fn turn_cancel_request_intent(
        &self,
        address: &crate::TurnAddress,
    ) -> Result<crate::TurnCancelIntentSnapshot, crate::store::StoreError> {
        self.ensure_session_not_deleted(&address.session_id)?;
        Ok(self
            .turn_cancel_requests
            .lock_recover()
            .get(&address.turn_id)
            .map_or(crate::TurnCancelIntentSnapshot::Absent, |stored| {
                crate::TurnCancelIntentSnapshot::Present {
                    request: stored.record.request.clone(),
                    revision: stored.intent_revision,
                }
            }))
    }

    async fn reconcile_turn_cancel_winner(
        &self,
        address: &crate::TurnAddress,
        observed: &crate::TurnCancelIntentSnapshot,
        evidence: &crate::TurnCancellationEvidence,
    ) -> Result<bool, crate::store::StoreError> {
        let _transaction = self.write_transaction.lock_recover();
        self.ensure_session_not_deleted(&address.session_id)?;
        let mut requests = self.turn_cancel_requests.lock_recover();
        if snapshot(&requests, &address.turn_id) != *observed {
            return Ok(false);
        }
        let outcome = requests
            .get(&address.turn_id)
            .and_then(|stored| stored.record.outcome.clone());
        let request = request_from_evidence(address, evidence);
        let revision = match requests.get(&address.turn_id) {
            Some(stored) if stored.record.request == request => stored.intent_revision,
            Some(stored) => crate::store::StoreError::checked_monotonic_increment(
                "turn_cancel_intent_revision",
                stored.intent_revision,
            )?,
            None => 1,
        };
        requests.insert(
            address.turn_id.clone(),
            super::InMemoryTurnCancelRequest {
                record: crate::TurnCancelRequestRecord { request, outcome },
                intent_revision: revision,
            },
        );
        Ok(true)
    }

    async fn enqueue_pending_turn_input(
        &self,
        draft: crate::PendingTurnInputDraft,
    ) -> Result<crate::PendingTurnInput, crate::store::StoreError> {
        let enqueued_at_ms = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.ensure_session_not_deleted(&draft.session_id)?;
        let submission_digest = draft.submission_digest().map_err(|err| {
            crate::store::StoreError::Backend(format!(
                "failed to digest pending turn input submission: {err}"
            ))
        })?;
        let mut pending = self.pending_turn_inputs.lock_recover();
        if let Some(source_key) = draft.source_key.as_deref()
            && let Some(existing) = pending.iter().find(|entry| {
                entry.input.session_id == draft.session_id
                    && entry.input.source_key.as_deref() == Some(source_key)
            })
        {
            if existing.submission_digest != submission_digest {
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
        let candidate_seq = crate::StoreError::checked_monotonic_increment(
            "turn_input_enqueue_sequence",
            *next_seq,
        )?;
        let input_id = draft
            .input_id
            .map(crate::InputId::new)
            .unwrap_or_else(|| crate::InputId::new(format!("recording-ti-{candidate_seq}")));
        // `input_id` is unique across the store, as on the SQL schemas'
        // `UNIQUE` column: a draft reusing an id any row already carries is
        // refused, whatever session that row belongs to.
        if pending.iter().any(|entry| entry.input.input_id == input_id) {
            return Err(crate::store::StoreError::PendingTurnInputIdConflict {
                session_id: draft.session_id,
                input_id,
            });
        }
        *next_seq = candidate_seq;
        let stored = crate::PendingTurnInput {
            input_id,
            session_id: draft.session_id,
            enqueue_seq: *next_seq,
            source_key: draft.source_key,
            state: crate::TurnInputState::open(draft.ingress),
            enqueued_at_ms,
            input: draft.input,
        };
        pending.push(InMemoryPendingTurnInput {
            input: stored.clone(),
            claim: super::ClaimHold::with_fencing_token(0),
            submission_digest,
        });
        pending.sort_by_key(|entry| entry.input.enqueue_seq);
        Ok(stored)
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::PendingTurnInputRead>, crate::store::StoreError> {
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        let live_lease = self.live_session_lease(session_id, now);
        let mut inputs = self
            .pending_turn_inputs
            .lock_recover()
            .iter()
            .filter(|entry| {
                entry.input.session_id == session_id
                    && matches!(
                        entry.input.state,
                        crate::TurnInputState::PendingActive(_)
                            | crate::TurnInputState::DeferredNextTurn
                    )
            })
            .map(|entry| match live_lease {
                Some((generation, lease_expires_at_ms))
                    if entry.claim.live_under(Some(generation)) =>
                {
                    crate::PendingTurnInputRead::held(entry.input.clone(), lease_expires_at_ms)
                }
                _ => crate::PendingTurnInputRead::pending(entry.input.clone()),
            })
            .collect::<Vec<_>>();
        inputs.sort_by_key(|read| read.input.enqueue_seq);
        Ok(inputs)
    }

    async fn list_turn_input_applications(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::TurnInputApplication>, crate::store::StoreError> {
        let mut commits = Vec::new();
        for ((stored_session_id, turn_id), record) in
            self.runtime_turn_commits.lock_recover().iter()
        {
            if stored_session_id != session_id {
                continue;
            }
            crate::store::ensure_supported_receipt_version(&record.result)?;
            commits.push((
                record.result.head_revision,
                turn_id.clone(),
                record.result.turn_input_applications.clone(),
            ));
        }
        commits.sort_by(|left, right| (left.0, left.1.as_str()).cmp(&(right.0, right.1.as_str())));
        Ok(commits
            .into_iter()
            .flat_map(|(_, _, applications)| applications)
            .collect())
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &SessionId,
        targets: &[crate::PendingTurnInputCancelTarget],
    ) -> Result<Vec<crate::PendingTurnInputCancelReceipt>, crate::store::StoreError> {
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        let live_generation = self.live_session_lease_generation(session_id, now);
        let runs = self.queued_runs.lock_recover();
        let mut pending = self.pending_turn_inputs.lock_recover();
        let mut results = Vec::with_capacity(targets.len());
        for target in targets {
            let outcome = match find_pending_turn_input_index(&pending, session_id, target) {
                Some(index) => {
                    let claim_is_live = pending[index].claim.live_under(live_generation);
                    let run_owns_input = runs.values().any(|run| {
                        run.scope.session_id() == Some(session_id)
                            && run.terminal.is_none()
                            && run.owns_member(&crate::store::QueuedRunMember::Input(
                                pending[index].input.input_id.clone(),
                            ))
                    });
                    pending[index].cancel_outcome(claim_is_live || run_owns_input)
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
        session_id: &SessionId,
        anchor: &crate::PendingTurnInputCancelTarget,
    ) -> Result<crate::PendingTurnInputSuffixCancelOutcome, crate::store::StoreError> {
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        let live_generation = self.live_session_lease_generation(session_id, now);
        let runs = self.queued_runs.lock_recover();
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
                let run_owns_input = runs.values().any(|run| {
                    run.scope.session_id() == Some(session_id)
                        && run.terminal.is_none()
                        && run.owns_member(&crate::store::QueuedRunMember::Input(
                            entry.input.input_id.clone(),
                        ))
                });
                entry.cancel_outcome(claim_is_live || run_owns_input)
            })
            .collect::<Vec<_>>();
        Ok(crate::PendingTurnInputSuffixCancelOutcome::Outcomes {
            anchor: anchor.clone(),
            outcomes,
        })
    }

    async fn claim_active_turn_inputs(
        &self,
        session_id: &SessionId,
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
        session_id: &SessionId,
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
                if let crate::TurnInputState::Accepted(scope) = &entry.input.state {
                    entry.input.state = crate::TurnInputState::PendingActive(scope.clone());
                }
                entry.claim.release();
            }
        }
        Ok(())
    }

    async fn orphaned_active_turn_ids(
        &self,
        session_id: &SessionId,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        scope: crate::OrphanedTurnInputScope<'_>,
    ) -> Result<Vec<crate::TurnId>, crate::store::StoreError> {
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.ensure_session_not_deleted(session_id)?;
        self.verify_session_execution_lease(session_id, session_execution_lease, now)?;
        let pending = self.pending_turn_inputs.lock_recover();
        let mut turn_ids = std::collections::BTreeSet::new();
        for entry in pending.iter() {
            if entry.input.session_id == session_id
                && crate::store_backend_support::orphaned_active_turn_input_is_repairable(
                    scope,
                    session_execution_lease.fencing_token,
                    &entry.input.state,
                    entry.claim.token().is_some(),
                    entry.claim.generation().unwrap_or(0),
                )
                && let Some(turn_id) = entry.input.state.active_turn_id()
            {
                turn_ids.insert(turn_id.clone());
            }
        }
        Ok(turn_ids.into_iter().collect())
    }

    async fn repair_orphaned_active_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        turn_id: &crate::TurnId,
        observed: &crate::TurnCancelIntentSnapshot,
        settlement: Option<&crate::TurnCancelClosureSettlement>,
    ) -> Result<crate::store::TurnCancelRepairResult, crate::store::StoreError> {
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.ensure_session_not_deleted(session_id)?;
        self.verify_session_execution_lease(session_id, session_execution_lease, now)?;
        let closure = settlement.map(crate::TurnCancelClosureSettlement::authorization);
        let pending_closure = self
            .turn_cancel_closure_authorizations
            .lock_recover()
            .get(turn_id)
            .cloned();
        let closure_required = pending_closure.is_some()
            || !matches!(observed, crate::TurnCancelIntentSnapshot::Absent);
        if closure_required != settlement.is_some()
            || closure.is_some_and(|authorization| {
                authorization.session_id() != session_id || authorization.turn_id() != turn_id
            })
        {
            return Err(crate::StoreError::TurnCancelClosureAuthorizationMismatch {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
            });
        }
        if let Some(closure) = closure
            && pending_closure.as_ref() != Some(closure)
        {
            return Err(crate::StoreError::TurnCancelClosureAuthorizationMismatch {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
            });
        }
        let mut pending = self.pending_turn_inputs.lock_recover();
        let mut requests = self.turn_cancel_requests.lock_recover();
        if snapshot(&requests, turn_id) != *observed {
            return Ok(crate::store::TurnCancelRepairResult::IntentChanged);
        }
        let effective =
            settlement.and_then(crate::TurnCancelClosureSettlement::effective_cancellation);
        let disposition = effective.map_or(crate::TurnCancelDisposition::Defer, |e| e.undelivered);
        if let Some(evidence) =
            settlement.and_then(crate::TurnCancelClosureSettlement::base_cancellation)
        {
            reconcile_authenticated_turn_cancel_winner(
                &mut requests,
                &crate::TurnAddress::new(session_id, turn_id),
                evidence,
            )?;
        }
        let mut outcome = crate::TurnCancelInputOutcome::default();
        for entry in pending.iter_mut() {
            if entry.input.session_id != session_id
                || !crate::store_backend_support::orphaned_active_turn_input_is_repairable(
                    crate::OrphanedTurnInputScope::Turn(turn_id),
                    session_execution_lease.fencing_token,
                    &entry.input.state,
                    entry.claim.token().is_some(),
                    entry.claim.generation().unwrap_or(0),
                )
            {
                continue;
            }
            let affected = crate::TurnCancelAffectedInput {
                input_id: entry.input.input_id.clone(),
                payload: entry.input.input.clone(),
                disposition,
            };
            match disposition {
                crate::TurnCancelDisposition::Defer => {
                    entry.input.state = crate::TurnInputState::DeferredNextTurn;
                }
                crate::TurnCancelDisposition::Drop => {
                    entry.input.state =
                        crate::TurnInputState::Cancelled(entry.input.state.ingress());
                }
            }
            entry.clear_claim();
            if effective.is_some()
                && let Some(record) = requests.get_mut(turn_id)
            {
                record
                    .record
                    .outcome
                    .get_or_insert_with(crate::TurnCancelInputOutcome::default)
                    .affected_inputs
                    .push(affected.clone());
            }
            outcome.affected_inputs.push(affected);
        }
        if settlement.is_some() {
            self.turn_cancel_closure_authorizations
                .lock_recover()
                .remove(turn_id);
        }
        Ok(crate::store::TurnCancelRepairResult::Applied(outcome))
    }
}

pub(super) fn snapshot(
    requests: &std::collections::HashMap<crate::TurnId, super::InMemoryTurnCancelRequest>,
    turn_id: &crate::TurnId,
) -> crate::TurnCancelIntentSnapshot {
    requests
        .get(turn_id)
        .map_or(crate::TurnCancelIntentSnapshot::Absent, |stored| {
            crate::TurnCancelIntentSnapshot::Present {
                request: stored.record.request.clone(),
                revision: stored.intent_revision,
            }
        })
}

pub(super) fn reconcile_authenticated_turn_cancel_winner(
    requests: &mut std::collections::HashMap<crate::TurnId, super::InMemoryTurnCancelRequest>,
    address: &crate::TurnAddress,
    evidence: &crate::TurnCancellationEvidence,
) -> Result<(), crate::StoreError> {
    let outcome = requests
        .get(&address.turn_id)
        .and_then(|stored| stored.record.outcome.clone());
    let request = request_from_evidence(address, evidence);
    let intent_revision = match requests.get(&address.turn_id) {
        Some(stored) if stored.record.request == request => stored.intent_revision,
        Some(stored) => crate::StoreError::checked_monotonic_increment(
            "turn_cancel_intent_revision",
            stored.intent_revision,
        )?,
        None => 1,
    };
    requests.insert(
        address.turn_id.clone(),
        super::InMemoryTurnCancelRequest {
            record: crate::TurnCancelRequestRecord { request, outcome },
            intent_revision,
        },
    );
    Ok(())
}

pub(super) fn request_from_evidence(
    address: &crate::TurnAddress,
    evidence: &crate::TurnCancellationEvidence,
) -> crate::TurnCancelRequest {
    crate::TurnCancelRequest {
        address: address.clone(),
        request_id: evidence.request_id.clone(),
        origin: evidence.origin.clone(),
        reason: evidence.reason.clone(),
        undelivered: evidence.undelivered,
        mode: evidence.mode,
    }
}
