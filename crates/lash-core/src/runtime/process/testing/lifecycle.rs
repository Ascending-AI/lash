use super::*;

#[async_trait::async_trait]
impl crate::runtime::process::registry::ProcessLifecycle for TestLocalProcessRegistry {
    async fn complete_process(
        &self,
        process_id: &ProcessId,
        await_output: ProcessAwaitOutput,
        authority: ProcessCompletionAuthority,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        self.complete_process_with_parent_end(process_id, await_output, authority, Vec::new())
            .await
    }

    async fn complete_process_with_parent_end(
        &self,
        process_id: &ProcessId,
        await_output: ProcessAwaitOutput,
        authority: ProcessCompletionAuthority,
        actions: Vec<crate::ToolIntentParentEndAction>,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        let _transaction = self.transaction.lock().await;
        // Hold the `managed` lock across load→validate→append so no other
        // completion can complete, prune, and re-register the row with a
        // different disposition between the validation and the terminal append.
        // The row we validate is the row we append to.
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(self.process_miss(process_id).await);
        };
        let await_output = await_output.with_cancel_origin(
            record
                .record
                .cancel_request
                .as_deref()
                .map(|request| request.origin),
        );
        if record.record.is_terminal() {
            return Ok(ProcessCompletionOutcome::from_stored(
                record.record.clone(),
                &await_output,
            ));
        }
        authority.validate(process_id, record.record.disposition, &await_output)?;
        let request = terminal_append_request(process_id, &await_output, Some(&authority));
        let replay_lookup = request
            .replay
            .as_ref()
            .and_then(|replay| record.keyed_events.get(replay.key.as_str()))
            .cloned();
        let last_sequence = record.events.last().map(|event| event.sequence);
        let wake_session_id = self.wake_targets.lock().await.get(process_id).cloned();
        let sender_floor = match wake_session_id.as_ref() {
            Some(target_session_id) => self
                .wake_allocation_floors
                .lock()
                .await
                .get(&(target_session_id.clone(), process_id.clone()))
                .copied(),
            None => None,
        };
        let sequence = super::super::allocate_process_event_sequence(last_sequence, sender_floor)?;
        let now = self.clock.timestamp_ms();
        let prepared = prepare_process_event_append(
            &record.record,
            request,
            sequence,
            last_sequence,
            replay_lookup,
            now,
            wake_session_id.as_ref(),
        )?;
        let outcome = match prepared {
            super::super::ProcessEventAppendPlan::Replay {
                repair_record,
                wake_delivery,
                ..
            } => {
                self.insert_wake_delivery(wake_delivery.as_ref()).await?;
                if let Some(repaired) = repair_record {
                    record.record = repaired;
                    record.change_seq = self.next_change_seq().await;
                }
                ProcessCompletionOutcome::AlreadyApplied {
                    stored: record.record.clone(),
                }
            }
            super::super::ProcessEventAppendPlan::Insert {
                event,
                projected_record,
                wake_delivery,
                ..
            } => {
                self.insert_wake_delivery(wake_delivery.as_ref()).await?;
                self.advance_wake_allocation_floor(wake_session_id.as_ref(), process_id, sequence)
                    .await;
                record.record = projected_record;
                record.change_seq = self.next_change_seq().await;
                record.parent_end_actions = (!actions.is_empty()).then_some(actions);
                if let Some(replay) = event.invocation.replay.clone() {
                    record.keyed_events.insert(replay.key, event.clone());
                }
                record.events.push(event);
                ProcessCompletionOutcome::Committed(record.record.clone())
            }
        };
        Ok(outcome)
    }

    async fn complete_process_with_lease(
        &self,
        lease: &ProcessLease,
        await_output: ProcessAwaitOutput,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        self.complete_process_with_lease_and_parent_end(lease, await_output, Vec::new())
            .await
    }

    async fn complete_process_with_lease_and_parent_end(
        &self,
        lease: &ProcessLease,
        await_output: ProcessAwaitOutput,
        actions: Vec<crate::ToolIntentParentEndAction>,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        if let Some(error) = self.process_terminal_write_error.lock().await.clone() {
            return Err(error);
        }
        if let Some(outcome) = self.process_terminal_write_outcome.lock().await.take() {
            return Ok(outcome);
        }
        let _transaction = self.transaction.lock().await;
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(&lease.process_id) else {
            return Err(self.process_miss(&lease.process_id).await);
        };
        let await_output = await_output.with_cancel_origin(
            record
                .record
                .cancel_request
                .as_deref()
                .map(|request| request.origin),
        );
        if record.record.is_terminal() {
            return Ok(ProcessCompletionOutcome::from_stored(
                record.record.clone(),
                &await_output,
            ));
        }
        let now = self.clock.timestamp_ms();
        let request = terminal_append_request(&lease.process_id, &await_output, None);
        let replay_lookup = request
            .replay
            .as_ref()
            .and_then(|replay| record.keyed_events.get(replay.key.as_str()))
            .cloned();
        let last_sequence = record.events.last().map(|event| event.sequence);
        let wake_session_id = self
            .wake_targets
            .lock()
            .await
            .get(&lease.process_id)
            .cloned();
        let sender_floor = match wake_session_id.as_ref() {
            Some(target_session_id) => self
                .wake_allocation_floors
                .lock()
                .await
                .get(&(target_session_id.clone(), lease.process_id.clone()))
                .copied(),
            None => None,
        };
        let sequence = super::super::allocate_process_event_sequence(last_sequence, sender_floor)?;
        let prepared = prepare_process_event_append(
            &record.record,
            request,
            sequence,
            last_sequence,
            replay_lookup,
            now,
            wake_session_id.as_ref(),
        )?;
        if let super::super::ProcessEventAppendPlan::Replay {
            repair_record,
            wake_delivery,
            ..
        } = prepared
        {
            self.insert_wake_delivery(wake_delivery.as_ref()).await?;
            if let Some(repaired) = repair_record {
                record.record = repaired;
                record.change_seq = self.next_change_seq().await;
            }
            return Ok(ProcessCompletionOutcome::AlreadyApplied {
                stored: record.record.clone(),
            });
        }

        let mut leases = self.leases.lock().await;
        let current = leases
            .get_mut(&lease.process_id)
            .filter(|current| {
                !current.lease_token.is_empty()
                    && current.owner.same_incarnation(&lease.owner)
                    && current.lease_token == lease.lease_token
                    && current.fencing_token == lease.fencing_token
                    && current.expires_at_epoch_ms > now
            })
            .ok_or_else(|| process_lease_expired(&lease.process_id))?;
        match prepared {
            super::super::ProcessEventAppendPlan::Replay { .. } => {
                unreachable!("replay returned above")
            }
            super::super::ProcessEventAppendPlan::Insert {
                event,
                projected_record,
                wake_delivery,
                ..
            } => {
                self.insert_wake_delivery(wake_delivery.as_ref()).await?;
                self.advance_wake_allocation_floor(
                    wake_session_id.as_ref(),
                    &lease.process_id,
                    sequence,
                )
                .await;
                record.record = projected_record;
                record.parent_end_actions = (!actions.is_empty()).then_some(actions);
                if let Some(replay) = event.invocation.replay.clone() {
                    record.keyed_events.insert(replay.key, event.clone());
                }
                record.events.push(event);
            }
        }
        record.change_seq = self.next_change_seq().await;
        current.owner = crate::LeaseOwnerIdentity::opaque("", "");
        current.lease_token.clear();
        current.claimed_at_epoch_ms = 0;
        current.expires_at_epoch_ms = 0;
        Ok(ProcessCompletionOutcome::Committed(record.record.clone()))
    }

    async fn list_pending_parent_end_plans(
        &self,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<crate::ProcessParentEndPlan>, PluginError> {
        parent_end::list(self, limit).await
    }

    async fn get_pending_parent_end_plan(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<crate::ProcessParentEndPlan>, PluginError> {
        parent_end::get(self, process_id).await
    }

    async fn complete_parent_end_plan(&self, process_id: &ProcessId) -> Result<(), PluginError> {
        parent_end::complete(self, process_id).await
    }

    async fn record_first_started_with_authority(
        &self,
        process_id: &ProcessId,
        started: ProcessStarted,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessStartOutcome, PluginError> {
        let _transaction = self.transaction.lock().await;
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(self.process_miss(process_id).await);
        };
        let leases = self.leases.lock().await;
        validate_in_memory_execution_authority(
            &leases,
            process_id,
            &record.record,
            authority,
            Some(&started),
            self.clock.timestamp_ms(),
        )?;
        match prepare_process_start(&record.record, &started, authority)? {
            ProcessStartPlan::AlreadyApplied => {
                return Ok(ProcessStartOutcome::AlreadyApplied(record.record.clone()));
            }
            ProcessStartPlan::AlreadyStarted { by } => {
                return Ok(ProcessStartOutcome::AlreadyStarted {
                    current: record.record.clone(),
                    by,
                });
            }
            ProcessStartPlan::AttemptsExhausted {
                attempts,
                max_attempts,
            } => {
                return Ok(ProcessStartOutcome::AttemptsExhausted {
                    current: record.record.clone(),
                    attempts,
                    max_attempts,
                });
            }
            ProcessStartPlan::Append => {}
        }
        let resumed_from_handover = record
            .record
            .first_started
            .as_deref()
            .is_some_and(|retained| authority.permits_owner_bound_resume(retained));
        let request =
            ProcessEventAppendRequest::first_started(process_id, &started, resumed_from_handover);
        self.append_managed_event(record, request).await?;
        drop(leases);
        Ok(ProcessStartOutcome::Started(record.record.clone()))
    }

    async fn request_process_cancel(
        &self,
        process_ref: &crate::ProcessRef,
        origin: crate::CancelOrigin,
        requester: String,
        attribution: Option<crate::RuntimeReplayAttribution>,
    ) -> Result<ProcessRecord, PluginError> {
        let _transaction = self.transaction.lock().await;
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(&process_ref.process_id) else {
            return Err(self.process_miss(&process_ref.process_id).await);
        };
        if record.record.incarnation != process_ref.incarnation {
            return Err(
                super::super::registry_transitions::process_incarnation_superseded(
                    process_ref,
                    record.record.incarnation,
                ),
            );
        }
        let request = crate::CancelRequest::new(origin, requester, self.clock.timestamp_ms());
        match prepare_process_transition(&record.record, ProcessTransition::RequestCancel(request))?
        {
            ProcessTransitionPlan::Unchanged => return Ok(record.record.clone()),
            ProcessTransitionPlan::Append(mut append) => {
                if let Some(replay) = append.replay.as_mut() {
                    replay.attribution = attribution;
                }
                self.append_managed_event(record, *append).await?;
            }
        }
        Ok(record.record.clone())
    }

    async fn request_process_abandon(
        &self,
        process_id: &ProcessId,
        request: AbandonRequest,
    ) -> Result<ProcessRecord, PluginError> {
        let _transaction = self.transaction.lock().await;
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(self.process_miss(process_id).await);
        };
        match prepare_process_transition(
            &record.record,
            ProcessTransition::RequestAbandon(request),
        )? {
            ProcessTransitionPlan::Unchanged => return Ok(record.record.clone()),
            ProcessTransitionPlan::Append(append) => {
                self.append_managed_event(record, *append).await?;
            }
        }
        Ok(record.record.clone())
    }

    async fn record_caller_departure(
        &self,
        process_id: &ProcessId,
    ) -> Result<ProcessRecord, PluginError> {
        let _transaction = self.transaction.lock().await;
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(self.process_miss(process_id).await);
        };
        match prepare_process_transition(&record.record, ProcessTransition::RecordCallerDeparture)?
        {
            ProcessTransitionPlan::Unchanged => return Ok(record.record.clone()),
            ProcessTransitionPlan::Append(append) => {
                self.append_managed_event(record, *append).await?;
            }
        }
        Ok(record.record.clone())
    }

    async fn set_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        wait: WaitState,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        let _transaction = self.transaction.lock().await;
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(self.process_miss(process_id).await);
        };
        let leases = self.leases.lock().await;
        validate_in_memory_execution_authority(
            &leases,
            process_id,
            &record.record,
            authority,
            None,
            self.clock.timestamp_ms(),
        )?;
        match prepare_process_transition(&record.record, ProcessTransition::EnterWait(wait))? {
            ProcessTransitionPlan::Unchanged => return Ok(record.record.clone()),
            ProcessTransitionPlan::Append(request) => {
                self.append_managed_event(record, *request).await?;
            }
        }
        drop(leases);
        Ok(record.record.clone())
    }

    async fn clear_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        let _transaction = self.transaction.lock().await;
        let mut managed = self.managed.lock().await;
        let Some(record) = managed.get_mut(process_id) else {
            return Err(self.process_miss(process_id).await);
        };
        let leases = self.leases.lock().await;
        validate_in_memory_execution_authority(
            &leases,
            process_id,
            &record.record,
            authority,
            None,
            self.clock.timestamp_ms(),
        )?;
        match prepare_process_transition(&record.record, ProcessTransition::ClearWait)? {
            ProcessTransitionPlan::Unchanged => return Ok(record.record.clone()),
            ProcessTransitionPlan::Append(request) => {
                self.append_managed_event(record, *request).await?;
            }
        }
        drop(leases);
        Ok(record.record.clone())
    }
}
