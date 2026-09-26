use crate::*;
use lash_core_execution::ProcessQuery as _;
use lash_core_execution::facade_support::{
    self, registry_transitions::ProcessLeaseReclaimDecision,
};
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
#[path = "process_registry/continuation_store.rs"]
mod continuation_store;
#[path = "process_registry/leases.rs"]
mod leases;
#[path = "process_registry/lifecycle.rs"]
mod lifecycle;
#[cfg(test)]
#[path = "process_registry/list_plan_tests.rs"]
mod list_plan_tests;
#[path = "process_registry/parent_end.rs"]
pub(crate) mod parent_end;
#[path = "process_registry/park_feed.rs"]
pub(crate) mod park_feed;
mod prune;
#[path = "process_registry/prune_api.rs"]
pub(crate) mod prune_api;
mod retention;
#[path = "process_registry/tool_intent_submission.rs"]
mod tool_intent_submission;
use crate::process_sql::{list_processes_sql, process_sql};

pub(crate) mod wake_delivery;
#[path = "process_registry/worklist.rs"]
pub(crate) mod worklist;
use prune::prune_process_rows_tx;
use retention::{filter_tombstoned_process_ids, filter_unregistered_process_ids};
use wake_delivery::{
    claim_pending_wake_deliveries, decode_wake_delivery_row, load_wake_delivery_tx,
    update_wake_delivery_state, wake_delivery_report,
};
impl lash_core_execution::FleetFormatStore for PostgresProcessRegistry {
    fn fleet_format(&self) -> lash_core_execution::FleetFormat {
        self.fleet_format
    }
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessQuery for PostgresProcessRegistry {
    async fn get_process_by_start_key(
        &self,
        start_key: &lash_core_execution::StartKey,
    ) -> Result<Option<ProcessRecord>, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let record = load_process_by_start_key_tx(&mut tx, start_key).await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(record)
    }

    async fn get_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<ProcessRecord>, PluginError> {
        if let Some(record) = load_process(&self.pool, process_id).await? {
            return Ok(Some(record));
        }
        let row = sqlx::query(process_sql().tombstone.select_terminal.sql())
            .bind(process_id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        if let Some(row) = row {
            return Err(registry_transitions::process_no_longer_retained(
                registry_transitions::ProcessTombstoneStamp {
                    terminal_label: row.get(0),
                    pruned_at_ms: plugin_u64_from_sql(
                        "ProcessTombstone",
                        "pruned_at_ms",
                        row.get(1),
                    )?,
                },
            ));
        }
        Ok(None)
    }

    async fn list_processes(
        &self,
        filter: &lash_core_execution::ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        if filter
            .created_at_start_ms
            .is_some_and(|value| value > i64::MAX as u64)
        {
            return Ok(Vec::new());
        }
        let definition = filter
            .definition
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(process_decode_error)?;
        let mut query = sqlx::query(list_processes_sql(filter))
            .bind(filter.status.labels())
            .bind(filter.originator.as_ref().map(|o| o.originator_id()))
            .bind(filter.identity_kind.as_deref())
            .bind(filter.identity_label.as_deref())
            .bind(definition)
            .bind(filter.caused_by_occurrence_id.as_deref())
            .bind(filter.caused_by_subscription_id.as_deref())
            .bind(filter.created_at_start_ms.map(clamp_epoch_ms))
            .bind(filter.created_at_end_ms.map(clamp_epoch_ms))
            .bind(filter.retired_since_ms.map(clamp_epoch_ms));
        if let Some(parent) = &filter.parent_scope {
            query = query.bind(parent.storage_kind()).bind(parent.storage_id());
        }
        if let Some(before_ms) = filter.cancel_pending_before_ms {
            query = query.bind(clamp_epoch_ms(before_ms));
        }
        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        let mut records: Vec<ProcessRecord> = Vec::new();
        for row in rows {
            if let Some(record) = decode_matching_process(row, filter)? {
                records.push(record);
            }
        }
        Ok(records)
    }

    async fn processes_changed_since(
        &self,
        cursor: ProcessChangeCursor,
        limit: usize,
    ) -> Result<(Vec<ProcessChange>, ProcessChangeCursor), PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let horizon = process_change_horizon_tx(&mut tx).await?;
        if cursor.store_sequence() < horizon {
            return Err(PluginError::ProcessChangeCursorPruned {
                requested_cursor: cursor,
                tombstone_compaction_horizon: ProcessChangeCursor::from_store_sequence(horizon),
            });
        }
        if limit == 0 {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok((Vec::new(), cursor));
        }
        let rows = sqlx::query(process_sql().process_postgres.list_changes_after.sql())
            .bind(cursor.store_sequence() as i64)
            .bind(limit as i64)
            .fetch_all(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        let mut records = Vec::new();
        let mut next_cursor = cursor;
        for row in rows {
            let change_seq: i64 = row.get(0);
            let kind: String = row.get(1);
            let json: String = row.get(2);
            records.push(if kind == "upsert" {
                ProcessChange::Upsert {
                    record: serde_json::from_str(&json).map_err(process_decode_error)?,
                }
            } else {
                ProcessChange::Deleted {
                    tombstone: serde_json::from_str(&json).map_err(process_decode_error)?,
                }
            });
            next_cursor = ProcessChangeCursor::from_store_sequence(plugin_u64_from_sql(
                "ProcessChange",
                "change_seq",
                change_seq,
            )?);
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok((records, next_cursor))
    }

    async fn list_non_terminal_page(
        &self,
        limit: std::num::NonZeroUsize,
        continuation: Option<lash_core_execution::ProcessWorklistCursor>,
    ) -> Result<lash_core_execution::ProcessWorklistPage, PluginError> {
        worklist::list_non_terminal_page(self, limit, continuation).await
    }

    async fn filter_unregistered_process_ids(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<ProcessId>, PluginError> {
        filter_unregistered_process_ids(&self.pool, process_ids).await
    }

    async fn filter_tombstoned_process_ids(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<ProcessId>, PluginError> {
        filter_tombstoned_process_ids(&self.pool, process_ids).await
    }

    async fn live_reference_summary(&self) -> Result<Vec<ProcessLiveReferenceView>, PluginError> {
        let records = worklist::collect_non_terminal_records(self).await?;
        Ok(ProcessLiveReferenceView::from_records(records.iter()))
    }

    async fn count_non_terminal_processes(&self) -> Result<usize, PluginError> {
        worklist::count_non_terminal_processes(self).await
    }

    async fn list_parked_processes(
        &self,
        query: &lash_core_execution::store::ProcessParkQuery,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        park_feed::list_parked_processes(self, query).await
    }

    async fn process_park_feed(
        &self,
        after: lash_core_execution::store::ParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<
        lash_core_execution::store::ParkFeedPage<lash_core_execution::store::ProcessParkKey>,
        PluginError,
    > {
        park_feed::process_park_feed(self, after, limit).await
    }

    async fn summarize_parked_processes(
        &self,
    ) -> Result<lash_core_execution::store::ParkSummary, PluginError> {
        park_feed::summarize_parked_processes(self).await
    }
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessRegistrar for PostgresProcessRegistry {
    async fn register_process_reporting_disposition(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<lash_core_execution::ProcessRegistrationOutcome, PluginError> {
        let mut observers = observers.to_vec();
        observers.sort();
        observers.dedup();
        let wake_session_id = registration.wake_session_id.clone();
        let start_key = registration.start_key.clone();
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        // While the process minted for a key is retained, a start under the
        // same key returns that process untouched (ADR 0107); a host's key
        // must also present its content.
        if let Some(start_key) = start_key.as_ref()
            && let Some(existing) = load_process_by_start_key_tx(&mut tx, start_key).await?
        {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            lash_core_execution::runtime::check_retained_start(&registration, &existing)?;
            return Ok(lash_core_execution::ProcessRegistrationOutcome::existing(
                existing,
            ));
        }
        let unprepared = registration.clone();
        let registration =
            lash_core_execution::runtime::prepare_process_registration(registration)?;
        // Late-registration fencing: a `Cancel` child whose parent scope
        // already has a ledger row can never be swept, so it is refused here
        // rather than left to outlive its parent.
        //
        // The read and this transaction's insert are one decision, so it is
        // taken under the parent scope's advisory lock. Without it the pair is
        // a check-then-act against a ledger write that runs in its own
        // transaction on another connection: the child would read "no row",
        // the row would commit, the sweep would page children without seeing
        // this uncommitted one, and the child would land live under an ended
        // scope. Holding the lock orders the two writes either way round.
        if registration.lifecycle.on_parent_end == lash_core_execution::OnParentEnd::Cancel
            && !matches!(
                registration.lifecycle.parent,
                lash_core_execution::ParentScope::Host
            )
        {
            parent_end::lock_parent_scope_tx(&mut tx, &registration.lifecycle.parent).await?;
        }
        if registration.lifecycle.on_parent_end == lash_core_execution::OnParentEnd::Cancel
            && !matches!(
                registration.lifecycle.parent,
                lash_core_execution::ParentScope::Host
            )
            && parent_end::plan_exists_tx(&mut tx, &registration.lifecycle.parent).await?
        {
            return Err(lash_core_execution::PluginError::ParentEnded {
                start_key: registration.start_key.clone(),
                parent: registration.lifecycle.parent.clone(),
            });
        }
        // Minted only once the start is admitted, so no refusal names an id
        // that was never registered.
        let process_id = self.process_id_mint.mint();
        let now = self.clock.timestamp_ms();
        let change_seq = next_process_change_seq_tx(&mut tx).await?;
        let mut record = ProcessRecord::from_prepared_registration(registration, process_id, now);
        let record_json = serde_json::to_string(&record).map_err(process_decode_error)?;
        let result = sqlx::query(process_sql().process_postgres.insert_registration.sql())
            .bind(record.id.as_str())
            .bind(
                record
                    .start_key
                    .as_ref()
                    .map(lash_core_execution::StartKey::as_str),
            )
            .bind(record.originator_id().as_str())
            .bind(wake_session_id.as_deref())
            .bind(record.identity.kind.as_str())
            .bind(&record.identity.label)
            .bind(record.created_at_ms as i64)
            .bind(record.updated_at_ms as i64)
            .bind(record.last_event_sequence as i64)
            .bind(change_seq as i64)
            .bind(process_status_label(&record))
            .bind(record.lifecycle.parent.storage_kind())
            .bind(record.lifecycle.parent.storage_id())
            .bind(record.lifecycle.on_parent_end.storage_label())
            .bind(cancel_requested_at_ms(&record))
            .bind(record_json)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        // On this tier alone the read that found no retained process for the
        // key and the insert that acts on it are two statements in one
        // `READ COMMITTED` transaction, so each takes its own snapshot: two
        // callers presenting one key can both read "no row". The change clock
        // above orders the pair: the first holds that row lock from its bump
        // until it commits, so by the time the second reaches this insert the
        // winner's row is committed and `ON CONFLICT DO NOTHING` reports zero
        // rows instead of raising the start-key unique index. Re-read the
        // winner under this statement's own snapshot and abandon the attempt:
        // the rollback takes the clock bump and the observer rows with it, so
        // the loser adds no event and no `change_seq` of its own (ADR 0046),
        // and the caller gets the sequential answer — the winner's process.
        if result.rows_affected() == 0 {
            let winner = match start_key.as_ref() {
                Some(start_key) => load_process_by_start_key_tx(&mut tx, start_key).await?,
                None => None,
            };
            tx.rollback().await.map_err(plugin_sqlx_error)?;
            let Some(winner) = winner else {
                return Err(PluginError::Session(format!(
                    "process `{}` lost the registration insert race to a row that no longer exists",
                    record.id
                )));
            };
            lash_core_execution::runtime::check_retained_start(&unprepared, &winner)?;
            return Ok(lash_core_execution::ProcessRegistrationOutcome::existing(
                winner,
            ));
        }
        let process_id = record.id.clone();
        for session_id in observers {
            sqlx::query(process_sql().observer.insert.sql())
                .bind(session_id.as_str())
                .bind(process_id.as_str())
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?;
            append_process_event_tx(
                &mut tx,
                &mut record,
                ProcessEventAppendRequest::observer_added(
                    &process_id,
                    &session_id,
                    &ProcessObserverBy::host("registration"),
                ),
                now,
                self.wake_delivery_config,
                self.fleet_format,
            )
            .await?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lash_core_execution::ProcessRegistrationOutcome::created(
            record,
        ))
    }

    fn bind_effect_host(&self, effect_host: &Arc<dyn lash_core_execution::EffectHost>) {
        // The host keeps its own scope fence; the binding carries only the
        // registration truth it lifts that fence against.
        self.scope_fence_hosts.bind(
            effect_host,
            lash_core_execution::ProcessRegistryBinding {
                registrations: Arc::new(PostgresRegistrationProbe {
                    pool: self.pool.clone(),
                }),
            },
        );
    }

    async fn set_external_ref(
        &self,
        process_id: &ProcessId,
        external_ref: ProcessExternalRef,
    ) -> Result<ProcessRecord, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        match lash_core_execution::runtime::prepare_process_transition(
            &record,
            ProcessTransition::SetExternalRef(external_ref),
        )? {
            ProcessTransitionPlan::Unchanged => {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(record);
            }
            ProcessTransitionPlan::Append(request) => {
                append_process_event_tx(
                    &mut tx,
                    &mut record,
                    *request,
                    self.clock.timestamp_ms(),
                    self.wake_delivery_config,
                    self.fleet_format,
                )
                .await?;
            }
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(record)
    }
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessObserverRegistry for PostgresProcessRegistry {
    async fn add_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let changed = sqlx::query(process_sql().observer_postgres.insert_if_absent.sql())
            .bind(session_id.as_str())
            .bind(process_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected();
        if changed > 0 {
            append_process_event_tx(
                &mut tx,
                &mut record,
                ProcessEventAppendRequest::observer_added(process_id, session_id, &by),
                self.clock.timestamp_ms(),
                self.wake_delivery_config,
                self.fleet_format,
            )
            .await?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(())
    }

    async fn remove_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let changed = sqlx::query(process_sql().observer.delete.sql())
            .bind(session_id.as_str())
            .bind(process_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected();
        if changed > 0 {
            append_process_event_tx(
                &mut tx,
                &mut record,
                ProcessEventAppendRequest::observer_removed(process_id, session_id, &by),
                self.clock.timestamp_ms(),
                self.wake_delivery_config,
                self.fleet_format,
            )
            .await?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(())
    }

    async fn transfer_observers(
        &self,
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: &[ProcessId],
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        for process_id in process_ids {
            let mut record = require_process_tx(&mut tx, process_id).await?;
            let removed = sqlx::query(process_sql().observer.delete.sql())
                .bind(from_session_id.as_str())
                .bind(process_id.as_str())
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?
                .rows_affected();
            if removed == 0 {
                return Err(PluginError::Session(format!(
                    "process `{process_id}` is not observed by `{from_session_id}`"
                )));
            }
            sqlx::query(process_sql().observer_postgres.insert_if_absent.sql())
                .bind(to_session_id.as_str())
                .bind(process_id.as_str())
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?;
            append_process_event_tx(
                &mut tx,
                &mut record,
                ProcessEventAppendRequest::observer_removed(process_id, from_session_id, &by),
                self.clock.timestamp_ms(),
                self.wake_delivery_config,
                self.fleet_format,
            )
            .await?;
            append_process_event_tx(
                &mut tx,
                &mut record,
                ProcessEventAppendRequest::observer_added(process_id, to_session_id, &by),
                self.clock.timestamp_ms(),
                self.wake_delivery_config,
                self.fleet_format,
            )
            .await?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)
    }

    async fn list_observed_by(
        &self,
        session_id: &SessionId,
        filter: &lash_core_execution::ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        let rows = sqlx::query(process_sql().registry_postgres.list_observed.sql())
            .bind(session_id.as_str())
            .bind(filter.status.labels())
            .bind(filter.retired_since_ms.map(clamp_epoch_ms))
            .fetch_all(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        rows.into_iter()
            .map(|row| {
                serde_json::from_str::<ProcessRecord>(&row.get::<String, _>(0))
                    .map_err(process_decode_error)
            })
            .filter(|result| {
                result
                    .as_ref()
                    .map_or(true, |record| filter.matches_record(record))
            })
            .collect()
    }

    async fn is_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
    ) -> Result<bool, PluginError> {
        let (retained, observer): (bool, bool) = sqlx::query_as(
            process_sql()
                .registry_postgres
                .observation_and_registration_exist
                .sql(),
        )
        .bind(session_id.as_str())
        .bind(process_id.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        if retained {
            return Ok(observer);
        }
        self.get_process(process_id).await?;
        Ok(false)
    }

    async fn observers_for_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<Vec<SessionId>, PluginError> {
        if self.get_process(process_id).await?.is_none() {
            return Err(registry_transitions::unknown_process(process_id));
        }
        sqlx::query_scalar(process_sql().observer.list_sessions_for_process.sql())
            .bind(process_id.as_str())
            .fetch_all(&self.pool)
            .await
            .map(|ids: Vec<String>| ids.into_iter().map(SessionId::from).collect())
            .map_err(plugin_sqlx_error)
    }

    async fn retarget_subscription(
        &self,
        process_id: &ProcessId,
        target: Option<&str>,
    ) -> Result<(), PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let previous: Option<String> =
            sqlx::query_scalar(process_sql().process.select_wake_session_id.sql())
                .bind(process_id.as_str())
                .fetch_one(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?;
        if previous.as_deref() == target {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(());
        }
        append_process_event_tx(
            &mut tx,
            &mut record,
            ProcessEventAppendRequest::subscription_retargeted(process_id, target),
            self.clock.timestamp_ms(),
            self.wake_delivery_config,
            self.fleet_format,
        )
        .await?;
        sqlx::query(process_sql().process.set_wake_session_id.sql())
            .bind(process_id.as_str())
            .bind(target)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        if let Some(previous) = previous {
            sqlx::query(process_sql().wake.discard_retargeted.sql())
                .bind(process_id.as_str())
                .bind(previous)
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)
    }

    async fn delete_session_process_state(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core_execution::ProcessSessionDeleteReport, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let discarded_wake_delivery_count =
            sqlx::query(process_sql().wake.discard_target_gone.sql())
                .bind(session_id.as_str())
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?
                .rows_affected() as usize;
        let removed_observer_count = sqlx::query(process_sql().observer.delete_by_session.sql())
            .bind(session_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected() as usize;
        let cleared_subscription_count =
            sqlx::query(process_sql().process.clear_wake_session_for_session.sql())
                .bind(session_id.as_str())
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?
                .rows_affected() as usize;
        sqlx::query(process_sql().floor.delete_by_session.sql())
            .bind(session_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lash_core_execution::ProcessSessionDeleteReport {
            session_id: session_id.clone(),
            removed_observer_count,
            discarded_wake_delivery_count,
            cleared_subscription_count,
        })
    }
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessEventLog for PostgresProcessRegistry {
    async fn append_event(
        &self,
        process_id: &ProcessId,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendReceipt, PluginError> {
        facade_support::validate_generic_process_event_append(&request)?;
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let occurred_at_ms = self.clock.timestamp_ms();
        let result = append_process_event_tx(
            &mut tx,
            &mut record,
            request,
            occurred_at_ms,
            self.wake_delivery_config,
            self.fleet_format,
        )
        .await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(result)
    }

    async fn append_events(
        &self,
        process_id: &ProcessId,
        requests: Vec<ProcessEventAppendRequest>,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<Vec<ProcessEventAppendReceipt>, PluginError> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let now_ms = process_lease_now_epoch_ms_tx(&mut tx).await?;
        validate_process_execution_authority_tx(
            &mut tx,
            process_id,
            &record,
            authority,
            None,
            now_ms,
            self.fleet_format,
        )
        .await?;
        // `occurred_at_ms` provenance is inconsistent in this backend: four
        // mutating paths stamp it from the server clock while the others (like
        // this one) use the injected clock. Decision-inert today — no fence or
        // retention predicate reads it — tracked as FIG-971.
        let occurred_at_ms = self.clock.timestamp_ms();
        let receipts = append_process_event_batch_tx(
            &mut tx,
            &mut record,
            requests,
            occurred_at_ms,
            self.wake_delivery_config,
            self.fleet_format,
        )
        .await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(receipts)
    }

    async fn event_page_after(
        &self,
        process_id: &ProcessId,
        after_sequence: u64,
        limit: std::num::NonZeroUsize,
        mode: lash_core_execution::ProcessEventQueryMode,
    ) -> Result<
        lash_core_execution::ProcessEventReadOutcome<lash_core_execution::ProcessEventPage>,
        PluginError,
    > {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        match require_process_tx(&mut tx, process_id).await {
            Ok(_) => {}
            Err(PluginError::ProcessNoLongerRetained {
                terminal_label,
                pruned_at_ms,
            }) => {
                tx.rollback().await.map_err(plugin_sqlx_error)?;
                return Ok(
                    lash_core_execution::ProcessEventReadOutcome::NoLongerRetained(
                        lash_core_execution::ProcessEventHistoryRetention::Pruned {
                            terminal_label,
                            pruned_at_ms,
                        },
                    ),
                );
            }
            Err(error) => {
                tx.rollback().await.map_err(plugin_sqlx_error)?;
                return Err(error);
            }
        }
        let after_sequence = i64::try_from(after_sequence).map_err(|_| {
            PluginError::Session("process event page sequence exceeds the SQL cursor range".into())
        })?;
        let fetch_limit = limit
            .get()
            .checked_add(1)
            .and_then(|value| i64::try_from(value).ok())
            .ok_or_else(|| PluginError::Session("process event page limit is too large".into()))?;
        let page = match mode {
            lash_core_execution::ProcessEventQueryMode::Full => {
                let rows = sqlx::query(process_sql().event.page_full.sql())
                    .bind(process_id.as_str())
                    .bind(after_sequence)
                    .bind(fetch_limit)
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(plugin_sqlx_error)?;
                let events = rows
                    .into_iter()
                    .map(|row| {
                        serde_json::from_str(&row.get::<String, _>(0)).map_err(process_decode_error)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                lash_core_execution::ProcessEventPage::from_full_rows(events, limit)
            }
            lash_core_execution::ProcessEventQueryMode::Lite => {
                let rows = sqlx::query(process_sql().event.page_lite.sql())
                    .bind(process_id.as_str())
                    .bind(after_sequence)
                    .bind(fetch_limit)
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(plugin_sqlx_error)?;
                let events = rows
                    .into_iter()
                    .map(|row| {
                        Ok(lash_core_execution::ProcessEventLite {
                            sequence: plugin_u64_from_sql(
                                "ProcessEventLite",
                                "sequence",
                                row.get(0),
                            )?,
                            event_type: row.get(1),
                        })
                    })
                    .collect::<Result<Vec<_>, PluginError>>()?;
                lash_core_execution::ProcessEventPage::from_lite_rows(events, limit)
            }
        };
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lash_core_execution::ProcessEventReadOutcome::Retained(page))
    }

    async fn count_events_through(
        &self,
        process_id: &ProcessId,
        event_type: &str,
        up_to_sequence: u64,
    ) -> Result<u64, PluginError> {
        if self.get_process(process_id).await?.is_none() {
            return Err(registry_transitions::unknown_process(process_id));
        }
        let row = sqlx::query(process_sql().event.count_by_type_through_sequence.sql())
            .bind(process_id.as_str())
            .bind(event_type)
            .bind(clamp_sequence_bound(up_to_sequence))
            .fetch_one(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        let count: i64 = row.get(0);
        Ok(count as u64)
    }

    async fn recent_events(
        &self,
        process_id: &ProcessId,
        limit: usize,
    ) -> Result<Vec<ProcessEvent>, PluginError> {
        if self.get_process(process_id).await?.is_none() {
            return Err(registry_transitions::unknown_process(process_id));
        }
        let rows = sqlx::query(process_sql().event.list_recent.sql())
            .bind(process_id.as_str())
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        let mut events = Vec::new();
        for row in rows {
            let json: String = row.get(0);
            events.push(serde_json::from_str(&json).map_err(process_decode_error)?);
        }
        events.reverse();
        Ok(events)
    }
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessToolIntents for PostgresProcessRegistry {
    async fn admit_tool_intent_submission(
        &self,
        submission: lash_core_execution::ToolIntentSubmissionRecord,
    ) -> Result<lash_core_execution::ToolIntentSubmissionAdmission, PluginError> {
        tool_intent_submission::admit(&self.pool, submission).await
    }

    async fn complete_tool_intent_submission(
        &self,
        replay_key: &str,
        outcome: lash_core_execution::ToolIntentExecutionOutcome,
    ) -> Result<lash_core_execution::ToolIntentSubmissionRecord, PluginError> {
        tool_intent_submission::complete(&self.pool, replay_key, outcome).await
    }
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessWakeOutbox for PostgresProcessRegistry {
    fn wake_delivery_config(&self) -> lash_core_execution::WakeDeliveryConfig {
        self.wake_delivery_config
    }

    async fn claim_pending_wake_deliveries(
        &self,
        limit: usize,
    ) -> Result<Vec<lash_core_execution::WakeDelivery>, PluginError> {
        claim_pending_wake_deliveries(self, limit).await
    }

    async fn list_wake_deliveries(
        &self,
        state: Option<lash_core_execution::WakeDeliveryState>,
    ) -> Result<Vec<lash_core_execution::WakeDelivery>, PluginError> {
        let rows = if let Some(state) = state {
            sqlx::query(process_sql().wake_postgres.list_by_state.sql())
                .bind(state.as_str())
                .fetch_all(&self.pool)
                .await
                .map_err(plugin_sqlx_error)?
        } else {
            sqlx::query(process_sql().wake_postgres.list_all.sql())
                .fetch_all(&self.pool)
                .await
                .map_err(plugin_sqlx_error)?
        };
        rows.into_iter()
            .map(|row| decode_wake_delivery_row(row, self.fleet_format))
            .collect()
    }

    async fn wake_delivery_report(
        &self,
    ) -> Result<lash_core_execution::WakeDeliveryReport, PluginError> {
        let deliveries = self.list_wake_deliveries(None).await?;
        Ok(wake_delivery_report(deliveries.iter()))
    }

    async fn mark_wake_enqueued(
        &self,
        delivery_id: &str,
        claim_token: &str,
    ) -> Result<lash_core_execution::WakeDeliveryClaimOutcome, PluginError> {
        let disposition = lash_core_execution::WakeDeliveryDisposition::Enqueued;
        update_wake_delivery_state(&self.pool, delivery_id, claim_token, disposition).await
    }

    async fn discard_wake_delivery(
        &self,
        delivery_id: &str,
        claim_token: &str,
        reason: lash_core_execution::WakeDiscardReason,
    ) -> Result<lash_core_execution::WakeDeliveryClaimOutcome, PluginError> {
        let disposition = lash_core_execution::WakeDeliveryDisposition::Discarded { reason };
        update_wake_delivery_state(&self.pool, delivery_id, claim_token, disposition).await
    }

    async fn redrive_wake_delivery(&self, delivery_id: &str) -> Result<(), PluginError> {
        let expires_at_ms = self
            .clock
            .timestamp_ms()
            .saturating_add(self.wake_delivery_config.delivery_expiry_ms);
        let next_attempt_at_ms = self.clock.timestamp_ms();
        let changed = sqlx::query(process_sql().wake.redrive_discarded.sql())
            .bind(delivery_id)
            .bind(expires_at_ms as i64)
            .bind(next_attempt_at_ms as i64)
            .execute(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected();
        if changed == 0 {
            return Err(PluginError::Session(format!(
                "wake delivery `{delivery_id}` is not discarded or does not exist"
            )));
        }
        Ok(())
    }

    async fn defer_wake_delivery(
        &self,
        delivery_id: &str,
        claim_token: &str,
        next_attempt_at_ms: u64,
    ) -> Result<lash_core_execution::WakeDeliveryClaimOutcome, PluginError> {
        let changed = sqlx::query(process_sql().wake.release_claim.sql())
            .bind(delivery_id)
            .bind(claim_token)
            .bind(next_attempt_at_ms as i64)
            .execute(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected();
        if changed == 0 {
            let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
            let delivery = load_wake_delivery_tx(&mut tx, delivery_id, self.fleet_format).await?;
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(lash_core_execution::WakeDeliveryClaimOutcome::ClaimLost {
                state: delivery.state(),
            });
        }
        Ok(lash_core_execution::WakeDeliveryClaimOutcome::Applied)
    }
}
#[async_trait::async_trait]
impl lash_core_execution::ProcessRetention for PostgresProcessRegistry {
    async fn pending_process_artifact_cleanup(
        &self,
    ) -> Result<Vec<lash_core_execution::ProcessArtifactCleanup>, PluginError> {
        let rows: Vec<String> = sqlx::query_scalar(process_sql().cleanup.list_pending.sql())
            .fetch_all(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        rows.into_iter()
            .map(|json| serde_json::from_str(&json).map_err(process_decode_error))
            .collect()
    }

    async fn complete_process_artifact_cleanup(
        &self,
        process_id: &ProcessId,
    ) -> Result<lash_core_execution::ProcessArtifactCleanupAck, PluginError> {
        prune_api::complete_process_artifact_cleanup(self, process_id).await
    }

    async fn compact_process_park_feed(
        &self,
        through: lash_core_execution::store::ParkFeedCursor,
    ) -> Result<(), PluginError> {
        park_feed::compact_process_park_feed(self, through).await
    }

    async fn compact_process_tombstones(
        &self,
        cutoff_epoch_ms: u64,
        watermark: lash_core_execution::ProjectionWatermark,
        trigger_store: Option<&dyn lash_core_execution::TriggerStore>,
    ) -> Result<usize, PluginError> {
        let max_change_seq = match watermark {
            lash_core_execution::ProjectionWatermark::UpTo(cursor) => {
                Some(cursor.store_sequence() as i64)
            }
            lash_core_execution::ProjectionWatermark::NoProjector => None,
        };
        let outstanding_trigger_delivery_process_ids = match trigger_store {
            Some(trigger_store) => trigger_store.list_delivery_process_ids().await?,
            None => Vec::new(),
        };
        let cutoff_epoch_ms = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        sqlx::query(process_sql().clock_postgres.select_current_for_update.sql())
            .fetch_one(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        let compacted_through: Option<i64> = sqlx::query_scalar(
            process_sql()
                .tombstone_postgres
                .select_max_compactable_change_seq
                .sql(),
        )
        .bind(cutoff_epoch_ms)
        .bind(max_change_seq)
        .bind(
            outstanding_trigger_delivery_process_ids
                .iter()
                .map(ProcessId::as_str)
                .collect::<Vec<_>>(),
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        let deleted = sqlx::query(process_sql().tombstone_postgres.delete_compactable.sql())
            .bind(cutoff_epoch_ms)
            .bind(max_change_seq)
            .bind(
                outstanding_trigger_delivery_process_ids
                    .iter()
                    .map(ProcessId::as_str)
                    .collect::<Vec<_>>(),
            )
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected() as usize;
        if let Some(compacted_through) = compacted_through {
            sqlx::query(process_sql().clock_postgres.raise_compaction_horizon.sql())
                .bind(compacted_through)
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(deleted)
    }

    async fn prune_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<lash_core_execution::ProcessListFilter>,
        watermark: lash_core_execution::ProjectionWatermark,
    ) -> Result<ProcessPruneReport, PluginError> {
        prune_api::prune_terminal_processes(self, cutoff_epoch_ms, filter, watermark).await
    }

    async fn prunable_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<lash_core_execution::ProcessListFilter>,
        watermark: lash_core_execution::ProjectionWatermark,
    ) -> Result<Vec<ProcessId>, PluginError> {
        prune_api::prunable_terminal_processes(self, cutoff_epoch_ms, filter, watermark).await
    }
}
impl lash_core_execution::ProcessClockRebind for PostgresProcessRegistry {
    fn with_runtime_clock(
        &self,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> Option<Arc<dyn ProcessRegistry>> {
        Some(Arc::new(self.clone().with_clock(clock)))
    }
}
#[cfg(any(test, feature = "testing"))]
#[async_trait::async_trait]
impl lash_core_execution::ProcessRegistryTestSupport for PostgresProcessRegistry {
    async fn wake_allocation_floor_for_testing(
        &self,
        target_session_id: &SessionId,
        process_id: &ProcessId,
    ) -> Result<Option<u64>, PluginError> {
        sqlx::query_scalar::<_, i64>(process_sql().floor.select_floor.sql())
            .bind(target_session_id.as_str())
            .bind(process_id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?
            .map(|value| plugin_u64_from_sql("WakeAllocationFloor", "allocation_floor", value))
            .transpose()
    }
}
/// This registry's registration truth for a bound effect host (ADR 0049).
struct PostgresRegistrationProbe {
    pool: sqlx::PgPool,
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessRegistrationProbe for PostgresRegistrationProbe {
    async fn process_is_registered(&self, process_id: &ProcessId) -> Result<bool, PluginError> {
        sqlx::query_scalar::<_, bool>(process_sql().process.exists_by_id.sql())
            .bind(process_id.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(plugin_sqlx_error)
    }
}
