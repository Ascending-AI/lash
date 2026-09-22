use super::*;

#[async_trait::async_trait]
impl lash_core::ProcessLifecycle for PostgresProcessRegistry {
    async fn complete_process(
        &self,
        process_id: &ProcessId,
        await_output: ProcessAwaitOutput,
        authority: lash_core::ProcessCompletionAuthority,
    ) -> Result<lash_core::ProcessCompletionOutcome, PluginError> {
        // Load (FOR UPDATE), validate the authority against the row's declared
        // disposition, and append the terminal event as one transaction. The
        // `FOR UPDATE` row lock held from the load through the commit is the
        // guard: under READ COMMITTED a concurrent complete→prune→re-register
        // would otherwise change the disposition between a separate read and the
        // append. Locking the row means the disposition we validate is the
        // disposition we append against — the re-registration serialises either
        // fully before our load or fully after our commit.
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let await_output = await_output.with_cancel_origin(
            record
                .cancel_request
                .as_deref()
                .map(|request| request.origin),
        );
        if record.is_terminal() {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(lash_core::ProcessCompletionOutcome::from_stored(
                record,
                &await_output,
            ));
        }
        authority.validate(process_id, record.disposition, &await_output)?;
        let request =
            facade_support::terminal_append_request(process_id, &await_output, Some(&authority));
        let occurred_at_ms = self.clock.timestamp_ms();
        let (_, arm) = apply_process_event_append_tx(
            &mut tx,
            &mut record,
            request,
            occurred_at_ms,
            self.wake_delivery_config,
            ProcessEventWriteAuthorization::Preauthorized,
        )
        .await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(match arm {
            ProcessEventAppendArm::Replayed => {
                lash_core::ProcessCompletionOutcome::AlreadyApplied { stored: record }
            }
            ProcessEventAppendArm::Inserted => {
                lash_core::ProcessCompletionOutcome::Committed(record)
            }
        })
    }

    async fn complete_process_with_lease(
        &self,
        lease: &ProcessLease,
        await_output: ProcessAwaitOutput,
    ) -> Result<lash_core::ProcessCompletionOutcome, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let process_id = lease.process_id.as_str();
        let mut record = require_process_tx(&mut tx, &ProcessId::from(process_id)).await?;
        let await_output = await_output.with_cancel_origin(
            record
                .cancel_request
                .as_deref()
                .map(|request| request.origin),
        );
        if record.is_terminal() {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(lash_core::ProcessCompletionOutcome::from_stored(
                record,
                &await_output,
            ));
        }
        let request = facade_support::terminal_append_request(
            &ProcessId::from(process_id),
            &await_output,
            None,
        );
        // A successful prior terminal append is replay-idempotent even though
        // that transaction already cleared the lease, so the lease fence is
        // re-checked inside the append sequence on the insert arm only.
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        let (_, arm) = apply_process_event_append_tx(
            &mut tx,
            &mut record,
            request,
            now,
            self.wake_delivery_config,
            ProcessEventWriteAuthorization::Lease(lease),
        )
        .await?;
        if arm == ProcessEventAppendArm::Replayed {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(lash_core::ProcessCompletionOutcome::AlreadyApplied { stored: record });
        }
        // The verdict inside the append sequence authorized this release; the
        // statement's predicate is the backstop and `require_fenced_write_applied`
        // returns this site's refusal if it and the locked read ever disagree.
        let released = sqlx::query(process_sql().lease.release.sql())
            .bind(process_id)
            .bind(&lease.lease_token)
            .bind(lease.fencing_token as i64)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected();
        lash_core::store_backend_support::require_fenced_write_applied(
            lash_core::store_backend_support::FencedWrite::ProcessLeaseRelease,
            crate::POSTGRES_BACKEND,
            process_id,
            released,
            || PluginError::ProcessLeaseSuperseded {
                process_id: ProcessId::from(process_id.to_string()),
            },
        )?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lash_core::ProcessCompletionOutcome::Committed(record))
    }

    async fn record_parent_end(&self, parent: &lash_core::ParentScope) -> Result<(), PluginError> {
        parent_end::record(&self.pool, parent, self.clock.timestamp_ms()).await
    }

    async fn list_pending_parent_end_plans(
        &self,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<lash_core::ParentEndPlan>, PluginError> {
        parent_end::list_pending(&self.pool, limit).await
    }

    async fn get_parent_end_plan(
        &self,
        parent: &lash_core::ParentScope,
    ) -> Result<Option<lash_core::ParentEndPlan>, PluginError> {
        parent_end::get(&self.pool, parent).await
    }

    async fn list_parent_end_children(
        &self,
        parent: &lash_core::ParentScope,
        after: Option<&ProcessId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        parent_end::children(&self.pool, parent, after, limit).await
    }

    async fn settle_parent_end_plan(
        &self,
        parent: &lash_core::ParentScope,
    ) -> Result<(), PluginError> {
        parent_end::settle(&self.pool, parent, self.clock.timestamp_ms()).await
    }

    async fn list_unrecorded_opener_parents(
        &self,
        after: Option<&str>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<lash_core::ParentScope>, PluginError> {
        parent_end::list_unrecorded_opener_parents(&self.pool, after, limit).await
    }

    async fn record_first_started_with_authority(
        &self,
        process_id: &ProcessId,
        started: ProcessStarted,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessStartOutcome, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        validate_process_execution_authority_tx(
            &mut tx,
            process_id,
            &record,
            authority,
            Some(&started),
            now,
        )
        .await?;
        match lash_core::runtime::prepare_process_start(&record, &started, authority)? {
            ProcessStartPlan::AlreadyApplied => {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(ProcessStartOutcome::AlreadyApplied(record));
            }
            ProcessStartPlan::AlreadyStarted { by } => {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(ProcessStartOutcome::AlreadyStarted {
                    current: record,
                    by,
                });
            }
            ProcessStartPlan::AttemptsExhausted {
                attempts,
                max_attempts,
            } => {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(ProcessStartOutcome::AttemptsExhausted {
                    current: record,
                    attempts,
                    max_attempts,
                });
            }
            ProcessStartPlan::Append => {}
        }
        let resumed_from_handover = record
            .first_started
            .as_deref()
            .is_some_and(|retained| authority.permits_owner_bound_resume(retained));
        let request =
            ProcessEventAppendRequest::first_started(process_id, &started, resumed_from_handover);
        append_process_event_tx(
            &mut tx,
            &mut record,
            request,
            now,
            self.wake_delivery_config,
        )
        .await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(ProcessStartOutcome::Started(record))
    }

    async fn request_process_cancel(
        &self,
        process_ref: &ProcessRef,
        origin: lash_core::CancelOrigin,
        requester: String,
        attribution: Option<lash_core::RuntimeReplayAttribution>,
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
        process_ref: &ProcessRef,
        origin: lash_core::CancelOrigin,
        requester: String,
        attribution: Option<lash_core::RuntimeReplayAttribution>,
    ) -> Result<(ProcessRecord, lash_core::StoreRealization), PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_ref_tx(&mut tx, process_ref).await?;
        let now = self.clock.timestamp_ms();
        let request = lash_core::CancelRequest::new(origin, requester, now);
        match lash_core::runtime::prepare_process_transition(
            &record,
            ProcessTransition::RequestCancel(request),
        )? {
            ProcessTransitionPlan::Unchanged => {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok((record, lash_core::StoreRealization::Coalesced));
            }
            ProcessTransitionPlan::Append(mut append) => {
                if let Some(replay) = append.replay.as_mut() {
                    replay.attribution = attribution;
                }
                append_process_event_tx(
                    &mut tx,
                    &mut record,
                    *append,
                    now,
                    self.wake_delivery_config,
                )
                .await?;
            }
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok((record, lash_core::StoreRealization::Realized))
    }

    async fn request_process_abandon(
        &self,
        process_id: &ProcessId,
        request: AbandonRequest,
    ) -> Result<ProcessRecord, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        match lash_core::runtime::prepare_process_transition(
            &record,
            ProcessTransition::RequestAbandon(request),
        )? {
            ProcessTransitionPlan::Unchanged => {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(record);
            }
            ProcessTransitionPlan::Append(append) => {
                append_process_event_tx(
                    &mut tx,
                    &mut record,
                    *append,
                    self.clock.timestamp_ms(),
                    self.wake_delivery_config,
                )
                .await?;
            }
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(record)
    }

    async fn record_caller_departure(
        &self,
        process_id: &ProcessId,
    ) -> Result<ProcessRecord, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let append = match lash_core::runtime::prepare_process_transition(
            &record,
            ProcessTransition::RecordCallerDeparture,
        )? {
            ProcessTransitionPlan::Unchanged => {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(record);
            }
            ProcessTransitionPlan::Append(append) => *append,
        };
        append_process_event_tx(
            &mut tx,
            &mut record,
            append,
            self.clock.timestamp_ms(),
            self.wake_delivery_config,
        )
        .await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(record)
    }

    async fn set_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        wait: lash_core::WaitState,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let lease_now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        validate_process_execution_authority_tx(
            &mut tx, process_id, &record, authority, None, lease_now,
        )
        .await?;
        let request = match lash_core::runtime::prepare_process_transition(
            &record,
            ProcessTransition::EnterWait(wait),
        )? {
            ProcessTransitionPlan::Unchanged => {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(record);
            }
            ProcessTransitionPlan::Append(request) => *request,
        };
        append_process_event_tx(
            &mut tx,
            &mut record,
            request,
            self.clock.timestamp_ms(),
            self.wake_delivery_config,
        )
        .await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(record)
    }

    async fn clear_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        validate_process_execution_authority_tx(&mut tx, process_id, &record, authority, None, now)
            .await?;
        let request = match lash_core::runtime::prepare_process_transition(
            &record,
            ProcessTransition::ClearWait,
        )? {
            ProcessTransitionPlan::Unchanged => {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(record);
            }
            ProcessTransitionPlan::Append(request) => *request,
        };
        append_process_event_tx(
            &mut tx,
            &mut record,
            request,
            now,
            self.wake_delivery_config,
        )
        .await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(record)
    }
}
