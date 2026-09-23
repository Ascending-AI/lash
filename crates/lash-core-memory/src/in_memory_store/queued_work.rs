use super::{InMemoryQueuedWorkClaimKind, InMemorySessionStore};
use crate::SessionId;
use lash_sansio::sync::MutexExt;

impl InMemorySessionStore {
    #[expect(
        clippy::too_many_arguments,
        reason = "matches the fenced claim operation and transaction clock"
    )]
    pub(super) fn claim_exact_run_batches(
        queued: &mut [super::InMemoryQueuedBatch],
        session_id: &SessionId,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        boundary: crate::QueuedWorkClaimBoundary,
        batch_ids: &[crate::BatchId],
        policy: crate::QueuedWorkClaimPolicy,
        now: u64,
    ) -> Result<crate::SelectedQueuedWorkClaimOutcome, crate::StoreError> {
        let generation = session_execution_lease.fencing_token;
        queued.sort_by_key(|entry| entry.batch.enqueue_seq);
        let requested_ids = batch_ids.iter().collect::<std::collections::BTreeSet<_>>();
        let present_ids = queued
            .iter()
            .filter(|entry| {
                entry.batch.session_id == session_id
                    && requested_ids.contains(&entry.batch.batch_id)
            })
            .map(|entry| entry.batch.batch_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let already_satisfied_batch_ids = batch_ids
            .iter()
            .filter(|batch_id| !present_ids.contains(batch_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if present_ids.is_empty() {
            return Ok(crate::SelectedQueuedWorkClaimOutcome::new(
                None,
                already_satisfied_batch_ids,
            ));
        }
        let claim_available =
            |entry: &super::InMemoryQueuedBatch| entry.claim.claimable_by(generation);
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
            return Ok(crate::SelectedQueuedWorkClaimOutcome::new(
                None,
                already_satisfied_batch_ids,
            ));
        }
        let involved_claim_ids = requested_indices
            .iter()
            .filter_map(|index| queued[*index].claim.id())
            .collect::<std::collections::BTreeSet<_>>();
        let mut validation_indices = requested_indices.clone();
        if !involved_claim_ids.is_empty() {
            validation_indices.extend(queued.iter().enumerate().filter_map(|(index, entry)| {
                (entry.batch.session_id == session_id
                    && entry.batch.available_at_ms <= now
                    && claim_available(entry)
                    && entry
                        .claim
                        .id()
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
                    queued[*index].claim.id(),
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
                    required_batch_ids: required_batch_ids
                        .into_iter()
                        .map(crate::BatchId::into_inner)
                        .collect(),
                }
            })?;
        let mut indices = if let Some(interrupted_indices) = interrupted_indices {
            interrupted_indices
                .into_iter()
                .map(|position| validation_indices[position])
                .collect::<Vec<_>>()
        } else {
            let min_enqueue_seq = queued[requested_indices[0]].batch.enqueue_seq;
            let Some(&last_index) = requested_indices.last() else {
                return Ok(crate::SelectedQueuedWorkClaimOutcome::new(
                    None,
                    already_satisfied_batch_ids,
                ));
            };
            let max_enqueue_seq = queued[last_index].batch.enqueue_seq;
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
                if queued[*index].batch.work_class() != crate::store::QueuedWorkClass::TurnWork {
                    return Ok(crate::SelectedQueuedWorkClaimOutcome::new(
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
                return Ok(crate::SelectedQueuedWorkClaimOutcome::new(
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
                crate::store::queued_work::ClaimCandidate::from_batch(
                    &entry.batch,
                    entry.claim.fencing_token,
                    entry.claim.id(),
                    entry.claim.token(),
                )
            })
            .collect::<Vec<_>>();
        let selected_len = match crate::store::queued_work::select_exact_turn_work_claim_prefix(
            &candidates,
            boundary,
            &policy,
            now,
        )? {
            crate::store::TurnWorkClaimPrefix::Selected { len } => len,
            crate::store::TurnWorkClaimPrefix::Refused { .. } => {
                return Ok(crate::SelectedQueuedWorkClaimOutcome::new(
                    None,
                    already_satisfied_batch_ids,
                ));
            }
        };
        indices.truncate(selected_len);
        let observations = indices
            .iter()
            .map(|&index| {
                let entry = &queued[index];
                crate::store::claim_plan::QueuedWorkClaimRow {
                    candidate: crate::store::queued_work::ClaimCandidate::from_batch(
                        &entry.batch,
                        entry.claim.fencing_token,
                        entry.claim.id(),
                        entry.claim.token(),
                    ),
                    batch: entry.batch.clone(),
                    claim_token: entry.claim.token(),
                    claim_session_lease_generation: entry
                        .claim
                        .diagnostic_generation()
                        .unwrap_or(0),
                }
            })
            .collect::<Vec<_>>();
        // The SQL backends validate fencing tokens over the full candidate
        // span, not just the selected prefix; `candidates` is that span
        // (FIG-1065).
        let plan = match crate::store::claim_plan::plan_queued_work_claim(
            crate::store::queued_work::ClaimIdDialect::RecordingQueuedWork,
            session_id,
            owner,
            generation,
            now,
            observations,
            &candidates,
        )? {
            // Empty and Defer both report no claim: a row held by this
            // generation was filtered out of `indices` by `claimable_by`
            // before selection (FIG-1065).
            crate::store::claim_plan::ClaimPlanDecision::Empty
            | crate::store::claim_plan::ClaimPlanDecision::Defer => {
                return Ok(crate::SelectedQueuedWorkClaimOutcome::new(
                    None,
                    already_satisfied_batch_ids,
                ));
            }
            crate::store::claim_plan::ClaimPlanDecision::Complete(plan) => plan,
        };
        // Assemble the claim record before mutating: it is the plan's only
        // remaining fallible step, and this path writes the live rows
        // directly rather than a staged copy.
        let writes = plan.writes().to_vec();
        let claim = plan.into_claim()?;
        for (&index, write) in indices.iter().zip(&writes) {
            queued[index].claim.acquire(
                claim.claim_id.clone(),
                claim.lease_token.clone(),
                owner.clone(),
                generation,
                write.next_claim_fencing_token,
            );
        }
        Ok(crate::SelectedQueuedWorkClaimOutcome::new(
            Some(claim),
            already_satisfied_batch_ids,
        ))
    }

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
                .get(&(
                    batch.session_id.clone().to_string(),
                    wake_source.process_id.clone().to_string(),
                ))
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
        let batch_id = crate::BatchId::new(format!("recording-qwb-{next_seq}"));
        let kind = batch.kind();
        let stored = crate::QueuedWorkBatch {
            batch_id: batch_id.clone(),
            session_id: batch.session_id,
            enqueue_seq: *next_seq,
            source_key: batch.source_key,
            delivery_policy: batch.delivery_policy,
            kind,
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
            claim: super::ClaimHold::with_fencing_token(0),
        });
        queued.sort_by_key(|entry| entry.batch.enqueue_seq);
        Ok(crate::QueuedWorkEnqueueOutcome::Inserted(stored))
    }
}

#[async_trait::async_trait]
impl crate::store::QueuedWorkStore for InMemorySessionStore {
    async fn pending_queued_run(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<crate::store::QueuedRunAdmission>, crate::StoreError> {
        let _transaction = self.write_transaction.lock_recover();
        self.ensure_session_not_deleted(session_id)?;
        Ok(self
            .queued_runs
            .lock_recover()
            .values()
            .find(|run| run.scope.session_id() == Some(session_id) && run.terminal.is_none())
            .cloned())
    }

    async fn select_queued_run(
        &self,
        fence: &crate::SessionExecutionLeaseAuthority,
        scope: &crate::ExecutionScope,
        owner: &crate::LeaseOwnerIdentity,
        max_inputs: usize,
        configuration: &crate::PersistedSessionConfig,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<crate::store::SelectedQueuedRun, crate::StoreError> {
        use crate::store::{QueuedRunMember, QueuedRunRequest};
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.ensure_session_not_deleted(&fence.session_id)?;
        self.verify_session_execution_lease(&fence.session_id, fence, now)?;
        #[cfg(any(test, feature = "testing"))]
        self.run_claim_after_lease_validation_hook();
        let conflict = || crate::StoreError::QueuedRunConflict {
            session_id: fence.session_id.clone(),
        };
        let mut runs = self.queued_runs.lock_recover();
        let mut admission = runs.get(scope).ok_or_else(conflict)?.clone();
        if scope.session_id() != Some(&fence.session_id) || admission.terminal.is_some() {
            return Err(conflict());
        }
        if admission.members.is_some() && &admission.configuration != configuration {
            return Err(crate::StoreError::QueuedRunConfigurationChanged {
                session_id: fence.session_id.clone(),
            });
        }
        let mut pending = self.pending_turn_inputs.lock_recover();
        let mut batches = self.queued_work.lock_recover();
        let mut staged_inputs = pending.clone();
        let mut staged_batches = batches.clone();
        let mut already_satisfied = admission.already_satisfied_batch_ids();
        let mut refusal = None;
        let (inputs, queued) = if let Some(members) = &admission.members {
            let input_ids: Vec<_> = members
                .iter()
                .filter_map(|member| {
                    if let QueuedRunMember::Input(id) = member {
                        Some(id)
                    } else {
                        None
                    }
                })
                .collect();
            let batch_ids: Vec<_> = members
                .iter()
                .filter_map(|member| {
                    if let QueuedRunMember::Batch(id) = member {
                        Some(id)
                    } else {
                        None
                    }
                })
                .collect();
            #[cfg(any(test, feature = "testing"))]
            if !batch_ids.is_empty()
                && self
                    .fail_next_exact_queue_claim
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(crate::StoreError::Backend(
                    "injected committed handoff claim failure".into(),
                ));
            }
            let inputs =
                self.reclaim_run_inputs(&mut staged_inputs, &input_ids, fence, owner, now)?;
            let queued =
                self.reclaim_run_batches(&mut staged_batches, &batch_ids, fence, owner, now)?;
            (inputs, queued)
        } else {
            let inputs = if matches!(admission.request, QueuedRunRequest::Automatic) {
                Self::claim_pending_turn_inputs_for_state(
                    &mut staged_inputs,
                    &fence.session_id,
                    fence,
                    owner,
                    max_inputs,
                    crate::TurnInputClaimMode::NextTurn,
                    now,
                )?
            } else {
                None
            };
            let queued = if inputs.is_none() {
                if let QueuedRunRequest::Selected { batch_ids } = &admission.request {
                    let result = Self::claim_exact_run_batches(
                        &mut staged_batches,
                        &fence.session_id,
                        fence,
                        owner,
                        crate::QueuedWorkClaimBoundary::Idle,
                        batch_ids,
                        policy,
                        now,
                    )?;
                    already_satisfied = result.already_satisfied_batch_ids;
                    let claim = result.claim;
                    if claim.as_ref().map_or(0, |claim| claim.batches.len())
                        + already_satisfied.len()
                        != batch_ids.len()
                    {
                        return Err(crate::StoreError::SelectedQueuedRunIncomplete {
                            unclaimed_batch_ids: batch_ids
                                .iter()
                                .filter(|id| {
                                    !already_satisfied.contains(id)
                                        && !claim.as_ref().is_some_and(|claim| {
                                            claim.batches.iter().any(|batch| batch.batch_id == *id)
                                        })
                                })
                                .cloned()
                                .collect(),
                        });
                    }
                    claim
                } else {
                    let result = Self::claim_ready_queued_work_for_state(
                        &mut staged_batches,
                        &fence.session_id,
                        fence,
                        owner,
                        InMemoryQueuedWorkClaimKind::TurnWork {
                            boundary: crate::QueuedWorkClaimBoundary::Idle,
                            policy,
                        },
                        now,
                    )?;
                    refusal = result.refusal();
                    result.claim()
                }
            } else {
                None
            };
            let members: Vec<_> = inputs
                .iter()
                .flat_map(|claim| {
                    claim
                        .inputs
                        .iter()
                        .map(|input| QueuedRunMember::Input(input.input_id.clone()))
                })
                .chain(queued.iter().flat_map(|claim| {
                    claim
                        .batches
                        .iter()
                        .map(|batch| QueuedRunMember::Batch(batch.batch_id.clone()))
                }))
                .collect();
            admission.configuration = configuration.clone();
            admission.initial_members = Some(members.clone());
            admission.members = Some(members);
            admission.revision = crate::StoreError::checked_monotonic_increment(
                "queued_run_revision",
                admission.revision,
            )?;
            (inputs.into_iter().collect(), queued.into_iter().collect())
        };
        runs.insert(scope.clone(), admission.clone());
        *pending = staged_inputs;
        *batches = staged_batches;
        Ok(crate::store::SelectedQueuedRun {
            admission: admission.clone(),
            inputs,
            queued,
            already_satisfied,
            refusal,
        })
    }

    async fn settle_queued_run(
        &self,
        fence: &crate::SessionExecutionLeaseAuthority,
        settlement: crate::store::QueuedRunCommit,
    ) -> Result<crate::store::QueuedRunAdmission, crate::StoreError> {
        let _transaction = self.write_transaction.lock_recover();
        self.ensure_session_not_deleted(&fence.session_id)?;
        self.verify_session_execution_lease(&fence.session_id, fence, self.clock.timestamp_ms())?;
        let conflict = || crate::StoreError::QueuedRunConflict {
            session_id: fence.session_id.clone(),
        };
        if settlement.scope.session_id() != Some(&fence.session_id)
            || !matches!(
                settlement.progress,
                crate::store::QueuedRunProgress::Settle { .. }
                    | crate::store::QueuedRunProgress::ForgetUnworked
            )
        {
            return Err(conflict());
        }
        let mut runs = self.queued_runs.lock_recover();
        let run = runs.get_mut(&settlement.scope).ok_or_else(conflict)?;
        use crate::store::{QueuedRunMember, QueuedRunProgress, QueuedRunTerminal};
        match &settlement.progress {
            QueuedRunProgress::Settle {
                terminal: QueuedRunTerminal::Failed { .. },
            } => {}
            QueuedRunProgress::Settle {
                terminal: QueuedRunTerminal::Empty,
            } if run.members.as_ref().is_some_and(Vec::is_empty)
                && run.withheld_members.is_empty()
                && run.assigned_members.is_empty() => {}
            QueuedRunProgress::ForgetUnworked if run.can_forget_unworked() => {}
            _ => return Err(conflict()),
        }
        let settled = run.advance(&settlement, &[])?;
        if run.terminal.is_some() {
            return Ok(settled);
        }
        let members: Vec<_> = run
            .initial_members
            .iter()
            .flatten()
            .chain(run.members.iter().flatten())
            .chain(run.withheld_members.iter())
            .chain(run.assigned_members.iter())
            .collect();
        self.queued_work.lock_recover().retain(|entry| {
            entry.batch.session_id != fence.session_id
                || !members.contains(&&QueuedRunMember::Batch(entry.batch.batch_id.clone()))
        });
        for entry in self
            .pending_turn_inputs
            .lock_recover()
            .iter_mut()
            .filter(|entry| {
                entry.input.session_id == fence.session_id
                    && members.contains(&&QueuedRunMember::Input(entry.input.input_id.clone()))
            })
        {
            if !entry.input.state.is_terminal()
                && !(entry.input.state == crate::TurnInputState::DeferredNextTurn
                    && run
                        .assigned_members
                        .contains(&QueuedRunMember::Input(entry.input.input_id.clone())))
            {
                entry.input.state = crate::TurnInputState::Cancelled(entry.input.state.ingress());
                entry.claim.release();
            }
        }
        if matches!(settlement.progress, QueuedRunProgress::ForgetUnworked) {
            runs.remove(&settlement.scope);
        } else {
            *run = settled.clone();
        }
        Ok(settled)
    }

    async fn begin_or_resume_queued_run(
        &self,
        fence: &crate::SessionExecutionLeaseAuthority,
        request: crate::store::BeginQueuedRun,
    ) -> Result<crate::store::QueuedRunAdmission, crate::StoreError> {
        request.validate(fence)?;
        let _transaction = self.write_transaction.lock_recover();
        self.ensure_session_not_deleted(&request.session_id)?;
        self.verify_session_execution_lease(&request.session_id, fence, self.clock.timestamp_ms())?;
        let mut runs = self.queued_runs.lock_recover();
        if let Some(admitted) = request
            .identity
            .as_ref()
            .and_then(|scope| runs.get_mut(scope))
        {
            let resumed = request.resume(admitted)?;
            *admitted = resumed.clone();
            return Ok(resumed);
        }
        if let Some(admitted) = runs.values_mut().find(|run| {
            run.scope.session_id() == Some(&request.session_id) && run.terminal.is_none()
        }) {
            let resumed = request.resume(admitted)?;
            *admitted = resumed.clone();
            return Ok(resumed);
        }
        let actual = self
            .session_head_meta
            .lock_recover()
            .as_ref()
            .map_or(0, |head| head.head_revision);
        if actual != request.expected_head_revision {
            return Err(crate::StoreError::HeadRevisionConflict {
                expected: request.expected_head_revision,
                actual,
            });
        }
        let admission = request.admit(uuid::Uuid::new_v4().to_string());
        runs.insert(admission.scope.clone(), admission.clone());
        Ok(admission)
    }

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
        session_id: &SessionId,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
    ) -> Result<Option<crate::QueuedWorkClaim>, crate::store::StoreError> {
        self.claim_ready_queued_work_in_memory(
            session_id,
            session_execution_lease,
            owner,
            InMemoryQueuedWorkClaimKind::LeadingSessionCommand,
        )
        .map(crate::QueuedWorkClaimOutcome::claim)
    }

    async fn claim_ready_queued_work(
        &self,
        session_id: &SessionId,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        boundary: crate::QueuedWorkClaimBoundary,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<crate::QueuedWorkClaimOutcome, crate::store::StoreError> {
        self.claim_ready_queued_work_in_memory(
            session_id,
            session_execution_lease,
            owner,
            InMemoryQueuedWorkClaimKind::TurnWork { boundary, policy },
        )
    }

    async fn claim_checkpoint_work(
        &self,
        session_id: &SessionId,
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
        #[cfg(any(test, feature = "testing"))]
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

        #[cfg(any(test, feature = "testing"))]
        self.checkpoint_write_transaction_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.verify_session_execution_lease(session_id, session_execution_lease, now)?;
        #[cfg(any(test, feature = "testing"))]
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
        )?
        .claim();
        self.assign_checkpoint_members(
            session_id,
            turn_id,
            turn_input_claim.as_ref(),
            queued_work_claim.as_ref(),
        );
        *pending = staged_pending;
        *queued = staged_queued;
        Ok((turn_input_claim, queued_work_claim))
    }

    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        session_id: &SessionId,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        boundary: crate::QueuedWorkClaimBoundary,
        batch_ids: &[crate::BatchId],
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<crate::SelectedQueuedWorkClaimOutcome, crate::store::StoreError> {
        if batch_ids.is_empty() {
            return Ok(crate::SelectedQueuedWorkClaimOutcome::new(None, Vec::new()));
        }
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.verify_session_execution_lease(session_id, session_execution_lease, now)?;
        #[cfg(any(test, feature = "testing"))]
        self.run_claim_after_lease_validation_hook();
        #[cfg(any(test, feature = "testing"))]
        if self
            .fail_next_exact_queue_claim
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(crate::SelectedQueuedWorkClaimOutcome::new(None, Vec::new()));
        }
        Self::claim_exact_run_batches(
            &mut self.queued_work.lock_recover(),
            session_id,
            session_execution_lease,
            owner,
            boundary,
            batch_ids,
            policy,
            now,
        )
    }

    async fn abandon_queued_work_claim(
        &self,
        claim: &crate::QueuedWorkClaim,
    ) -> Result<(), crate::store::StoreError> {
        let mut queued = self.queued_work.lock_recover();
        for entry in queued.iter_mut() {
            if entry.batch.session_id == claim.session_id
                && entry.claim.owned_by(&claim.claim_id, &claim.lease_token)
            {
                #[cfg(any(test, feature = "testing"))]
                self.abandoned_queued_work_claim_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                entry.claim.restore(
                    claim.abandon_restore_claim_id.clone(),
                    claim
                        .abandon_restore_claim_token
                        .as_deref()
                        .map(str::to_string),
                );
            }
        }
        Ok(())
    }

    async fn cancel_queued_work_batch(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<crate::QueuedWorkBatch>, crate::store::StoreError> {
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        let live_generation = self.live_session_lease_generation(session_id, now);
        let runs = self.queued_runs.lock_recover();
        let mut queued = self.queued_work.lock_recover();
        let Some(index) = queued.iter().position(|entry| {
            entry.batch.session_id == session_id && entry.batch.batch_id == batch_id
        }) else {
            return Ok(None);
        };
        let entry = &queued[index];
        if runs.values().any(|run| {
            run.scope.session_id() == Some(session_id)
                && run.terminal.is_none()
                && run.owns_member(&crate::store::QueuedRunMember::Batch(
                    entry.batch.batch_id.clone(),
                ))
        }) {
            return Ok(None);
        }
        if entry.claim.token().is_some() && entry.claim.live_under(live_generation) {
            return Ok(None);
        }
        Ok(Some(queued.remove(index).batch))
    }

    async fn queued_work_batch_completed(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<bool, crate::StoreError> {
        let marker = crate::store_backend_support::session_command_batch_completion_key(
            session_id, batch_id,
        )?;
        Ok(self
            .runtime_turn_commits
            .lock_recover()
            .contains_key(&(session_id.clone(), marker)))
    }

    async fn list_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::QueuedWorkBatch>, crate::store::StoreError> {
        #[cfg(any(test, feature = "testing"))]
        self.refuse_injected_counter_defect("queued_work_claim_fencing_token")?;
        let mut batches = self
            .queued_work
            .lock_recover()
            .iter()
            .filter(|entry| entry.batch.session_id == session_id)
            .map(|entry| entry.batch.clone())
            .collect::<Vec<_>>();
        batches.sort_by_key(|batch| batch.enqueue_seq);
        #[cfg(any(test, feature = "testing"))]
        if self
            .drop_next_list_queued_work_batch
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            batches.pop();
        }
        Ok(batches)
    }

    async fn pending_session_work_ordering(
        &self,
        session_id: &SessionId,
    ) -> Result<crate::store::PendingSessionWorkOrdering, crate::store::StoreError> {
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        let live_generation = self.live_session_lease_generation(session_id, now);
        let session_command = self
            .queued_work
            .lock_recover()
            .iter()
            .filter(|entry| {
                entry.batch.session_id == session_id && (!entry.claim.live_under(live_generation))
            })
            .filter_map(|entry| {
                // SQL projections compare against the stable `Control` wire value.
                (entry.batch.kind == crate::QueuedWorkKind::Control).then_some(
                    crate::store::PendingWorkOrderingKey {
                        enqueued_at_ms: entry.batch.enqueued_at_ms,
                        enqueue_seq: entry.batch.enqueue_seq,
                    },
                )
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
                    && (!entry.claim.live_under(live_generation))
            })
            .map(|entry| crate::store::PendingWorkOrderingKey {
                enqueued_at_ms: entry.input.enqueued_at_ms,
                enqueue_seq: entry.input.enqueue_seq,
            })
            // Within one family the sequence is a real tiebreak: it comes from a
            // single counter.
            .min_by_key(|key| (key.enqueued_at_ms, key.enqueue_seq));
        Ok(crate::store::PendingSessionWorkOrdering {
            session_command,
            turn_input,
        })
    }

    async fn list_pending_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::QueuedWorkBatch>, crate::store::StoreError> {
        #[cfg(any(test, feature = "testing"))]
        self.list_pending_queued_work_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        #[cfg(any(test, feature = "testing"))]
        self.refuse_injected_counter_defect("queued_work_claim_fencing_token")?;
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        let live_generation = self.live_session_lease_generation(session_id, now);
        let mut batches = self
            .queued_work
            .lock_recover()
            .iter()
            .filter(|entry| {
                entry.batch.session_id == session_id && (!entry.claim.live_under(live_generation))
            })
            .map(|entry| entry.batch.clone())
            .collect::<Vec<_>>();
        batches.sort_by_key(|batch| batch.enqueue_seq);
        #[cfg(any(test, feature = "testing"))]
        if self
            .drop_next_list_pending_queued_work_batch
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            batches.pop();
        }
        Ok(batches)
    }
}
