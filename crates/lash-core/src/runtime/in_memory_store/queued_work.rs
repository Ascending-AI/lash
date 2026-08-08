use super::{InMemoryQueuedWorkClaimKind, InMemorySessionStore};

impl InMemorySessionStore {
    pub(super) fn enqueue_queued_work_in_memory(
        &self,
        batch: crate::QueuedWorkBatchDraft,
    ) -> Result<crate::QueuedWorkEnqueueOutcome, crate::store::StoreError> {
        let mut queued = self.queued_work.lock().expect("lock queued work");
        let fences = self
            .wake_redelivery_fences
            .lock()
            .expect("lock wake redelivery fences");
        let mut next_seq = self
            .queued_work_next_seq
            .lock()
            .expect("lock queued work seq");
        Self::enqueue_queued_work_for_state(
            &mut queued,
            &fences,
            &mut next_seq,
            batch,
            self.clock.timestamp_ms(),
        )
    }

    pub(super) fn enqueue_queued_work_for_state(
        queued: &mut Vec<super::InMemoryQueuedBatch>,
        wake_redelivery_fences: &std::collections::HashMap<(String, String), u64>,
        next_seq: &mut u64,
        batch: crate::QueuedWorkBatchDraft,
        enqueued_at_ms: u64,
    ) -> Result<crate::QueuedWorkEnqueueOutcome, crate::store::StoreError> {
        if let Some(source_key) = batch.source_key.as_deref()
            && let Some(existing) = queued.iter().find(|entry| {
                entry.batch.session_id == batch.session_id
                    && entry.batch.source_key.as_deref() == Some(source_key)
            })
        {
            return Ok(crate::QueuedWorkEnqueueOutcome::Existing(
                existing.batch.clone(),
            ));
        }
        if let Some(wake_source) = batch.process_wake_source.as_ref()
            && let Some(allocation_floor) = wake_redelivery_fences
                .get(&(batch.session_id.clone(), wake_source.process_id.clone()))
                .copied()
            && wake_source.sequence <= allocation_floor
        {
            return Err(crate::StoreError::ProcessWakeSequenceRewound {
                session_id: batch.session_id.clone(),
                process_id: wake_source.process_id.clone(),
                sequence: wake_source.sequence,
                allocation_floor,
            });
        }
        *next_seq = next_seq.saturating_add(1);
        let batch_id = format!("recording-qwb-{next_seq}");
        let stored = crate::QueuedWorkBatch {
            batch_id: batch_id.clone(),
            session_id: batch.session_id,
            enqueue_seq: *next_seq,
            source_key: batch.source_key,
            delivery_policy: batch.delivery_policy,
            slot_policy: batch.slot_policy,
            merge_key: batch.merge_key,
            available_at_ms: batch.available_at_ms,
            enqueued_at_ms,
            items: batch
                .payloads
                .into_iter()
                .enumerate()
                .map(|(index, payload)| crate::QueuedWorkItem {
                    item_id: format!("{batch_id}:item:{index}"),
                    payload,
                })
                .collect(),
        };
        queued.push(super::InMemoryQueuedBatch {
            batch: stored.clone(),
            claim_id: None,
            claim_token: None,
            claim_owner: None,
            claim_fencing_token: 0,
            claim_session_lease_generation: 0,
        });
        queued.sort_by_key(|entry| entry.batch.enqueue_seq);
        Ok(crate::QueuedWorkEnqueueOutcome::Inserted(stored))
    }
}

#[async_trait::async_trait]
impl crate::store::QueuedWorkStore for InMemorySessionStore {
    async fn enqueue_queued_work(
        &self,
        batch: crate::QueuedWorkBatchDraft,
    ) -> Result<crate::QueuedWorkBatch, crate::store::StoreError> {
        batch
            .validate_process_wake_source()
            .map_err(crate::store::StoreError::Backend)?;
        // This is the in-memory counterpart of the SQL transaction/advisory
        // source lock: floor lookup, live-row lookup, and insertion all run
        // while the single write-transaction mutex is held. Queue completion
        // takes the same mutex before advancing the floor and deleting the row.
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        self.ensure_session_not_deleted(&batch.session_id)?;
        self.enqueue_queued_work_in_memory(batch)
            .map(crate::QueuedWorkEnqueueOutcome::into_batch)
    }

    async fn enqueue_queued_work_with_outcome(
        &self,
        batch: crate::QueuedWorkBatchDraft,
    ) -> Result<crate::QueuedWorkEnqueueOutcome, crate::store::StoreError> {
        batch
            .validate_process_wake_source()
            .map_err(crate::store::StoreError::Backend)?;
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        self.ensure_session_not_deleted(&batch.session_id)?;
        self.enqueue_queued_work_in_memory(batch)
    }

    async fn claim_leading_ready_session_command(
        &self,
        session_id: &str,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
    ) -> Result<Option<crate::QueuedWorkClaim>, crate::store::StoreError> {
        self.claim_ready_queued_work_in_memory(
            session_id,
            session_execution_lease,
            owner,
            InMemoryQueuedWorkClaimKind::LeadingSessionCommand,
        )
    }

    async fn claim_ready_queued_work(
        &self,
        session_id: &str,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        boundary: crate::QueuedWorkClaimBoundary,
        max_batches: usize,
    ) -> Result<Option<crate::QueuedWorkClaim>, crate::store::StoreError> {
        self.claim_ready_queued_work_in_memory(
            session_id,
            session_execution_lease,
            owner,
            InMemoryQueuedWorkClaimKind::TurnWork {
                boundary,
                max_batches,
            },
        )
    }

    async fn claim_checkpoint_work(
        &self,
        session_id: &str,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        turn_id: &crate::TurnId,
        checkpoint: crate::CheckpointKind,
        max_inputs: usize,
        max_batches: usize,
    ) -> Result<
        (
            Option<crate::TurnInputClaim>,
            Option<crate::QueuedWorkClaim>,
        ),
        crate::store::StoreError,
    > {
        #[cfg(test)]
        self.checkpoint_probe_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if !self.checkpoint_work_pending_in_memory(
            session_id,
            session_execution_lease.fencing_token,
            turn_id,
            checkpoint,
            max_inputs,
            max_batches,
        )? {
            return Ok((None, None));
        }

        #[cfg(test)]
        self.checkpoint_write_transaction_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        self.verify_session_execution_lease(session_id, session_execution_lease)?;
        #[cfg(test)]
        self.run_claim_after_lease_validation_hook();
        let turn_input_claim = self.claim_pending_turn_inputs_after_lease_validation(
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            crate::TurnInputClaimMode::ActiveTurn {
                turn_id: turn_id.clone(),
                checkpoint,
            },
        )?;
        let queued_work_claim = self.claim_ready_queued_work_after_lease_validation(
            session_id,
            session_execution_lease,
            owner,
            super::InMemoryQueuedWorkClaimKind::TurnWork {
                boundary: crate::QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
                max_batches,
            },
        )?;
        Ok((turn_input_claim, queued_work_claim))
    }

    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        session_id: &str,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        boundary: crate::QueuedWorkClaimBoundary,
        batch_ids: &[String],
    ) -> Result<Option<crate::QueuedWorkClaim>, crate::store::StoreError> {
        if batch_ids.is_empty() {
            return Ok(None);
        }
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        self.verify_session_execution_lease(session_id, session_execution_lease)?;
        #[cfg(test)]
        self.run_claim_after_lease_validation_hook();
        #[cfg(test)]
        if self
            .fail_next_exact_queue_claim
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(None);
        }
        let generation = session_execution_lease.fencing_token;
        let now = self.clock.timestamp_ms();
        let mut queued = self.queued_work.lock().expect("lock queued work");
        let mut indices = Vec::new();
        for batch_id in batch_ids {
            let Some(index) = queued.iter().position(|entry| {
                entry.batch.session_id == session_id
                    && entry.batch.batch_id == *batch_id
                    && entry.batch.available_at_ms <= now
                    && (entry.claim_token.is_none()
                        || entry.claim_session_lease_generation != generation)
            }) else {
                return Ok(None);
            };
            if Self::queued_batch_work_class(&queued[index].batch)?
                != crate::store::QueuedWorkClass::TurnWork
            {
                return Ok(None);
            }
            indices.push(index);
        }
        let candidates = indices
            .iter()
            .map(|index| {
                let entry = &queued[*index];
                crate::store::queued_work::ClaimCandidate {
                    enqueue_seq: entry.batch.enqueue_seq,
                    claim_fencing_token: entry.claim_fencing_token,
                    work_class: crate::store::QueuedWorkClass::TurnWork,
                    delivery_policy: entry.batch.delivery_policy,
                    slot_policy: entry.batch.slot_policy,
                    merge_key: entry.batch.merge_key.clone(),
                }
            })
            .collect::<Vec<_>>();
        if crate::store::queued_work::select_turn_work_claim_prefix(
            &candidates,
            boundary,
            candidates.len(),
        ) != candidates.len()
        {
            return Ok(None);
        }
        let first = &queued[indices[0]];
        let fencing_token = first.claim_fencing_token.saturating_add(1);
        let claim_id = crate::store::queued_work::derive_claim_id(
            crate::store::queued_work::ClaimIdDialect::RecordingQueuedWork,
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
        Ok(Some(crate::QueuedWorkClaim {
            session_id: session_id.to_string(),
            claim_id,
            owner: owner.clone(),
            lease_token,
            fencing_token,
            session_lease_generation: generation,
            data: crate::QueuedWorkClaimData { batches },
        }))
    }

    async fn abandon_queued_work_claim(
        &self,
        claim: &crate::QueuedWorkClaim,
    ) -> Result<(), crate::store::StoreError> {
        let mut queued = self.queued_work.lock().expect("lock queued work");
        for entry in queued.iter_mut() {
            if entry.batch.session_id == claim.session_id
                && entry.claim_id.as_deref() == Some(claim.claim_id.as_str())
                && entry.claim_token.as_deref() == Some(claim.lease_token.as_str())
            {
                #[cfg(test)]
                self.abandoned_queued_work_claim_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                entry.claim_id = None;
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
    ) -> Result<Option<crate::QueuedWorkBatch>, crate::store::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        let now = self.clock.timestamp_ms();
        let live_generation = self.live_session_lease_generation(session_id, now);
        let mut queued = self.queued_work.lock().expect("lock queued work");
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

    async fn list_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::QueuedWorkBatch>, crate::store::StoreError> {
        let mut batches = self
            .queued_work
            .lock()
            .expect("lock queued work")
            .iter()
            .filter(|entry| entry.batch.session_id == session_id)
            .map(|entry| entry.batch.clone())
            .collect::<Vec<_>>();
        batches.sort_by_key(|batch| batch.enqueue_seq);
        #[cfg(test)]
        if self
            .drop_next_list_queued_work_batch
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            batches.pop();
        }
        Ok(batches)
    }

    async fn list_pending_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::QueuedWorkBatch>, crate::store::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        let now = self.clock.timestamp_ms();
        let live_generation = self.live_session_lease_generation(session_id, now);
        let mut batches = self
            .queued_work
            .lock()
            .expect("lock queued work")
            .iter()
            .filter(|entry| {
                entry.batch.session_id == session_id
                    && (entry.claim_token.is_none()
                        || live_generation != Some(entry.claim_session_lease_generation))
            })
            .map(|entry| entry.batch.clone())
            .collect::<Vec<_>>();
        batches.sort_by_key(|batch| batch.enqueue_seq);
        #[cfg(test)]
        if self
            .drop_next_list_pending_queued_work_batch
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            batches.pop();
        }
        Ok(batches)
    }
}
