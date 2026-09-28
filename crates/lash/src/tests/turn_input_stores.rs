use super::*;

/// SnapshotStore serves the pending turn-input lifecycle only as far as one
/// session's own turns need it: every turn is admitted before it is driven
/// (ADR 0069), so acceptance, admission, cancel, and release have to work.
/// Rows are held in memory in enqueue order and are settled by the turn's
/// commit under the root that admitted them.
#[async_trait]
impl lash_core::IngressStore for SnapshotStore {
    async fn turn_cancel_request_intent(
        &self,
        _address: &lash_core::facade_support::TurnAddress,
    ) -> std::result::Result<lash_core::TurnCancelIntentSnapshot, lash_core::StoreError> {
        Ok(lash_core::TurnCancelIntentSnapshot::Absent)
    }

    async fn validate_turn_cancellation_binding(
        &self,
        _session_id: &SessionId,
        _fence: &lash_core::store::DriveFence,
        _binding_id: &str,
        _admitted_scope: &lash_core::ExecutionScope,
    ) -> std::result::Result<(), lash_core::StoreError> {
        Ok(())
    }

    async fn authorize_turn_cancel_closure(
        &self,
        _fence: &lash_core::store::DriveFence,
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
        _fence: &lash_core::store::DriveFence,
        _binding_id: &str,
        _admitted_scope: &lash_core::ExecutionScope,
    ) -> std::result::Result<Vec<lash_core::TurnCancelClosureAuthorization>, lash_core::StoreError>
    {
        Ok(Vec::new())
    }

    async fn enqueue_pending_turn_inputs(
        &self,
        batch: lash_core::PendingTurnInputBatch,
    ) -> std::result::Result<Vec<lash_core::PendingTurnInput>, lash_core::store::StoreError> {
        let mut seq = self.pending_turn_input_seq.lock_recover();
        let mut admitted = Vec::new();
        for input in batch.into_drafts() {
            *seq += 1;
            let state = lash_core::TurnInputState::open(input.ingress.clone());
            let stored = lash_core::PendingTurnInput {
                input_id: input
                    .input_id
                    .unwrap_or_else(|| format!("snapshot-ti-{}", *seq))
                    .into(),
                session_id: input.session_id,
                enqueue_seq: *seq,
                source_key: input.source_key,
                state,
                enqueued_at_ms: now_epoch_ms(),
                run_spec: input
                    .run_spec
                    .hash()
                    .expect("hash the snapshot input's spec"),
                input: input.input,
            };
            self.pending_turn_inputs.lock_recover().push(stored.clone());
            admitted.push(stored);
        }
        Ok(admitted)
    }

    async fn admit_pending_turn_inputs(
        &self,
        batch: lash_core::PendingTurnInputBatch,
        _ingress_claim_ttl_ms: u64,
    ) -> std::result::Result<lash_core::TurnInputAdmission, lash_core::store::StoreError> {
        lash_core::SessionCommitStore::read_session_state_version(self).await?;
        self.enqueue_pending_turn_inputs(batch)
            .await
            .map(lash_core::TurnInputAdmission::Enqueued)
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<Vec<lash_core::PendingTurnInputRead>, lash_core::store::StoreError>
    {
        Ok(self
            .pending_turn_inputs
            .lock_recover()
            .iter()
            .filter(|input| input.session_id == session_id)
            .cloned()
            .map(
                |input| match self.admitted_inputs.lock_recover().get(&input.input_id) {
                    Some(root) => lash_core::PendingTurnInputRead::admitted(input, root.clone()),
                    None => lash_core::PendingTurnInputRead::open(input),
                },
            )
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
                let admitted = self.admitted_inputs.lock_recover();
                let outcome = match found {
                    Some(index) => match admitted.get(&pending[index].input_id) {
                        Some(root) => lash_core::PendingTurnInputCancelOutcome::AlreadyAdmitted {
                            input: pending[index].clone(),
                            root: root.clone(),
                        },
                        None => lash_core::PendingTurnInputCancelOutcome::Cancelled(
                            pending.remove(index),
                        ),
                    },
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

    async fn enqueue_queued_work_with_outcome(
        &self,
        _batch: lash_core::runtime::QueuedWorkBatchDraft,
    ) -> std::result::Result<
        lash_core::runtime::QueuedWorkEnqueueOutcome,
        lash_core::store::StoreError,
    > {
        Err(lash_core::store::StoreError::Backend(
            "queued work is not supported by SnapshotStore".to_string(),
        ))
    }

    // Idle dispatch probes the command lane on every drive; this store keeps
    // no queued work, so the run is always empty.
    async fn open_session_command_run(
        &self,
        _fence: &lash_core::store::DriveFence,
    ) -> std::result::Result<Vec<lash_core::runtime::QueuedWorkBatch>, lash_core::store::StoreError>
    {
        Ok(Vec::new())
    }

    async fn cancel_queued_work_batch(
        &self,
        _session_id: &SessionId,
        _batch_id: &str,
    ) -> std::result::Result<
        Option<lash_core::runtime::QueuedWorkBatch>,
        lash_core::store::StoreError,
    > {
        Ok(None)
    }

    async fn queued_work_batch_completed(
        &self,
        _session_id: &SessionId,
        _batch_id: &str,
    ) -> std::result::Result<bool, lash_core::store::StoreError> {
        Ok(false)
    }

    async fn pending_session_work_ordering(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<
        lash_core::store::PendingSessionWorkOrdering,
        lash_core::store::StoreError,
    > {
        Ok(lash_core::store::PendingSessionWorkOrdering {
            session_command: None,
            turn_input: None,
        })
    }

    async fn list_queued_work(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<Vec<lash_core::runtime::QueuedWorkBatch>, lash_core::store::StoreError>
    {
        Ok(Vec::new())
    }

    async fn list_open_queued_work(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<Vec<lash_core::runtime::QueuedWorkBatch>, lash_core::store::StoreError>
    {
        Ok(Vec::new())
    }
}

#[async_trait]
impl lash_core::IngressStore for BoundSessionStore {
    async fn validate_turn_cancellation_binding(
        &self,
        _session_id: &SessionId,
        _fence: &lash_core::store::DriveFence,
        _binding_id: &str,
        _admitted_scope: &lash_core::ExecutionScope,
    ) -> std::result::Result<(), lash_core::StoreError> {
        Ok(())
    }

    async fn authorize_turn_cancel_closure(
        &self,
        _fence: &lash_core::store::DriveFence,
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
        _fence: &lash_core::store::DriveFence,
        _binding_id: &str,
        _admitted_scope: &lash_core::ExecutionScope,
    ) -> std::result::Result<Vec<lash_core::TurnCancelClosureAuthorization>, lash_core::StoreError>
    {
        Ok(Vec::new())
    }

    async fn enqueue_pending_turn_inputs(
        &self,
        _batch: lash_core::PendingTurnInputBatch,
    ) -> std::result::Result<Vec<lash_core::PendingTurnInput>, lash_core::store::StoreError> {
        unreachable!("BoundSessionStore does not serve pending turn input")
    }

    async fn admit_pending_turn_inputs(
        &self,
        _batch: lash_core::PendingTurnInputBatch,
        _ingress_claim_ttl_ms: u64,
    ) -> std::result::Result<lash_core::TurnInputAdmission, lash_core::store::StoreError> {
        unreachable!("BoundSessionStore does not serve pending turn input")
    }

    async fn list_pending_turn_inputs(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<Vec<lash_core::PendingTurnInputRead>, lash_core::store::StoreError>
    {
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

    async fn enqueue_queued_work_with_outcome(
        &self,
        _batch: lash_core::runtime::QueuedWorkBatchDraft,
    ) -> std::result::Result<
        lash_core::runtime::QueuedWorkEnqueueOutcome,
        lash_core::store::StoreError,
    > {
        unreachable!("BoundSessionStore does not serve queued work")
    }

    async fn open_session_command_run(
        &self,
        _fence: &lash_core::store::DriveFence,
    ) -> std::result::Result<Vec<lash_core::runtime::QueuedWorkBatch>, lash_core::store::StoreError>
    {
        Ok(Vec::new())
    }

    async fn cancel_queued_work_batch(
        &self,
        _session_id: &SessionId,
        _batch_id: &str,
    ) -> std::result::Result<
        Option<lash_core::runtime::QueuedWorkBatch>,
        lash_core::store::StoreError,
    > {
        Ok(None)
    }

    async fn queued_work_batch_completed(
        &self,
        _session_id: &SessionId,
        _batch_id: &str,
    ) -> std::result::Result<bool, lash_core::store::StoreError> {
        Ok(false)
    }

    async fn pending_session_work_ordering(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<
        lash_core::store::PendingSessionWorkOrdering,
        lash_core::store::StoreError,
    > {
        Ok(lash_core::store::PendingSessionWorkOrdering {
            session_command: None,
            turn_input: None,
        })
    }

    async fn list_queued_work(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<Vec<lash_core::runtime::QueuedWorkBatch>, lash_core::store::StoreError>
    {
        Ok(Vec::new())
    }

    async fn list_open_queued_work(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<Vec<lash_core::runtime::QueuedWorkBatch>, lash_core::store::StoreError>
    {
        Ok(Vec::new())
    }
}
