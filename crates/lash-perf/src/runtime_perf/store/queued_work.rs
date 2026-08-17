use super::*;

use std::collections::BTreeSet;

use lash_core::LeaseOwnerIdentity;
use lash_core::runtime::QueuedWorkBatch;

#[derive(Clone)]
pub(super) struct RuntimePerfQueuedBatch {
    pub(super) batch: QueuedWorkBatch,
    pub(super) claim_id: Option<String>,
    pub(super) claim_token: Option<String>,
    pub(super) claim_owner: Option<LeaseOwnerIdentity>,
    pub(super) claim_fencing_token: u64,
    pub(super) claim_session_lease_generation: u64,
}

pub(super) struct SelectedBatchPresence {
    pub(super) requested_ids: BTreeSet<String>,
    pub(super) present_ids: BTreeSet<String>,
    pub(super) already_satisfied_batch_ids: Vec<String>,
}

pub(super) fn selected_batch_presence(
    queued: &[RuntimePerfQueuedBatch],
    session_id: &str,
    batch_ids: &[String],
) -> SelectedBatchPresence {
    let requested_ids = batch_ids.iter().cloned().collect::<BTreeSet<_>>();
    let present_ids = queued
        .iter()
        .filter(|entry| {
            entry.batch.session_id == session_id && requested_ids.contains(&entry.batch.batch_id)
        })
        .map(|entry| entry.batch.batch_id.clone())
        .collect::<BTreeSet<_>>();
    let already_satisfied_batch_ids = batch_ids
        .iter()
        .filter(|batch_id| !present_ids.contains(batch_id.as_str()))
        .cloned()
        .collect();
    SelectedBatchPresence {
        requested_ids,
        present_ids,
        already_satisfied_batch_ids,
    }
}

#[async_trait::async_trait]
impl QueuedWorkStore for RuntimePerfStore {
    async fn enqueue_queued_work(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkBatch, StoreError> {
        Ok(self.enqueue_queued_work_in_memory(batch))
    }

    async fn claim_leading_ready_session_command(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        self.claim_ready_queued_work_perf(
            session_id,
            session_execution_lease,
            owner,
            RuntimePerfQueuedWorkClaimKind::LeadingSessionCommand,
        )
    }

    async fn claim_ready_queued_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: QueuedWorkClaimBoundary,
        policy: lash_core::QueuedWorkClaimPolicy,
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        self.claim_ready_queued_work_perf(
            session_id,
            session_execution_lease,
            owner,
            RuntimePerfQueuedWorkClaimKind::TurnWork { boundary, policy },
        )
    }

    async fn claim_checkpoint_work(
        &self,
        session_id: &str,
        session_execution_lease: &store::SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &lash_core::TurnId,
        checkpoint: lash_core::CheckpointKind,
        max_inputs: usize,
        policy: lash_core::QueuedWorkClaimPolicy,
    ) -> Result<(Option<lash_core::TurnInputClaim>, Option<QueuedWorkClaim>), StoreError> {
        let turn_input_claim = TurnInputStore::claim_active_turn_inputs(
            self,
            session_id,
            session_execution_lease,
            owner,
            turn_id,
            checkpoint,
            max_inputs,
        )
        .await?;
        let queued_work_claim = self.claim_ready_queued_work_perf(
            session_id,
            session_execution_lease,
            owner,
            RuntimePerfQueuedWorkClaimKind::TurnWork {
                boundary: QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
                policy,
            },
        )?;
        Ok((turn_input_claim, queued_work_claim))
    }

    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: QueuedWorkClaimBoundary,
        batch_ids: &[String],
        policy: lash_core::QueuedWorkClaimPolicy,
    ) -> Result<SelectedQueuedWorkClaimOutcome, StoreError> {
        if batch_ids.is_empty() {
            return Ok(SelectedQueuedWorkClaimOutcome::new(None, Vec::new()));
        }
        self.verify_session_execution_lease(session_id, session_execution_lease)?;
        let generation = session_execution_lease.fencing_token;
        let now = current_epoch_ms();
        let mut queued = self.queued_work.lock_recover();
        let queued_work::SelectedBatchPresence {
            requested_ids,
            present_ids,
            already_satisfied_batch_ids,
        } = queued_work::selected_batch_presence(&queued, session_id, batch_ids);
        if present_ids.is_empty() {
            return Ok(SelectedQueuedWorkClaimOutcome::new(
                None,
                already_satisfied_batch_ids,
            ));
        }
        let claim_available = |entry: &RuntimePerfQueuedBatch| {
            entry.claim_token.is_none() || entry.claim_session_lease_generation != generation
        };
        let requested_indices = queued
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.batch.session_id == session_id
                    && entry.batch.available_at_ms <= now
                    && claim_available(entry)
                    && requested_ids.contains(&entry.batch.batch_id)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if requested_indices.len() != present_ids.len() {
            return Ok(SelectedQueuedWorkClaimOutcome::new(
                None,
                already_satisfied_batch_ids,
            ));
        }
        let involved_claim_ids = requested_indices
            .iter()
            .filter_map(|index| queued[*index].claim_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let mut validation_indices = requested_indices.clone();
        if !involved_claim_ids.is_empty() {
            validation_indices.extend(queued.iter().enumerate().filter_map(|(index, entry)| {
                (entry.batch.session_id == session_id
                    && entry.batch.available_at_ms <= now
                    && claim_available(entry)
                    && entry
                        .claim_id
                        .as_ref()
                        .is_some_and(|claim_id| involved_claim_ids.contains(claim_id)))
                .then_some(index)
            }));
            validation_indices.sort_unstable();
            validation_indices.dedup();
        }
        let validation_batch_claims = validation_indices
            .iter()
            .map(|index| {
                (
                    queued[*index].batch.batch_id.clone(),
                    queued[*index].claim_id.clone(),
                )
            })
            .collect::<Vec<_>>();
        let interrupted_indices = store::queued_work::select_interrupted_exact_claim_indices(
            &validation_batch_claims,
            batch_ids,
        )
        .map_err(|required_batch_ids| {
            StoreError::SelectedQueuedWorkRequiresInterruptedComposition { required_batch_ids }
        })?;
        let mut indices = if let Some(interrupted_indices) = interrupted_indices {
            interrupted_indices
                .into_iter()
                .map(|position| validation_indices[position])
                .collect::<Vec<_>>()
        } else {
            let min_enqueue_seq = queued[requested_indices[0]].batch.enqueue_seq;
            let max_enqueue_seq = queued[*requested_indices.last().expect("requested rows exist")]
                .batch
                .enqueue_seq;
            let span_indices = queued
                .iter()
                .enumerate()
                .filter(|(_, entry)| {
                    entry.batch.session_id == session_id
                        && entry.batch.available_at_ms <= now
                        && claim_available(entry)
                        && (min_enqueue_seq..=max_enqueue_seq).contains(&entry.batch.enqueue_seq)
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            for index in &requested_indices {
                if Self::queued_batch_work_class(&queued[*index].batch)?
                    != lash_core::store::QueuedWorkClass::TurnWork
                {
                    return Ok(SelectedQueuedWorkClaimOutcome::new(
                        None,
                        already_satisfied_batch_ids,
                    ));
                }
            }
            let first_requested = requested_indices[0];
            let Some(first_position) = span_indices
                .iter()
                .position(|index| *index == first_requested)
            else {
                return Ok(SelectedQueuedWorkClaimOutcome::new(
                    None,
                    already_satisfied_batch_ids,
                ));
            };
            span_indices[first_position..]
                .iter()
                .copied()
                .take_while(|index| requested_ids.contains(&queued[*index].batch.batch_id))
                .collect::<Vec<_>>()
        };
        let candidates = indices
            .iter()
            .map(|index| {
                let entry = &queued[*index];
                store::queued_work::ClaimCandidate::from_batch(
                    &entry.batch,
                    entry.claim_fencing_token,
                    entry.claim_id.clone(),
                )
            })
            .collect::<Vec<_>>();
        let selected_len = store::queued_work::select_exact_turn_work_claim_prefix(
            &candidates,
            boundary,
            &policy,
            now,
        )?;
        if selected_len == 0 {
            return Ok(SelectedQueuedWorkClaimOutcome::new(
                None,
                already_satisfied_batch_ids,
            ));
        }
        indices.truncate(selected_len);
        let first = &queued[indices[0]];
        let abandon_restore_claim_id = first.claim_id.clone();
        let fencing_token = first.claim_fencing_token.saturating_add(1);
        let claim_id = store::queued_work::derive_claim_id(
            store::queued_work::ClaimIdDialect::PerformanceQueuedWork,
            first.batch.enqueue_seq,
            fencing_token,
        );
        let lease_token = format!(
            "{}:{}:{}:{claim_id}:{now}",
            session_id, owner.owner_id, owner.incarnation_id
        );
        let mut batches = Vec::new();
        for index in indices {
            let entry = &mut queued[index];
            entry.claim_id = Some(claim_id.clone());
            entry.claim_token = Some(lease_token.clone());
            entry.claim_owner = Some(owner.clone());
            entry.claim_fencing_token = entry.claim_fencing_token.saturating_add(1);
            entry.claim_session_lease_generation = generation;
            batches.push(entry.batch.clone());
        }
        Ok(SelectedQueuedWorkClaimOutcome::new(
            Some(QueuedWorkClaim {
                session_id: session_id.to_string(),
                claim_id,
                owner: owner.clone(),
                lease_token,
                fencing_token,
                session_lease_generation: generation,
                data: lash_core::store_backend_support::queued_work_claim_data(
                    batches,
                    abandon_restore_claim_id,
                ),
            }),
            already_satisfied_batch_ids,
        ))
    }

    async fn abandon_queued_work_claim(&self, claim: &QueuedWorkClaim) -> Result<(), StoreError> {
        let mut queued = self.queued_work.lock_recover();
        for entry in queued.iter_mut() {
            if entry.batch.session_id == claim.session_id
                && entry.claim_id.as_deref() == Some(claim.claim_id.as_str())
                && entry.claim_token.as_deref() == Some(claim.lease_token.as_str())
            {
                entry.claim_id =
                    lash_core::store_backend_support::queued_work_abandon_restore_claim_id(claim)
                        .map(str::to_string);
                entry.claim_token = None;
                entry.claim_owner = None;
                entry.claim_session_lease_generation = 0;
            }
        }
        Ok(())
    }

    async fn cancel_queued_work_batch(
        &self,
        session_id: &str,
        batch_id: &str,
    ) -> Result<Option<QueuedWorkBatch>, StoreError> {
        let now = current_epoch_ms();
        let live_generation = self.live_session_lease_generation(session_id, now);
        let mut queued = self.queued_work.lock_recover();
        let Some(index) = queued.iter().position(|entry| {
            entry.batch.session_id == session_id && entry.batch.batch_id == batch_id
        }) else {
            return Ok(None);
        };
        let entry = &queued[index];
        if entry.claim_token.is_some()
            && live_generation == Some(entry.claim_session_lease_generation)
        {
            return Ok(None);
        }
        Ok(Some(queued.remove(index).batch))
    }

    async fn list_queued_work(&self, session_id: &str) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        let mut batches = self
            .queued_work
            .lock_recover()
            .iter()
            .filter(|entry| entry.batch.session_id == session_id)
            .map(|entry| entry.batch.clone())
            .collect::<Vec<_>>();
        batches.sort_by_key(|batch| batch.enqueue_seq);
        Ok(batches)
    }

    async fn pending_session_work_ordering(
        &self,
        session_id: &str,
    ) -> Result<store::PendingSessionWorkOrdering, StoreError> {
        let now = current_epoch_ms();
        let live_generation = self.live_session_lease_generation(session_id, now);
        let session_command = self
            .queued_work
            .lock_recover()
            .iter()
            .filter(|entry| {
                entry.batch.session_id == session_id
                    && (entry.claim_token.is_none()
                        || live_generation != Some(entry.claim_session_lease_generation))
                    // `work_kind = 'control'` is what the SQL projections read,
                    // and `Cancel` deliberately matches neither family.
                    && entry.batch.kind == lash_core::QueuedWorkKind::Control
            })
            .map(|entry| store::PendingWorkOrderingKey {
                enqueued_at_ms: entry.batch.enqueued_at_ms,
                enqueue_seq: entry.batch.enqueue_seq,
            })
            // Within one family the sequence is a real tiebreak: it comes from a
            // single counter.
            .min_by_key(|key| (key.enqueued_at_ms, key.enqueue_seq));
        let turn_input = self
            .pending_turn_inputs
            .lock_recover()
            .iter()
            .filter(|entry| {
                entry.input.session_id == session_id
                    && entry.input.state.is_next_turn_pending()
                    && (entry.claim_token.is_none()
                        || live_generation != Some(entry.claim_session_lease_generation))
            })
            .map(|entry| store::PendingWorkOrderingKey {
                enqueued_at_ms: entry.input.enqueued_at_ms,
                enqueue_seq: entry.input.enqueue_seq,
            })
            // Within one family the sequence is a real tiebreak: it comes from a
            // single counter.
            .min_by_key(|key| (key.enqueued_at_ms, key.enqueue_seq));
        Ok(store::PendingSessionWorkOrdering {
            session_command,
            turn_input,
        })
    }

    async fn list_pending_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        let now = current_epoch_ms();
        let live_generation = self.live_session_lease_generation(session_id, now);
        let mut batches = self
            .queued_work
            .lock_recover()
            .iter()
            .filter(|entry| {
                entry.batch.session_id == session_id
                    && (entry.claim_token.is_none()
                        || live_generation != Some(entry.claim_session_lease_generation))
            })
            .map(|entry| entry.batch.clone())
            .collect::<Vec<_>>();
        batches.sort_by_key(|batch| batch.enqueue_seq);
        Ok(batches)
    }
}
