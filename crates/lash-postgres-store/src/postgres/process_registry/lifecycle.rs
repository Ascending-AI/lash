use super::*;

#[async_trait::async_trait]
impl lash_core_execution::ProcessLifecycle for PostgresProcessRegistry {
    async fn complete_process_with_prelude(
        &self,
        process_id: &ProcessId,
        await_output: ProcessAwaitOutput,
        prelude: Vec<ProcessEventAppendRequest>,
        authority: lash_core_execution::ProcessCompletionAuthority,
    ) -> Result<lash_core_execution::ProcessCompletionOutcome, PluginError> {
        // Load (FOR UPDATE), validate the authority against the row's input
        // class, and append the run's terminal batch (`prelude`, then the
        // terminal event, one process save; FIG-3571) as one transaction. The
        // `FOR UPDATE` row lock held from the load through the commit is the
        // guard: under READ COMMITTED a concurrent complete→prune→re-register
        // would otherwise change the input class between a separate read and the
        // append. Locking the row means the input class we validate is the one
        // we append against — the re-registration serialises either
        // fully before our load or fully after our commit.
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let await_output = await_output.with_cancel_origin(
            record
                .cancel_request
                .as_deref()
                .map(|request| request.origin),
        );
        authority.validate(&record)?;
        if record.is_terminal() {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(lash_core_execution::ProcessCompletionOutcome::from_stored(
                record,
                &await_output,
            ));
        }
        let occurred_at_ms = self.clock.timestamp_ms();
        let mut batch = ProcessEventBatch::for_fleet(self.fence.fleet());
        for request in prelude {
            batch
                .stage(&mut tx, &mut record, request, occurred_at_ms)
                .await?;
        }
        let request =
            facade_support::terminal_append_request(process_id, &await_output, Some(&authority));
        let (_, arm) = batch
            .stage_arm(&mut tx, &mut record, request, occurred_at_ms)
            .await?;
        batch.commit(&mut tx, &record).await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(match arm {
            ProcessEventAppendArm::Replayed => {
                lash_core_execution::ProcessCompletionOutcome::AlreadyApplied { stored: record }
            }
            ProcessEventAppendArm::Inserted => {
                lash_core_execution::ProcessCompletionOutcome::Committed(record)
            }
        })
    }

    async fn record_parent_end(
        &self,
        parent: &lash_core_execution::ScopeId,
    ) -> Result<(), PluginError> {
        parent_end::record(&self.pool, &self.fence, parent, self.clock.timestamp_ms()).await
    }

    async fn settle_terminal_publication(
        &self,
        process_id: &ProcessId,
    ) -> Result<bool, PluginError> {
        super::terminal_publication::settle(
            &self.pool,
            &self.fence,
            process_id,
            self.clock.timestamp_ms(),
        )
        .await
    }

    async fn terminal_publication(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<lash_core_execution::ProcessTerminalPublication>, PluginError> {
        super::terminal_publication::get(&self.pool, process_id).await
    }

    async fn get_parent_end_plan(
        &self,
        parent: &lash_core_execution::ScopeId,
    ) -> Result<Option<lash_core_execution::ParentEndPlan>, PluginError> {
        parent_end::get(&self.pool, parent, self.fence.fleet()).await
    }

    async fn get_parent_end_plan_by_key(
        &self,
        parent_kind: &str,
        parent_id: &str,
    ) -> Result<Option<lash_core_execution::ParentEndPlan>, PluginError> {
        parent_end::get_by_key(&self.pool, parent_kind, parent_id, self.fence.fleet()).await
    }

    async fn list_parent_end_children(
        &self,
        parent: &lash_core_execution::ScopeId,
        after: Option<&ProcessId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        parent_end::children(&self.pool, parent, after, limit).await
    }

    async fn settle_parent_end_plan(
        &self,
        parent: &lash_core_execution::ScopeId,
    ) -> Result<(), PluginError> {
        parent_end::settle(&self.pool, &self.fence, parent, self.clock.timestamp_ms()).await
    }

    async fn list_unrecorded_opener_parents(
        &self,
        after: Option<&str>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<lash_core_execution::ScopeId>, PluginError> {
        parent_end::list_unrecorded_opener_parents(&self.pool, after, limit).await
    }

    async fn record_first_started_with_authority(
        &self,
        process_id: &ProcessId,
        started: ProcessStarted,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessStartOutcome, PluginError> {
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let now = self.clock.timestamp_ms();
        validate_process_execution_authority(process_id, &record, authority, Some(&started))?;
        match lash_core_execution::runtime::prepare_process_start(&record, &started)? {
            ProcessStartPlan::AlreadyApplied => {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(ProcessStartOutcome::AlreadyApplied(record));
            }
            ProcessStartPlan::Append => {}
        }
        let request = ProcessEventAppendRequest::first_started(process_id, &started);
        append_process_event_tx(&mut tx, &mut record, request, now, self.fence.fleet()).await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(ProcessStartOutcome::Started(record))
    }

    async fn request_process_cancel(
        &self,
        process_id: &ProcessId,
        origin: lash_core_execution::CancelOrigin,
        requester: String,
        attribution: Option<lash_core_execution::RuntimeReplayAttribution>,
    ) -> Result<ProcessRecord, PluginError> {
        self.request_process_cancel_reporting_realization(
            process_id,
            origin,
            requester,
            attribution,
        )
        .await
        .map(|(record, _)| record)
    }

    async fn request_process_cancel_reporting_realization(
        &self,
        process_id: &ProcessId,
        origin: lash_core_execution::CancelOrigin,
        requester: String,
        attribution: Option<lash_core_execution::RuntimeReplayAttribution>,
    ) -> Result<(ProcessRecord, lash_core_execution::StoreRealization), PluginError> {
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let now = self.clock.timestamp_ms();
        let request = lash_core_execution::CancelRequest::new(origin, requester, now);
        match lash_core_execution::runtime::prepare_process_transition(
            &record,
            ProcessTransition::RequestCancel(request),
        )? {
            ProcessTransitionPlan::Unchanged => {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok((record, lash_core_execution::StoreRealization::Coalesced));
            }
            ProcessTransitionPlan::Append(mut append) => {
                if let Some(replay) = append.replay.as_mut() {
                    replay.attribution = attribution;
                }
                append_process_event_tx(&mut tx, &mut record, *append, now, self.fence.fleet())
                    .await?;
            }
        }
        // The recorded cancel reaches the process's actor in the same
        // transaction: its cancel mail and a control wake, which readies even
        // a parked actor (ADR 0132 §11).
        if !record.is_terminal()
            && let Some(request) = record.cancel_request.as_deref()
        {
            crate::durable::processes::cancel_mail_within(
                &mut tx,
                process_id,
                request.origin,
                &request.requester,
                true,
                lash_durable::DurableInstant(i64::try_from(now).unwrap_or(i64::MAX)),
            )
            .await
            .map_err(|error| PluginError::Session(error.to_string()))?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok((record, lash_core_execution::StoreRealization::Realized))
    }

    async fn set_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        wait: lash_core_execution::WaitState,
        prelude: Vec<ProcessEventAppendRequest>,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        validate_process_execution_authority(process_id, &record, authority, None)?;
        let occurred_at_ms = self.clock.timestamp_ms();
        // The run's pending prelude commits ahead of the transition, in its
        // transaction (FIG-3571).
        let mut batch = ProcessEventBatch::for_fleet(self.fence.fleet());
        for request in prelude {
            batch
                .stage(&mut tx, &mut record, request, occurred_at_ms)
                .await?;
        }
        if let ProcessTransitionPlan::Append(request) =
            lash_core_execution::runtime::prepare_process_transition(
                &record,
                ProcessTransition::EnterWait(wait),
            )?
        {
            batch
                .stage(&mut tx, &mut record, *request, occurred_at_ms)
                .await?;
        }
        batch.commit(&mut tx, &record).await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(record)
    }

    async fn clear_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        prelude: Vec<ProcessEventAppendRequest>,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let now = self.clock.timestamp_ms();
        validate_process_execution_authority(process_id, &record, authority, None)?;
        // The run's pending prelude commits ahead of the transition, in its
        // transaction (FIG-3571).
        let mut batch = ProcessEventBatch::for_fleet(self.fence.fleet());
        for request in prelude {
            batch.stage(&mut tx, &mut record, request, now).await?;
        }
        if let ProcessTransitionPlan::Append(request) =
            lash_core_execution::runtime::prepare_process_transition(
                &record,
                ProcessTransition::ClearWait,
            )?
        {
            batch.stage(&mut tx, &mut record, *request, now).await?;
        }
        batch.commit(&mut tx, &record).await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(record)
    }
}
