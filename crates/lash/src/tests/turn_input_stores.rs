use super::*;

/// SnapshotStore serves the pending turn-input lifecycle only as far as one
/// session's own turns need it: every turn is admitted before it is driven
/// (ADR 0069), so acceptance, claim, cancel, and release have to work. Rows are
/// held in memory in enqueue order and are settled by the turn's commit, which
/// this double records without inspecting.
#[async_trait]
impl lash_core::TurnInputStore for SnapshotStore {
    async fn turn_cancel_request_intent(
        &self,
        _address: &lash_core::facade_support::TurnAddress,
    ) -> std::result::Result<lash_core::TurnCancelIntentSnapshot, lash_core::StoreError> {
        Ok(lash_core::TurnCancelIntentSnapshot::Absent)
    }

    fn turn_cancellation_authority(&self) -> Option<lash_core::TurnCancellationAuthority> {
        Some(
            self.turn_cancellation_authority
                .get_or_init(|| {
                    lash_core::TurnCancellationAuthority::new(
                        format!("snapshot-store:{}", uuid::Uuid::new_v4()),
                        Arc::new(
                            lash_core::facade_support::NativeRuntimeEffectController::default(),
                        ),
                    )
                })
                .clone(),
        )
    }

    async fn validate_turn_cancellation_binding(
        &self,
        _session_id: &SessionId,
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _binding_id: &str,
        _admitted_scope: &lash_core::ExecutionScope,
    ) -> std::result::Result<(), lash_core::StoreError> {
        Ok(())
    }

    async fn authorize_turn_cancel_closure(
        &self,
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _authorization: &lash_core::TurnCancelClosureAuthorization,
    ) -> std::result::Result<lash_core::TurnCancelClosureAuthorizationOutcome, lash_core::StoreError>
    {
        Ok(lash_core::TurnCancelClosureAuthorizationOutcome::Authorized)
    }

    async fn pending_turn_cancel_closure_pins(
        &self,
    ) -> std::result::Result<Vec<lash_core::TurnCancelClosureAuthorization>, lash_core::StoreError>
    {
        Ok(Vec::new())
    }

    async fn pending_turn_cancel_closures(
        &self,
        _session_id: &SessionId,
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _binding_id: &str,
        _admitted_scope: &lash_core::ExecutionScope,
    ) -> std::result::Result<Vec<lash_core::TurnCancelClosureAuthorization>, lash_core::StoreError>
    {
        Ok(Vec::new())
    }

    async fn enqueue_pending_turn_input(
        &self,
        input: lash_core::PendingTurnInputDraft,
    ) -> std::result::Result<lash_core::PendingTurnInput, lash_core::store::StoreError> {
        let mut seq = self.pending_turn_input_seq.lock_recover();
        *seq += 1;
        let state = input.ingress.initial_state();
        let stored = lash_core::PendingTurnInput {
            input_id: input
                .input_id
                .unwrap_or_else(|| format!("snapshot-ti-{}", *seq)),
            session_id: input.session_id,
            enqueue_seq: *seq,
            source_key: input.source_key,
            ingress: input.ingress,
            state,
            enqueued_at_ms: now_epoch_ms(),
            input: input.input,
        };
        self.pending_turn_inputs.lock_recover().push(stored.clone());
        Ok(stored)
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<Vec<lash_core::PendingTurnInput>, lash_core::store::StoreError> {
        Ok(self
            .pending_turn_inputs
            .lock_recover()
            .iter()
            .filter(|input| input.session_id == session_id)
            .cloned()
            .collect())
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &SessionId,
        targets: &[lash_core::PendingTurnInputCancelTarget],
    ) -> std::result::Result<
        Vec<lash_core::PendingTurnInputCancelReceipt>,
        lash_core::store::StoreError,
    > {
        let mut pending = self.pending_turn_inputs.lock_recover();
        Ok(targets
            .iter()
            .map(|target| {
                let found = pending.iter().position(|input| {
                    input.session_id == session_id
                        && match target {
                            lash_core::PendingTurnInputCancelTarget::InputId(input_id) => {
                                input.input_id == *input_id
                            }
                            lash_core::PendingTurnInputCancelTarget::SourceKey(source_key) => {
                                input.source_key.as_deref() == Some(source_key.as_str())
                            }
                        }
                });
                let outcome = match found {
                    Some(index) => {
                        lash_core::PendingTurnInputCancelOutcome::Cancelled(pending.remove(index))
                    }
                    None => lash_core::PendingTurnInputCancelOutcome::NotFound,
                };
                lash_core::PendingTurnInputCancelReceipt {
                    target: target.clone(),
                    outcome,
                }
            })
            .collect())
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        _session_id: &SessionId,
        _anchor: &lash_core::PendingTurnInputCancelTarget,
    ) -> std::result::Result<
        lash_core::PendingTurnInputSuffixCancelOutcome,
        lash_core::store::StoreError,
    > {
        unreachable!("SnapshotStore does not serve pending turn input")
    }

    // Turn checkpoints and idle dispatch probe the input queue on every
    // turn; this store's queue is always empty, so claims find nothing.
    async fn claim_active_turn_inputs(
        &self,
        _session_id: &SessionId,
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _owner: &lash_core::LeaseOwnerIdentity,
        _turn_id: &lash_core::TurnId,
        _checkpoint: lash_core::CheckpointKind,
        _max_inputs: usize,
    ) -> std::result::Result<Option<lash_core::TurnInputClaim>, lash_core::store::StoreError> {
        Ok(None)
    }

    // The claim takes the rows out of the pending list and hands them to the
    // caller; the turn's commit settles them, and an abandoned claim puts them
    // back exactly where a real backend's cleared claim columns would.
    async fn claim_next_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> std::result::Result<Option<lash_core::TurnInputClaim>, lash_core::store::StoreError> {
        let mut pending = self.pending_turn_inputs.lock_recover();
        let mut claimed = Vec::new();
        while claimed.len() < max_inputs {
            let Some(index) = pending.iter().position(|input| {
                input.session_id == session_id
                    && input.state == lash_core::TurnInputState::DeferredNextTurn
            }) else {
                break;
            };
            let mut input = pending.remove(index);
            input.state = lash_core::TurnInputState::Accepted;
            claimed.push(input);
        }
        if claimed.is_empty() {
            return Ok(None);
        }
        let generation = self
            .session_execution_lease_generations
            .lock_recover()
            .get(session_id)
            .copied()
            .unwrap_or_default();
        Ok(Some(lash_core::TurnInputClaim {
            session_id: SessionId::from(session_id.to_string()),
            claim_id: format!("snapshot-turn-input-claim-{}", claimed[0].enqueue_seq),
            owner: owner.clone(),
            lease_token: session_execution_lease.lease_token.clone(),
            fencing_token: session_execution_lease.fencing_token,
            session_lease_generation: generation,
            data: lash_core::runtime::TurnInputClaimData {
                mode: lash_core::TurnInputClaimMode::NextTurn,
                inputs: claimed,
                applications: Vec::new(),
            },
        }))
    }

    async fn abandon_turn_input_claim(
        &self,
        claim: &lash_core::TurnInputClaim,
    ) -> std::result::Result<(), lash_core::store::StoreError> {
        let mut pending = self.pending_turn_inputs.lock_recover();
        for input in &claim.inputs {
            let mut restored = input.clone();
            restored.state = lash_core::TurnInputState::DeferredNextTurn;
            pending.push(restored);
        }
        pending.sort_by_key(|input| input.enqueue_seq);
        Ok(())
    }

    // Nothing here holds an input, so the orphan sweep the drain runs finds
    // nothing to repair.
    async fn orphaned_active_turn_ids(
        &self,
        _session_id: &SessionId,
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _scope: lash_core::OrphanedTurnInputScope<'_>,
    ) -> std::result::Result<Vec<lash_core::TurnId>, lash_core::store::StoreError> {
        Ok(Vec::new())
    }

    async fn repair_orphaned_active_turn_inputs(
        &self,
        _session_id: &SessionId,
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _turn_id: &lash_core::TurnId,
        _observed: &lash_core::TurnCancelIntentSnapshot,
        _settlement: Option<&lash_core::TurnCancelClosureSettlement>,
    ) -> std::result::Result<lash_core::TurnCancelRepairResult, lash_core::store::StoreError> {
        Ok(lash_core::TurnCancelRepairResult::Applied(
            Default::default(),
        ))
    }
}

#[async_trait]
impl lash_core::TurnInputStore for BoundSessionStore {
    fn turn_cancellation_authority(&self) -> Option<lash_core::TurnCancellationAuthority> {
        Some(
            self.turn_cancellation_authority
                .get_or_init(|| {
                    lash_core::TurnCancellationAuthority::new(
                        format!("bound-store:{}", uuid::Uuid::new_v4()),
                        Arc::new(
                            lash_core::facade_support::NativeRuntimeEffectController::default(),
                        ),
                    )
                })
                .clone(),
        )
    }

    async fn validate_turn_cancellation_binding(
        &self,
        _session_id: &SessionId,
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _binding_id: &str,
        _admitted_scope: &lash_core::ExecutionScope,
    ) -> std::result::Result<(), lash_core::StoreError> {
        Ok(())
    }

    async fn authorize_turn_cancel_closure(
        &self,
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _authorization: &lash_core::TurnCancelClosureAuthorization,
    ) -> std::result::Result<lash_core::TurnCancelClosureAuthorizationOutcome, lash_core::StoreError>
    {
        unreachable!("BoundSessionStore never authorizes turn cancellation")
    }

    async fn pending_turn_cancel_closure_pins(
        &self,
    ) -> std::result::Result<Vec<lash_core::TurnCancelClosureAuthorization>, lash_core::StoreError>
    {
        Ok(Vec::new())
    }

    async fn pending_turn_cancel_closures(
        &self,
        _session_id: &SessionId,
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _binding_id: &str,
        _admitted_scope: &lash_core::ExecutionScope,
    ) -> std::result::Result<Vec<lash_core::TurnCancelClosureAuthorization>, lash_core::StoreError>
    {
        Ok(Vec::new())
    }

    async fn enqueue_pending_turn_input(
        &self,
        _input: lash_core::PendingTurnInputDraft,
    ) -> std::result::Result<lash_core::PendingTurnInput, lash_core::store::StoreError> {
        unreachable!("BoundSessionStore does not serve pending turn input")
    }

    async fn list_pending_turn_inputs(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<Vec<lash_core::PendingTurnInput>, lash_core::store::StoreError> {
        Ok(Vec::new())
    }

    async fn cancel_pending_turn_inputs(
        &self,
        _session_id: &SessionId,
        _targets: &[lash_core::PendingTurnInputCancelTarget],
    ) -> std::result::Result<
        Vec<lash_core::PendingTurnInputCancelReceipt>,
        lash_core::store::StoreError,
    > {
        unreachable!("BoundSessionStore does not serve pending turn input")
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        _session_id: &SessionId,
        _anchor: &lash_core::PendingTurnInputCancelTarget,
    ) -> std::result::Result<
        lash_core::PendingTurnInputSuffixCancelOutcome,
        lash_core::store::StoreError,
    > {
        unreachable!("BoundSessionStore does not serve pending turn input")
    }

    async fn claim_active_turn_inputs(
        &self,
        _session_id: &SessionId,
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _owner: &lash_core::LeaseOwnerIdentity,
        _turn_id: &lash_core::TurnId,
        _checkpoint: lash_core::CheckpointKind,
        _max_inputs: usize,
    ) -> std::result::Result<Option<lash_core::TurnInputClaim>, lash_core::store::StoreError> {
        unreachable!("BoundSessionStore does not serve pending turn input")
    }

    async fn claim_next_turn_inputs(
        &self,
        _session_id: &SessionId,
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _owner: &lash_core::LeaseOwnerIdentity,
        _max_inputs: usize,
    ) -> std::result::Result<Option<lash_core::TurnInputClaim>, lash_core::store::StoreError> {
        unreachable!("BoundSessionStore does not serve pending turn input")
    }

    async fn abandon_turn_input_claim(
        &self,
        _claim: &lash_core::TurnInputClaim,
    ) -> std::result::Result<(), lash_core::store::StoreError> {
        Ok(())
    }

    // Nothing here holds an input, so the orphan sweep the drain runs finds
    // nothing to repair.
    async fn orphaned_active_turn_ids(
        &self,
        _session_id: &SessionId,
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _scope: lash_core::OrphanedTurnInputScope<'_>,
    ) -> std::result::Result<Vec<lash_core::TurnId>, lash_core::store::StoreError> {
        Ok(Vec::new())
    }

    async fn repair_orphaned_active_turn_inputs(
        &self,
        _session_id: &SessionId,
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _turn_id: &lash_core::TurnId,
        _observed: &lash_core::TurnCancelIntentSnapshot,
        _settlement: Option<&lash_core::TurnCancelClosureSettlement>,
    ) -> std::result::Result<lash_core::TurnCancelRepairResult, lash_core::store::StoreError> {
        Ok(lash_core::TurnCancelRepairResult::Applied(
            Default::default(),
        ))
    }
}
