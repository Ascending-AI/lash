//! Process-event `occurred_at_ms` stamps use the injected registry clock on
//! every write. Lifecycle transitions and event batches sample it once for
//! their events. Database-clock admission and delivery decisions
//! retain their separate clock contract.

use crate::*;
use lash_core_execution::ProcessQuery as _;
use lash_core_execution::facade_support;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
#[path = "process_registry/event_release.rs"]
mod event_release;
#[path = "process_registry/lifecycle.rs"]
mod lifecycle;
#[cfg(test)]
#[path = "process_registry/list_plan_tests.rs"]
mod list_plan_tests;
#[path = "process_registry/parent_end.rs"]
pub(crate) mod parent_end;
mod prune;
#[path = "process_registry/prune_api.rs"]
pub(crate) mod prune_api;
mod retention;
#[path = "process_registry/tool_intent_submission.rs"]
mod tool_intent_submission;
use crate::process_sql::{list_processes_sql, process_sql};

#[path = "process_registry/pages.rs"]
pub(crate) mod pages;
#[path = "process_registry/registration.rs"]
pub(crate) mod registration;
use prune::prune_process_rows_tx;
use registration::{AppliedRegistration, apply_registration_tx};
use retention::{filter_tombstoned_process_ids, filter_unregistered_process_ids};
impl lash_core_execution::FleetFormatStore for PostgresProcessRegistry {
    fn fleet_format(&self) -> lash_core_execution::FleetFormat {
        self.fence.fleet()
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
                registry_transitions::ProcessTombstoneStamp::from_row(
                    process_id,
                    row.get(0),
                    plugin_u64_from_sql("ProcessTombstone", "pruned_at_ms", row.get(1))?,
                )?,
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
            .definition_id
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
            .bind(filter.created_at_start_ms.map(clamp_epoch_ms))
            .bind(filter.created_at_end_ms.map(clamp_epoch_ms))
            .bind(filter.retired_since_ms.map(clamp_epoch_ms));
        if let Some(scope) = &filter.until {
            query = query.bind(scope.storage_kind()).bind(scope.storage_id());
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
        crate::change_feed::sequence_before_read(
            &self.pool,
            &self.fence,
            crate::change_feed::Feed::Processes,
        )
        .await
        .map_err(plugin_store_error)?;
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

    async fn list_non_terminal_processes_page(
        &self,
        limit: std::num::NonZeroUsize,
        continuation: Option<lash_core_execution::ProcessRegistryCursor>,
    ) -> Result<lash_core_execution::NonTerminalProcessPage, PluginError> {
        pages::list_non_terminal_processes_page(self, limit, continuation).await
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
        let records = pages::collect_non_terminal_records(self).await?;
        Ok(ProcessLiveReferenceView::from_records(records.iter()))
    }

    async fn count_non_terminal_processes(&self) -> Result<usize, PluginError> {
        pages::count_non_terminal_processes(self).await
    }
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessRegistrar for PostgresProcessRegistry {
    async fn prepare_process_registration(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<lash_core_execution::PreparedProcessRegistration, PluginError> {
        let registration =
            lash_core_execution::runtime::prepare_process_registration(registration)?;
        let existing = match registration.start_key.as_ref() {
            Some(key) => {
                lash_core_execution::ProcessQuery::get_process_by_start_key(self, key).await?
            }
            None => None,
        };
        let retained = existing.is_some();
        let process_id = existing.map_or_else(|| self.process_id_mint.mint(), |record| record.id);
        Ok(lash_core_execution::PreparedProcessRegistration::new(
            registration,
            observers.to_vec(),
            process_id,
            retained,
            self.clock.timestamp_ms(),
        ))
    }
    async fn commit_process_registration(
        &self,
        prepared: lash_core_execution::PreparedProcessRegistration,
        anchor: lash_core_execution::TraceAnchor,
    ) -> Result<lash_core_execution::ProcessRegistrationReceipt, PluginError> {
        let (registration, observers, process_id, retained, now) = prepared.into_commit(anchor);
        let unprepared = registration.clone();
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        match apply_registration_tx(
            &mut tx,
            registration,
            observers,
            process_id,
            retained,
            now,
            self.fence.fleet(),
        )
        .await?
        {
            AppliedRegistration::Created(record) => {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                Ok(lash_core_execution::ProcessRegistrationReceipt::created(
                    record,
                ))
            }
            AppliedRegistration::Retained { record } => {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                lash_core_execution::runtime::check_retained_start(&unprepared, &record)?;
                Ok(lash_core_execution::ProcessRegistrationReceipt::existing(
                    record,
                ))
            }
            // The rollback takes the staged row and the observer rows with
            // it, so the loser adds no event and no `change_seq` of its own
            // (ADR 0046), and the caller gets the sequential answer — the
            // winner's process.
            AppliedRegistration::LostRace { winner } => {
                tx.rollback().await.map_err(plugin_sqlx_error)?;
                lash_core_execution::runtime::check_retained_start(&unprepared, &winner)?;
                Ok(lash_core_execution::ProcessRegistrationReceipt::existing(
                    winner,
                ))
            }
        }
    }

    async fn set_external_ref(
        &self,
        process_id: &ProcessId,
        external_ref: ProcessExternalRef,
    ) -> Result<ProcessRecord, PluginError> {
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
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
                    self.fence.fleet(),
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
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let changed = sqlx::query(process_sql().observer_postgres.insert_if_absent.sql())
            .bind(session_id.as_str())
            .bind(process_id.as_str())
            .execute(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected();
        if changed > 0 {
            append_process_event_tx(
                &mut tx,
                &mut record,
                ProcessEventAppendRequest::observer_added(process_id, session_id, &by),
                self.clock.timestamp_ms(),
                self.fence.fleet(),
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
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let changed = sqlx::query(process_sql().observer.delete.sql())
            .bind(session_id.as_str())
            .bind(process_id.as_str())
            .execute(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected();
        if changed > 0 {
            append_process_event_tx(
                &mut tx,
                &mut record,
                ProcessEventAppendRequest::observer_removed(process_id, session_id, &by),
                self.clock.timestamp_ms(),
                self.fence.fleet(),
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
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        for process_id in process_ids {
            let mut record = require_process_tx(&mut tx, process_id).await?;
            let removed = sqlx::query(process_sql().observer.delete.sql())
                .bind(from_session_id.as_str())
                .bind(process_id.as_str())
                .execute(&mut **tx)
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
                .execute(&mut **tx)
                .await
                .map_err(plugin_sqlx_error)?;
            append_process_event_tx(
                &mut tx,
                &mut record,
                ProcessEventAppendRequest::observer_removed(process_id, from_session_id, &by),
                self.clock.timestamp_ms(),
                self.fence.fleet(),
            )
            .await?;
            append_process_event_tx(
                &mut tx,
                &mut record,
                ProcessEventAppendRequest::observer_added(process_id, to_session_id, &by),
                self.clock.timestamp_ms(),
                self.fence.fleet(),
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
            .map_err(plugin_sqlx_error)?
            .into_iter()
            .map(|id: String| SessionId::parse(id).map_err(PluginError::from))
            .collect()
    }

    async fn delete_session_process_state(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core_execution::ProcessSessionDeleteReport, PluginError> {
        // The session's scope is not closed here: its `CloseSession` intent
        // is the one owner of that row (FIG-3607 R10, ADR 0108 §5).
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        let removed_observer_count = sqlx::query(process_sql().observer.delete_by_session.sql())
            .bind(session_id.as_str())
            .execute(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected() as usize;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lash_core_execution::ProcessSessionDeleteReport {
            session_id: session_id.clone(),
            removed_observer_count,
        })
    }
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessEventLog for PostgresProcessRegistry {
    async fn append_events(
        &self,
        process_id: &ProcessId,
        requests: Vec<ProcessEventAppendRequest>,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<Vec<ProcessEventAppendReceipt>, PluginError> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        validate_process_execution_authority(process_id, &record, authority, None)?;
        let occurred_at_ms = self.clock.timestamp_ms();
        let receipts = append_process_event_batch_tx(
            &mut tx,
            &mut record,
            requests,
            occurred_at_ms,
            self.fence.fleet(),
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
        let released_through = event_release::released_through_tx(&mut tx, process_id).await?;
        if after_sequence < released_through {
            tx.rollback().await.map_err(plugin_sqlx_error)?;
            return Ok(
                lash_core_execution::ProcessEventReadOutcome::NoLongerRetained(
                    lash_core_execution::ProcessEventHistoryRetention::Released {
                        released_through,
                    },
                ),
            );
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
                let fleet_format = self.fence.fleet();
                let events = rows
                    .into_iter()
                    .map(|row| ProcessEvent::decode(&row.get::<String, _>(0), fleet_format))
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
                            kind: lash_core_execution::ProcessEventKind::parse(
                                &row.get::<String, _>(1),
                            )
                            .ok_or_else(|| {
                                PluginError::Session("unknown process lifecycle event kind".into())
                            })?,
                        })
                    })
                    .collect::<Result<Vec<_>, PluginError>>()?;
                lash_core_execution::ProcessEventPage::from_lite_rows(events, limit)
            }
        };
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lash_core_execution::ProcessEventReadOutcome::Retained(page))
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
        let fleet_format = self.fence.fleet();
        let mut events = Vec::new();
        for row in rows {
            let json: String = row.get(0);
            events.push(ProcessEvent::decode(&json, fleet_format)?);
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
        tool_intent_submission::admit(
            &self.pool,
            &self.fence,
            submission,
            self.clock.timestamp_ms(),
        )
        .await
    }

    async fn complete_tool_intent_submission(
        &self,
        replay_key: &str,
        outcome: lash_core_execution::ToolIntentExecutionOutcome,
    ) -> Result<
        lash_core_execution::store::StoreTransition<
            lash_core_execution::ToolIntentSubmissionRecord,
        >,
        PluginError,
    > {
        tool_intent_submission::complete(
            &self.pool,
            &self.fence,
            replay_key,
            outcome,
            self.clock.timestamp_ms(),
        )
        .await
    }
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessRetention for PostgresProcessRegistry {
    async fn release_process_events(
        &self,
        process_id: &ProcessId,
        through: u64,
    ) -> Result<lash_core_execution::ProcessEventRelease, PluginError> {
        event_release::release_process_events(self, process_id, through).await
    }

    async fn compact_process_tombstones(
        &self,
        cutoff_epoch_ms: u64,
        watermark: lash_core_execution::ProjectionWatermark,
    ) -> Result<usize, PluginError> {
        let cutoff_epoch_ms = clamp_epoch_ms(cutoff_epoch_ms);
        let max_change_seq = match watermark {
            lash_core_execution::ProjectionWatermark::UpTo(cursor) => {
                Some(cursor.store_sequence() as i64)
            }
            lash_core_execution::ProjectionWatermark::NoProjector => None,
        };
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        // Tombstones are judged by their sequence: sequence the committed
        // ones first, under the clock lock that also orders this compaction
        // against every reader's horizon.
        crate::change_feed::sequence_processes(&mut tx)
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
        .fetch_one(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
        let deleted = sqlx::query(process_sql().tombstone_postgres.delete_compactable.sql())
            .bind(cutoff_epoch_ms)
            .bind(max_change_seq)
            .execute(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected() as usize;
        if let Some(compacted_through) = compacted_through {
            sqlx::query(process_sql().clock_postgres.raise_compaction_horizon.sql())
                .bind(compacted_through)
                .execute(&mut **tx)
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

    async fn release_consumer_hold(
        &self,
        process_id: &ProcessId,
        key: &str,
    ) -> Result<(), PluginError> {
        crate::guarded_tx::guarded(&self.pool, &self.fence, |tx| {
            Box::pin(async move {
                sqlx::query(process_sql().process.release_consumer_hold.sql())
                    .bind(process_id.as_str())
                    .bind(key)
                    .execute(tx.as_mut())
                    .await
                    .map(drop)
                    .map_err(store_sqlx_error)
            })
        })
        .await
        .map_err(plugin_store_error)
    }

    async fn abandon_consumer_hold(
        &self,
        key: &str,
        owner: &lash_core_execution::ScopeId,
    ) -> Result<Vec<ProcessId>, PluginError> {
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        parent_end::lock_consumer_hold_tx(&mut tx, key).await?;
        sqlx::query(process_sql().abandoned_hold.mark.sql())
            .bind(key)
            .bind(owner.storage_kind())
            .bind(owner.storage_id())
            .bind(self.clock.timestamp_ms() as i64)
            .execute(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?;
        let ids: Vec<String> = sqlx::query_scalar(process_sql().process.select_owed_cancels.sql())
            .bind(key)
            .fetch_all(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        ids.iter()
            .map(|id| {
                ProcessId::parse(id).map_err(|error| {
                    PluginError::Session(format!("a held process row names an invalid id: {error}"))
                })
            })
            .collect()
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
impl lash_core_execution::ProcessRegistryTestSupport for PostgresProcessRegistry {}
