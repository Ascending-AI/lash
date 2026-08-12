use super::{InMemoryQueuedWorkClaimKind, InMemorySessionStore};
use lash_sansio::sync::MutexExt;

impl InMemorySessionStore {
    pub(super) fn enqueue_queued_work_in_memory(
        &self,
        batch: crate::QueuedWorkBatchDraft,
        enqueued_at_ms: u64,
    ) -> Result<crate::QueuedWorkEnqueueOutcome, crate::store::StoreError> {
        let mut queued = self.queued_work.lock_recover();
        let fences = self.wake_redelivery_fences.lock_recover();
        let mut next_seq = self.queued_work_next_seq.lock_recover();
        Self::enqueue_queued_work_for_state(
            &mut queued,
            &fences,
            &mut next_seq,
            batch,
            enqueued_at_ms,
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
        *next_seq = crate::StoreError::checked_monotonic_increment(
            "queued_work_enqueue_sequence",
            *next_seq,
        )?;
        let batch_id = format!("recording-qwb-{next_seq}");
        let stored = crate::QueuedWorkBatch {
            batch_id: batch_id.clone(),
            session_id: batch.session_id,
            enqueue_seq: *next_seq,
            source_key: batch.source_key,
            delivery_policy: batch.delivery_policy,
            kind: batch.kind,
            authority: batch.authority,
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
        let enqueued_at_ms = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.ensure_session_not_deleted(&batch.session_id)?;
        self.enqueue_queued_work_in_memory(batch, enqueued_at_ms)
            .map(crate::QueuedWorkEnqueueOutcome::into_batch)
    }

    async fn enqueue_queued_work_with_outcome(
        &self,
        batch: crate::QueuedWorkBatchDraft,
    ) -> Result<crate::QueuedWorkEnqueueOutcome, crate::store::StoreError> {
        batch
            .validate_process_wake_source()
            .map_err(crate::store::StoreError::Backend)?;
        let enqueued_at_ms = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.ensure_session_not_deleted(&batch.session_id)?;
        self.enqueue_queued_work_in_memory(batch, enqueued_at_ms)
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
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<Option<crate::QueuedWorkClaim>, crate::store::StoreError> {
        self.claim_ready_queued_work_in_memory(
            session_id,
            session_execution_lease,
            owner,
            InMemoryQueuedWorkClaimKind::TurnWork { boundary, policy },
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
        policy: crate::QueuedWorkClaimPolicy,
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
            policy.max_rows,
        )? {
            return Ok((None, None));
        }

        #[cfg(test)]
        self.checkpoint_write_transaction_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.verify_session_execution_lease(session_id, session_execution_lease, now)?;
        #[cfg(test)]
        self.run_claim_after_lease_validation_hook();
        // Prepare both claim families against private state and publish them
        // together only after every selector, budget, and fencing check has
        // succeeded. This is the in-memory equivalent of the SQL transaction:
        // a queued-work refusal cannot make an active-turn input disappear.
        let mut pending = self.pending_turn_inputs.lock_recover();
        let mut queued = self.queued_work.lock_recover();
        let mut staged_pending = pending.clone();
        let mut staged_queued = queued.clone();
        let turn_input_claim = Self::claim_pending_turn_inputs_for_state(
            &mut staged_pending,
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            crate::TurnInputClaimMode::ActiveTurn {
                turn_id: turn_id.clone(),
                checkpoint,
            },
            now,
        )?;
        let queued_work_claim = Self::claim_ready_queued_work_for_state(
            &mut staged_queued,
            session_id,
            session_execution_lease,
            owner,
            super::InMemoryQueuedWorkClaimKind::TurnWork {
                boundary: crate::QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
                policy,
            },
            now,
        )?;
        *pending = staged_pending;
        *queued = staged_queued;
        Ok((turn_input_claim, queued_work_claim))
    }

    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        session_id: &str,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        boundary: crate::QueuedWorkClaimBoundary,
        batch_ids: &[String],
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<Option<crate::QueuedWorkClaim>, crate::store::StoreError> {
        if batch_ids.is_empty() {
            return Ok(None);
        }
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.verify_session_execution_lease(session_id, session_execution_lease, now)?;
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
        let mut queued = self.queued_work.lock_recover();
        queued.sort_by_key(|entry| entry.batch.enqueue_seq);
        let requested_ids = batch_ids.iter().collect::<std::collections::BTreeSet<_>>();
        if requested_ids.len() != batch_ids.len() {
            return Ok(None);
        }
        let claim_available = |entry: &super::InMemoryQueuedBatch| {
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
        if requested_indices.len() != requested_ids.len() {
            return Ok(None);
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
        let interrupted_indices =
            crate::store::queued_work::select_interrupted_exact_claim_indices(
                &validation_batch_claims,
                batch_ids,
            )
            .map_err(|required_batch_ids| {
                crate::StoreError::SelectedQueuedWorkRequiresInterruptedComposition {
                    required_batch_ids,
                }
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
                    != crate::store::QueuedWorkClass::TurnWork
                {
                    return Ok(None);
                }
            }
            let first_requested = requested_indices[0];
            let Some(first_position) = span_indices
                .iter()
                .position(|index| *index == first_requested)
            else {
                return Ok(None);
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
                crate::store::queued_work::ClaimCandidate::from_batch(
                    &entry.batch,
                    entry.claim_fencing_token,
                    entry.claim_id.clone(),
                )
            })
            .collect::<Vec<_>>();
        let selected_len = crate::store::queued_work::select_turn_work_claim_prefix(
            &candidates,
            boundary,
            policy,
            now,
        )?;
        if selected_len == 0 {
            return Ok(None);
        }
        indices.truncate(selected_len);
        let next_fencing_tokens = indices
            .iter()
            .map(|index| {
                crate::StoreError::checked_monotonic_increment(
                    "queued_work_claim_fencing_token",
                    queued[*index].claim_fencing_token,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let first = &queued[indices[0]];
        let abandon_restore_claim_id = first.claim_id.clone();
        let fencing_token = next_fencing_tokens[0];
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
        for (index, next_fencing_token) in indices.into_iter().zip(next_fencing_tokens) {
            let entry = &mut queued[index];
            entry.claim_id = Some(claim_id.clone());
            entry.claim_token = Some(lease_token.clone());
            entry.claim_owner = Some(owner.clone());
            entry.claim_fencing_token = next_fencing_token;
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
            data: crate::QueuedWorkClaimData {
                batches,
                abandon_restore_claim_id,
            },
        }))
    }

    async fn abandon_queued_work_claim(
        &self,
        claim: &crate::QueuedWorkClaim,
    ) -> Result<(), crate::store::StoreError> {
        let mut queued = self.queued_work.lock_recover();
        for entry in queued.iter_mut() {
            if entry.batch.session_id == claim.session_id
                && entry.claim_id.as_deref() == Some(claim.claim_id.as_str())
                && entry.claim_token.as_deref() == Some(claim.lease_token.as_str())
            {
                #[cfg(test)]
                self.abandoned_queued_work_claim_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                entry.claim_id = claim.abandon_restore_claim_id.clone();
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
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
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

    async fn list_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::QueuedWorkBatch>, crate::store::StoreError> {
        #[cfg(test)]
        self.refuse_injected_counter_defect("queued_work_claim_fencing_token")?;
        let mut batches = self
            .queued_work
            .lock_recover()
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
        #[cfg(test)]
        self.refuse_injected_counter_defect("queued_work_claim_fencing_token")?;
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
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
