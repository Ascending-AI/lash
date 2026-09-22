use super::*;

#[async_trait::async_trait]
impl crate::runtime::process::registry::ProcessLifecycle for TestLocalProcessRegistry {
    async fn complete_process(
        &self,
        process_id: &ProcessId,
        await_output: ProcessAwaitOutput,
        authority: ProcessCompletionAuthority,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        self.write(async |state| {
            let Some(record) = state.managed.get(process_id) else {
                return Err(process_miss(state, process_id));
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
            let planned = self
                .plan_managed_event_append(state, process_id, request)
                .await?;
            let receipt = self
                .apply_managed_event_append(state, process_id, planned)
                .await?;
            let record = state
                .managed
                .get(process_id)
                .expect("event appends target a managed row")
                .record
                .clone();
            Ok(match receipt.realization {
                crate::StoreRealization::Coalesced => {
                    ProcessCompletionOutcome::AlreadyApplied { stored: record }
                }
                crate::StoreRealization::Realized => {
                    parent_end::record_terminal_locked(state, self.clock.timestamp_ms(), &record);
                    ProcessCompletionOutcome::Committed(record)
                }
            })
        })
        .await
    }

    async fn complete_process_with_lease(
        &self,
        lease: &ProcessLease,
        await_output: ProcessAwaitOutput,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        if let Some(error) = self.process_terminal_write_error.lock().await.clone() {
            return Err(error);
        }
        if let Some(outcome) = self.process_terminal_write_outcome.lock().await.take() {
            return Ok(outcome);
        }
        self.write(async |state| {
            let Some(record) = state.managed.get(&lease.process_id) else {
                return Err(process_miss(state, &lease.process_id));
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
            let planned = self
                .plan_managed_event_append(state, &lease.process_id, request)
                .await?;
            // A replayed terminal needs no lease: the stored row is the proof.
            if matches!(
                planned.prepared,
                super::super::ProcessEventAppendPlan::Replay { .. }
            ) {
                self.apply_managed_event_append(state, &lease.process_id, planned)
                    .await?;
                return Ok(ProcessCompletionOutcome::AlreadyApplied {
                    stored: state
                        .managed
                        .get(&lease.process_id)
                        .expect("event appends target a managed row")
                        .record
                        .clone(),
                });
            }
            let lease_live = state.leases.get(&lease.process_id).is_some_and(|current| {
                !current.lease_token.is_empty()
                    && current.owner.same_incarnation(&lease.owner)
                    && current.lease_token == lease.lease_token
                    && current.fencing_token == lease.fencing_token
                    && current.expires_at_epoch_ms > now
            });
            if !lease_live {
                return Err(process_lease_expired(&lease.process_id));
            }
            self.apply_managed_event_append(state, &lease.process_id, planned)
                .await?;
            let record = state
                .managed
                .get(&lease.process_id)
                .expect("event appends target a managed row")
                .record
                .clone();
            parent_end::record_terminal_locked(state, now, &record);
            if let Some(current) = state.leases.get_mut(&lease.process_id) {
                current.owner = crate::LeaseOwnerIdentity::opaque("", "");
                current.lease_token.clear();
                current.claimed_at_epoch_ms = 0;
                current.expires_at_epoch_ms = 0;
            }
            Ok(ProcessCompletionOutcome::Committed(record))
        })
        .await
    }

    async fn record_parent_end(&self, parent: &crate::ParentScope) -> Result<(), PluginError> {
        parent_end::record(self, parent).await
    }

    async fn list_pending_parent_end_plans(
        &self,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<crate::ParentEndPlan>, PluginError> {
        parent_end::list_pending(self, limit).await
    }

    async fn get_parent_end_plan(
        &self,
        parent: &crate::ParentScope,
    ) -> Result<Option<crate::ParentEndPlan>, PluginError> {
        parent_end::get(self, parent).await
    }

    async fn list_parent_end_children(
        &self,
        parent: &crate::ParentScope,
        after: Option<&ProcessId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        parent_end::children(self, parent, after, limit).await
    }

    async fn settle_parent_end_plan(&self, parent: &crate::ParentScope) -> Result<(), PluginError> {
        parent_end::settle(self, parent).await
    }

    async fn list_unrecorded_opener_parents(
        &self,
        after: Option<&str>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<crate::ParentScope>, PluginError> {
        parent_end::list_unrecorded_opener_parents(self, after, limit).await
    }

    async fn record_first_started_with_authority(
        &self,
        process_id: &ProcessId,
        started: ProcessStarted,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessStartOutcome, PluginError> {
        self.write(async |state| {
            let Some(record) = state.managed.get(process_id) else {
                return Err(process_miss(state, process_id));
            };
            validate_in_memory_execution_authority(
                &state.leases,
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
            let request = ProcessEventAppendRequest::first_started(
                process_id,
                &started,
                resumed_from_handover,
            );
            self.append_managed_event(state, process_id, request)
                .await?;
            Ok(ProcessStartOutcome::Started(
                state
                    .managed
                    .get(process_id)
                    .expect("event appends target a managed row")
                    .record
                    .clone(),
            ))
        })
        .await
    }

    async fn request_process_cancel(
        &self,
        process_ref: &crate::ProcessRef,
        origin: crate::CancelOrigin,
        requester: String,
        attribution: Option<crate::RuntimeReplayAttribution>,
    ) -> Result<ProcessRecord, PluginError> {
        self.request_process_cancel_reporting_realization(
            process_ref,
            origin,
            requester,
            attribution,
        )
        .await
        .map(|(record, _)| record)
    }

    async fn request_process_cancel_reporting_realization(
        &self,
        process_ref: &crate::ProcessRef,
        origin: crate::CancelOrigin,
        requester: String,
        attribution: Option<crate::RuntimeReplayAttribution>,
    ) -> Result<(ProcessRecord, crate::StoreRealization), PluginError> {
        if let Some(error) = self.cancel_request_write_error.lock().await.take() {
            return Err(error);
        }
        self.write(async |state| {
            let Some(record) = state.managed.get(&process_ref.process_id) else {
                return Err(process_miss(state, &process_ref.process_id));
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
            match prepare_process_transition(
                &record.record,
                ProcessTransition::RequestCancel(request),
            )? {
                ProcessTransitionPlan::Unchanged => {
                    return Ok((record.record.clone(), crate::StoreRealization::Coalesced));
                }
                ProcessTransitionPlan::Append(mut append) => {
                    if let Some(replay) = append.replay.as_mut() {
                        replay.attribution = attribution;
                    }
                    self.append_managed_event(state, &process_ref.process_id, *append)
                        .await?;
                }
            }
            Ok((
                state
                    .managed
                    .get(&process_ref.process_id)
                    .expect("event appends target a managed row")
                    .record
                    .clone(),
                crate::StoreRealization::Realized,
            ))
        })
        .await
    }

    async fn request_process_abandon(
        &self,
        process_id: &ProcessId,
        request: AbandonRequest,
    ) -> Result<ProcessRecord, PluginError> {
        self.write(async |state| {
            let Some(record) = state.managed.get(process_id) else {
                return Err(process_miss(state, process_id));
            };
            match prepare_process_transition(
                &record.record,
                ProcessTransition::RequestAbandon(request),
            )? {
                ProcessTransitionPlan::Unchanged => return Ok(record.record.clone()),
                ProcessTransitionPlan::Append(append) => {
                    self.append_managed_event(state, process_id, *append)
                        .await?;
                }
            }
            Ok(state
                .managed
                .get(process_id)
                .expect("event appends target a managed row")
                .record
                .clone())
        })
        .await
    }

    async fn record_caller_departure(
        &self,
        process_id: &ProcessId,
    ) -> Result<ProcessRecord, PluginError> {
        self.write(async |state| {
            let Some(record) = state.managed.get(process_id) else {
                return Err(process_miss(state, process_id));
            };
            match prepare_process_transition(
                &record.record,
                ProcessTransition::RecordCallerDeparture,
            )? {
                ProcessTransitionPlan::Unchanged => return Ok(record.record.clone()),
                ProcessTransitionPlan::Append(append) => {
                    self.append_managed_event(state, process_id, *append)
                        .await?;
                }
            }
            Ok(state
                .managed
                .get(process_id)
                .expect("event appends target a managed row")
                .record
                .clone())
        })
        .await
    }

    async fn set_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        wait: WaitState,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        self.write(async |state| {
            let Some(record) = state.managed.get(process_id) else {
                return Err(process_miss(state, process_id));
            };
            validate_in_memory_execution_authority(
                &state.leases,
                process_id,
                &record.record,
                authority,
                None,
                self.clock.timestamp_ms(),
            )?;
            match prepare_process_transition(&record.record, ProcessTransition::EnterWait(wait))? {
                ProcessTransitionPlan::Unchanged => return Ok(record.record.clone()),
                ProcessTransitionPlan::Append(request) => {
                    self.append_managed_event(state, process_id, *request)
                        .await?;
                }
            }
            Ok(state
                .managed
                .get(process_id)
                .expect("event appends target a managed row")
                .record
                .clone())
        })
        .await
    }

    async fn clear_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        self.write(async |state| {
            let Some(record) = state.managed.get(process_id) else {
                return Err(process_miss(state, process_id));
            };
            validate_in_memory_execution_authority(
                &state.leases,
                process_id,
                &record.record,
                authority,
                None,
                self.clock.timestamp_ms(),
            )?;
            match prepare_process_transition(&record.record, ProcessTransition::ClearWait)? {
                ProcessTransitionPlan::Unchanged => return Ok(record.record.clone()),
                ProcessTransitionPlan::Append(request) => {
                    self.append_managed_event(state, process_id, *request)
                        .await?;
                }
            }
            Ok(state
                .managed
                .get(process_id)
                .expect("event appends target a managed row")
                .record
                .clone())
        })
        .await
    }
}
