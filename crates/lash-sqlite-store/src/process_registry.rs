use super::*;
use lash_core_execution::ProcessEventPageTokenStoreExt as _;
use lash_core_execution::ProcessQuery as _;
use lash_core_execution::facade_support;
use lash_sansio::ProcessId;
#[path = "process_registry/continuation_store.rs"]
mod continuation_store;
mod leases;
#[cfg(test)]
mod list_tests;
#[path = "process_registry/parent_end.rs"]
pub(crate) mod parent_end;
#[path = "process_registry/prune_api.rs"]
mod prune_api;
mod registration;
#[path = "process_registry/retention.rs"]
mod retention;
mod segment_handover;
#[path = "process_registry/sql.rs"]
pub(crate) mod sql;
mod support;
#[path = "process_registry/tool_intent_submission.rs"]
mod tool_intent_submission;
mod wake_delivery;
pub(crate) mod worklist;

use sql::process_sql;
use support::cancel_requested_at_ms;
use support::process_scope_fence_key;
use support::process_status_label;
pub(crate) use support::{ProcessEventAppendArm, ProcessEventWriteAuthorization, tx_outcome};
use wake_delivery::{load_wake_delivery_conn, update_wake_delivery_state, wake_delivery_report};

#[async_trait::async_trait]
impl lash_core_execution::ProcessQuery for SqliteProcessRegistry {
    async fn get_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<ProcessRecord>, lash_core_execution::PluginError> {
        let process_id = process_id.clone();
        self.conn
            .call(move |conn| {
                Ok((|| {
                    if let Some(record) = Self::load_process_conn(conn, &process_id)? {
                        return Ok(Some(record));
                    }
                    let tombstone: Option<(String, i64)> = conn
                        .query_row(
                            process_sql().tombstone.select_latest_terminal.sql(),
                            params![process_id.as_str()],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()
                        .map_err(process_sqlite_error)?;
                    if let Some((terminal_label, pruned_at_ms)) = tombstone {
                        return Err(registry_transitions::process_no_longer_retained(
                            registry_transitions::ProcessTombstoneStamp {
                                terminal_label,
                                pruned_at_ms: plugin_u64_from_sql(
                                    "ProcessTombstone",
                                    "pruned_at_ms",
                                    pruned_at_ms,
                                )?,
                            },
                        ));
                    }
                    Ok(None)
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn get_process_ref(
        &self,
        process_ref: &ProcessRef,
    ) -> Result<Option<ProcessRecord>, lash_core_execution::PluginError> {
        let process_ref = process_ref.clone();
        self.conn
            .call(move |conn| Ok(Self::require_process_ref_conn(conn, &process_ref).map(Some)))
            .await
            .map_err(process_sqlite_error)?
    }

    async fn list_processes(
        &self,
        filter: &lash_core_execution::ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, lash_core_execution::PluginError> {
        if filter
            .created_at_start_ms
            .is_some_and(|value| value > i64::MAX as u64)
        {
            return Ok(Vec::new());
        }
        let filter = filter.clone();
        let definition = filter
            .definition
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(process_decode_error)?;
        let status = filter
            .status
            .labels()
            .map(|labels| serde_json::to_string(&labels))
            .transpose()
            .map_err(process_decode_error)?;
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let (sql, values) = sql::list_processes_query(&filter, status, definition);
                    let mut stmt = conn.prepare(sql).map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                            row.get::<_, String>(0)
                        })
                        .map_err(process_sqlite_error)?;
                    let mut records = Vec::new();
                    for row in rows {
                        let record: ProcessRecord =
                            serde_json::from_str(&row.map_err(process_sqlite_error)?)
                                .map_err(process_decode_error)?;
                        // SQLite's JSON functions deliberately coerce some JSON
                        // representations. The typed/canonical SQL predicate is
                        // the pushdown; the Rust predicate is the exact
                        // `serde_json::Value` equality fence.
                        if filter.matches_record(&record) {
                            records.push(record);
                        }
                    }
                    Ok(records)
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn processes_changed_since(
        &self,
        cursor: ProcessChangeCursor,
        limit: usize,
    ) -> Result<(Vec<ProcessChange>, ProcessChangeCursor), lash_core_execution::PluginError> {
        self.conn
            .call(move |conn| {
                Ok(
                    crate::process_registry_change::processes_changed_since_conn(
                        conn, cursor, limit,
                    ),
                )
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn list_non_terminal_page(
        &self,
        limit: std::num::NonZeroUsize,
        continuation: Option<lash_core_execution::ProcessWorklistCursor>,
    ) -> Result<lash_core_execution::ProcessWorklistPage, lash_core_execution::PluginError> {
        worklist::list_non_terminal_page(self, limit, continuation).await
    }

    async fn filter_unregistered_process_ids(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<ProcessId>, lash_core_execution::PluginError> {
        retention::filter_unregistered_process_ids(self, process_ids).await
    }

    async fn filter_tombstoned_process_ids(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<ProcessId>, lash_core_execution::PluginError> {
        retention::filter_tombstoned_process_ids(self, process_ids).await
    }

    async fn live_reference_summary(
        &self,
    ) -> Result<Vec<ProcessLiveReferenceView>, lash_core_execution::PluginError> {
        let records = worklist::collect_non_terminal_records(self).await?;
        Ok(ProcessLiveReferenceView::from_records(records.iter()))
    }

    async fn count_non_terminal_processes(
        &self,
    ) -> Result<usize, lash_core_execution::PluginError> {
        worklist::count_non_terminal_processes(self).await
    }
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessObserverRegistry for SqliteProcessRegistry {
    async fn add_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        by: ProcessObserverBy,
    ) -> Result<(), lash_core_execution::PluginError> {
        self.set_observer(session_id, process_id, by, true, None)
            .await
    }

    async fn add_observer_ref(
        &self,
        session_id: &SessionId,
        process_ref: &ProcessRef,
        by: ProcessObserverBy,
    ) -> Result<(), lash_core_execution::PluginError> {
        self.set_observer(
            session_id,
            &process_ref.process_id,
            by,
            true,
            Some(process_ref.incarnation),
        )
        .await
    }

    async fn remove_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        by: ProcessObserverBy,
    ) -> Result<(), lash_core_execution::PluginError> {
        self.set_observer(session_id, process_id, by, false, None)
            .await
    }

    async fn transfer_observers(
        &self,
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: &[ProcessId],
        by: ProcessObserverBy,
    ) -> Result<(), lash_core_execution::PluginError> {
        let from_session_id = from_session_id.clone();
        let to_session_id = to_session_id.clone();
        let process_ids = process_ids.to_vec();
        let now = self.clock.timestamp_ms();
        let config = self.wake_delivery_config;
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    for process_id in &process_ids {
                        let mut record = Self::require_process_conn(tx, process_id)?;
                        let removed = tx
                            .execute(
                                process_sql().observer.delete.sql(),
                                params![
                                    from_session_id.as_str(),
                                    process_id.as_str(),
                                    record.incarnation.registration_sequence() as i64
                                ],
                            )
                            .map_err(process_sqlite_error)?;
                        if removed == 0 {
                            return Err(lash_core_execution::PluginError::Session(format!(
                                "process `{process_id}` is not observed by `{from_session_id}`"
                            )));
                        }
                        tx.execute(
                            process_sql().observer_sqlite.insert_if_absent.sql(),
                            params![
                                to_session_id.as_str(),
                                process_id.as_str(),
                                record.incarnation.registration_sequence() as i64
                            ],
                        )
                        .map_err(process_sqlite_error)?;
                        Self::append_event_conn(
                            tx,
                            &mut record,
                            ProcessEventAppendRequest::observer_removed(
                                process_id,
                                &from_session_id,
                                &by,
                            ),
                            now,
                            config,
                        )?;
                        Self::append_event_conn(
                            tx,
                            &mut record,
                            ProcessEventAppendRequest::observer_added(
                                process_id,
                                &to_session_id,
                                &by,
                            ),
                            now,
                            config,
                        )?;
                    }
                    Ok(())
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn list_observed_by(
        &self,
        session_id: &SessionId,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, lash_core_execution::PluginError> {
        let session_id = session_id.clone();
        let filter = filter.clone();
        let status = filter
            .status
            .labels()
            .map(|labels| serde_json::to_string(&labels))
            .transpose()
            .map_err(process_decode_error)?;
        let retired_since_ms = filter.retired_since_ms.map(crate::clamp_epoch_ms);
        self.conn
            .call(move |conn| {
                let registry = &process_sql().registry_sqlite;
                let sql = if retired_since_ms.is_some() {
                    registry.list_observed_recent_retired.sql()
                } else {
                    registry.list_observed.sql()
                };
                let mut stmt = conn.prepare(sql)?;
                let rows = stmt.query_map(
                    params![session_id.as_str(), status, retired_since_ms],
                    |row| row.get::<_, String>(0),
                )?;
                rows.map(|row| {
                    serde_json::from_str::<ProcessRecord>(&row?).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(err),
                        )
                    })
                })
                .filter(|result| {
                    result
                        .as_ref()
                        .map_or(true, |record| filter.matches_record(record))
                })
                .collect()
            })
            .await
            .map_err(process_sqlite_error)
    }

    async fn is_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
    ) -> Result<bool, lash_core_execution::PluginError> {
        let session_id = session_id.clone();
        let process_id = process_id.clone();
        let queried_process_id = process_id.clone();
        let (retained, observer) = self
            .conn
            .call(move |conn| {
                let retained = conn.query_row(
                    process_sql().process.exists_by_id.sql(),
                    params![queried_process_id.as_str()],
                    |row| row.get::<_, bool>(0),
                )?;
                let observer = conn.query_row(
                    process_sql().observer_sqlite.exists.sql(),
                    params![session_id.as_str(), queried_process_id.as_str()],
                    |row| row.get::<_, bool>(0),
                )?;
                Ok((retained, observer))
            })
            .await
            .map_err(process_sqlite_error)?;
        if retained {
            return Ok(observer);
        }
        self.get_process(&process_id).await?;
        Ok(false)
    }

    async fn observers_for_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<Vec<SessionId>, lash_core_execution::PluginError> {
        let process_id = process_id.clone();
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let record = Self::require_process_conn(conn, &process_id)?;
                    let mut stmt = conn
                        .prepare(
                            process_sql()
                                .observer_sqlite
                                .list_sessions_for_incarnation
                                .sql(),
                        )
                        .map_err(process_sqlite_error)?;
                    stmt.query_map(
                        params![
                            process_id.as_str(),
                            record.incarnation.registration_sequence()
                        ],
                        |row| row.get::<_, String>(0).map(SessionId::from),
                    )
                    .map_err(process_sqlite_error)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(process_sqlite_error)
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn retarget_subscription(
        &self,
        process_id: &ProcessId,
        target: Option<&str>,
    ) -> Result<(), lash_core_execution::PluginError> {
        self.retarget_subscription_impl(process_id, target).await
    }

    async fn delete_session_process_state(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core_execution::ProcessSessionDeleteReport, lash_core_execution::PluginError>
    {
        let session_id_owned = session_id.to_string();
        let (removed_observer_count, discarded_wake_delivery_count, cleared_subscription_count) =
            self.conn
                .write_flow(move |tx| {
                    Ok(tx_outcome((|| {
                        let session_id = session_id_owned;
                        let discarded_wake_delivery_count = tx
                            .execute(
                                process_sql().wake.discard_target_gone.sql(),
                                params![session_id],
                            )
                            .map_err(process_sqlite_error)?;
                        let removed_observer_count = tx
                            .execute(
                                process_sql().observer.delete_by_session.sql(),
                                params![session_id],
                            )
                            .map_err(process_sqlite_error)?;
                        let cleared_subscription_count = tx
                            .execute(
                                process_sql().process.clear_wake_session_for_session.sql(),
                                params![session_id],
                            )
                            .map_err(process_sqlite_error)?;
                        tx.execute(
                            process_sql().floor.delete_by_session.sql(),
                            params![session_id],
                        )
                        .map_err(process_sqlite_error)?;
                        Ok((
                            removed_observer_count,
                            discarded_wake_delivery_count,
                            cleared_subscription_count,
                        ))
                    })()))
                })
                .await
                .map_err(process_sqlite_error)??;
        Ok(lash_core_execution::ProcessSessionDeleteReport {
            session_id: session_id.clone(),
            removed_observer_count,
            discarded_wake_delivery_count,
            cleared_subscription_count,
        })
    }
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessEventLog for SqliteProcessRegistry {
    async fn append_event(
        &self,
        process_id: &ProcessId,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendReceipt, lash_core_execution::PluginError> {
        facade_support::validate_generic_process_event_append(&request)?;
        let process_id = process_id.clone();
        let occurred_at_ms = self.clock.timestamp_ms();
        let wake_delivery_config = self.wake_delivery_config;
        let (result, _appended) = self
            .conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_conn(tx, &process_id)?;
                    Self::append_event_conn(
                        tx,
                        &mut record,
                        request,
                        occurred_at_ms,
                        wake_delivery_config,
                    )
                })()))
            })
            .await
            .map_err(process_sqlite_error)??;
        Ok(result)
    }

    async fn append_event_ref(
        &self,
        process_ref: &ProcessRef,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendReceipt, lash_core_execution::PluginError> {
        facade_support::validate_generic_process_event_append(&request)?;
        let process_ref = process_ref.clone();
        let occurred_at_ms = self.clock.timestamp_ms();
        let wake_delivery_config = self.wake_delivery_config;
        let (result, _appended) = self
            .conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_ref_conn(tx, &process_ref)?;
                    Self::append_event_conn(
                        tx,
                        &mut record,
                        request,
                        occurred_at_ms,
                        wake_delivery_config,
                    )
                })()))
            })
            .await
            .map_err(process_sqlite_error)??;
        Ok(result)
    }

    async fn append_event_with_authority(
        &self,
        process_id: &ProcessId,
        request: ProcessEventAppendRequest,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessEventAppendReceipt, lash_core_execution::PluginError> {
        let process_id = process_id.clone();
        let authority = authority.clone();
        let occurred_at_ms = self.clock.timestamp_ms();
        let wake_delivery_config = self.wake_delivery_config;
        let (result, _appended) = self
            .conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_conn(tx, &process_id)?;
                    validate_process_execution_authority_conn(
                        tx,
                        &process_id,
                        &record,
                        &authority,
                        None,
                        occurred_at_ms,
                    )?;
                    Self::append_event_conn(
                        tx,
                        &mut record,
                        request,
                        occurred_at_ms,
                        wake_delivery_config,
                    )
                })()))
            })
            .await
            .map_err(process_sqlite_error)??;
        Ok(result)
    }

    async fn event_page(
        &self,
        process_id: &ProcessId,
        limit: std::num::NonZeroUsize,
        mode: lash_core_execution::ProcessEventQueryMode,
        continuation: Option<lash_core_execution::ProcessEventPageToken>,
    ) -> Result<
        lash_core_execution::ProcessEventReadOutcome<lash_core_execution::ProcessEventPage>,
        lash_core_execution::PluginError,
    > {
        let process_id = process_id.clone();
        #[cfg(feature = "testing")]
        let read_pause = self.conn.fault_injector();
        self.conn
            .read(move |conn| {
                Ok((|| {
                    if let Some(token) = continuation.as_ref() {
                        if token.process_id() != process_id {
                            return Err(lash_core_execution::PluginError::Session(format!(
                                "process event page token belongs to `{}`, not `{process_id}`",
                                token.process_id()
                            )));
                        }
                        if token.mode() != mode {
                            return Err(lash_core_execution::PluginError::Session(
                                "process event page token projection does not match the requested mode"
                                    .to_string(),
                            ));
                        }
                    }
                    let record_result = match continuation.as_ref() {
                        Some(token) => Self::require_process_ref_conn(
                            conn,
                            &ProcessRef::new(process_id.clone(), token.process_incarnation()),
                        ),
                        None => Self::require_process_conn(conn, &process_id),
                    };
                    let record = match record_result {
                        Ok(record) => record,
                        Err(lash_core_execution::PluginError::ProcessNoLongerRetained {
                            terminal_label,
                            pruned_at_ms,
                        }) => {
                            return Ok(lash_core_execution::ProcessEventReadOutcome::NoLongerRetained(
                                lash_core_execution::ProcessEventHistoryRetention::Pruned {
                                    terminal_label,
                                    pruned_at_ms,
                                },
                            ));
                        }
                        Err(lash_core_execution::PluginError::ProcessIncarnationSuperseded {
                            requested_incarnation,
                            current_incarnation,
                            ..
                        }) => {
                            return Ok(lash_core_execution::ProcessEventReadOutcome::NoLongerRetained(
                                lash_core_execution::ProcessEventHistoryRetention::Retired {
                                    requested_incarnation,
                                    current_incarnation,
                                },
                            ));
                        }
                        Err(error) => return Err(error),
                    };
                    #[cfg(feature = "testing")]
                    if let Some(injector) = read_pause.as_ref() {
                        injector.reach_process_event_page_after_identity();
                    }
                    let after_sequence = continuation
                        .as_ref()
                        .map_or(0, lash_core_execution::ProcessEventPageToken::after_sequence);
                    let after_sequence = i64::try_from(after_sequence).map_err(|_| {
                        lash_core_execution::PluginError::Session(
                            "process event page token sequence exceeds the SQL cursor range"
                                .to_string(),
                        )
                    })?;
                    let fetch_limit = limit
                        .get()
                        .checked_add(1)
                        .and_then(|value| i64::try_from(value).ok())
                        .ok_or_else(|| {
                            lash_core_execution::PluginError::Session(
                                "process event page limit is too large".to_string(),
                            )
                        })?;
                    let page = match mode {
                        lash_core_execution::ProcessEventQueryMode::Full => {
                            let mut stmt = conn
                                .prepare(process_sql().event.page_full.sql())
                                .map_err(process_sqlite_error)?;
                            let rows = stmt
                                .query_map(
                                    params![
                                        process_id.as_str(),
                                        record.incarnation.registration_sequence() as i64,
                                        after_sequence,
                                        fetch_limit,
                                    ],
                                    |row| row.get::<_, String>(0),
                                )
                                .map_err(process_sqlite_error)?;
                            let mut events = Vec::new();
                            for row in rows {
                                events.push(
                                    serde_json::from_str(&row.map_err(process_sqlite_error)?)
                                        .map_err(process_decode_error)?,
                                );
                            }
                            lash_core_execution::ProcessEventPage::from_full_rows(
                                events,
                                limit,
                                &process_id,
                                record.incarnation,
                            )
                        }
                        lash_core_execution::ProcessEventQueryMode::Lite => {
                            let mut stmt = conn
                                .prepare(process_sql().event.page_lite.sql())
                                .map_err(process_sqlite_error)?;
                            let rows = stmt
                                .query_map(
                                    params![
                                        process_id.as_str(),
                                        record.incarnation.registration_sequence() as i64,
                                        after_sequence,
                                        fetch_limit,
                                    ],
                                    |row| {
                                        Ok(lash_core_execution::ProcessEventLite {
                                            sequence: plugin_u64_from_sql(
                                                "ProcessEventLite",
                                                "sequence",
                                                row.get(0)?,
                                            )
                                            .map_err(|error| {
                                                rusqlite::Error::FromSqlConversionFailure(
                                                    0,
                                                    rusqlite::types::Type::Integer,
                                                    Box::new(error),
                                                )
                                            })?,
                                            event_type: row.get(1)?,
                                        })
                                    },
                                )
                                .map_err(process_sqlite_error)?;
                            let events = rows
                                .map(|row| row.map_err(process_sqlite_error))
                                .collect::<Result<Vec<_>, _>>()?;
                            lash_core_execution::ProcessEventPage::from_lite_rows(
                                events,
                                limit,
                                &process_id,
                                record.incarnation,
                            )
                        }
                    };
                    Ok(lash_core_execution::ProcessEventReadOutcome::Retained(page))
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn count_events_through(
        &self,
        process_id: &ProcessId,
        event_type: &str,
        up_to_sequence: u64,
    ) -> Result<u64, lash_core_execution::PluginError> {
        let process_id = process_id.clone();
        let event_type = event_type.to_string();
        self.conn
            .call(move |conn| {
                Ok((|| {
                    Self::require_process_conn(conn, &process_id)?;
                    conn.query_row(
                        process_sql().event.count_by_type_through_sequence.sql(),
                        params![
                            process_id.as_str(),
                            event_type,
                            crate::clamp_sequence_bound(up_to_sequence)
                        ],
                        |row| row.get::<_, i64>(0),
                    )
                    .map(|count| count as u64)
                    .map_err(process_sqlite_error)
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn count_events_through_ref(
        &self,
        process_ref: &ProcessRef,
        event_type: &str,
        up_to_sequence: u64,
    ) -> Result<u64, lash_core_execution::PluginError> {
        let process_ref = process_ref.clone();
        let event_type = event_type.to_string();
        self.conn
            .call(move |conn| {
                Ok((|| {
                    Self::require_process_ref_conn(conn, &process_ref)?;
                    conn.query_row(
                        process_sql()
                            .event
                            .count_by_incarnation_type_through_sequence
                            .sql(),
                        params![
                            process_ref.process_id.as_str(),
                            process_ref.incarnation.registration_sequence() as i64,
                            event_type,
                            crate::clamp_sequence_bound(up_to_sequence),
                        ],
                        |row| row.get::<_, i64>(0),
                    )
                    .map(|count| count as u64)
                    .map_err(process_sqlite_error)
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn recent_events(
        &self,
        process_id: &ProcessId,
        limit: usize,
    ) -> Result<Vec<ProcessEvent>, lash_core_execution::PluginError> {
        support::recent_events(self, process_id, limit).await
    }
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessLifecycle for SqliteProcessRegistry {
    async fn complete_process(
        &self,
        process_id: &ProcessId,
        await_output: ProcessAwaitOutput,
        authority: lash_core_execution::ProcessCompletionAuthority,
    ) -> Result<lash_core_execution::ProcessCompletionOutcome, lash_core_execution::PluginError>
    {
        // Load, validate the authority against the row's declared disposition,
        // and append the terminal event as one atomic transaction, so a
        // concurrent complete→prune→re-register cannot slip a different
        // disposition between the validation and the append.
        super::process_registry_completion::complete_process(
            self,
            process_id,
            await_output,
            authority,
        )
        .await
    }

    async fn complete_process_with_lease(
        &self,
        lease: &ProcessLease,
        await_output: ProcessAwaitOutput,
    ) -> Result<lash_core_execution::ProcessCompletionOutcome, lash_core_execution::PluginError>
    {
        super::process_registry_completion::complete_process_with_lease(self, lease, await_output)
            .await
    }

    async fn record_parent_end(
        &self,
        parent: &lash_core_execution::ParentScope,
    ) -> Result<(), lash_core_execution::PluginError> {
        parent_end::record(self, parent).await
    }

    async fn list_pending_parent_end_plans(
        &self,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<lash_core_execution::ParentEndPlan>, lash_core_execution::PluginError> {
        parent_end::list_pending(self, limit).await
    }

    async fn get_parent_end_plan(
        &self,
        parent: &lash_core_execution::ParentScope,
    ) -> Result<Option<lash_core_execution::ParentEndPlan>, lash_core_execution::PluginError> {
        parent_end::get(self, parent).await
    }

    async fn list_parent_end_children(
        &self,
        parent: &lash_core_execution::ParentScope,
        after: Option<&ProcessId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<lash_core_execution::ProcessRecord>, lash_core_execution::PluginError> {
        parent_end::children(self, parent, after, limit).await
    }

    async fn settle_parent_end_plan(
        &self,
        parent: &lash_core_execution::ParentScope,
    ) -> Result<(), lash_core_execution::PluginError> {
        parent_end::settle(self, parent).await
    }

    async fn list_unrecorded_opener_parents(
        &self,
        after: Option<&str>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<lash_core_execution::ParentScope>, lash_core_execution::PluginError> {
        parent_end::list_unrecorded_opener_parents(self, after, limit).await
    }

    async fn record_first_started_with_authority(
        &self,
        process_id: &ProcessId,
        started: ProcessStarted,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessStartOutcome, lash_core_execution::PluginError> {
        let process_id = process_id.clone();
        let authority = authority.clone();
        let now = self.clock.timestamp_ms();
        let wake_delivery_config = self.wake_delivery_config;
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_conn(tx, &process_id)?;
                    validate_process_execution_authority_conn(
                        tx,
                        &process_id,
                        &record,
                        &authority,
                        Some(&started),
                        now,
                    )?;
                    match lash_core_execution::runtime::prepare_process_start(
                        &record, &started, &authority,
                    )? {
                        ProcessStartPlan::AlreadyApplied => {
                            return Ok(ProcessStartOutcome::AlreadyApplied(record));
                        }
                        ProcessStartPlan::AlreadyStarted { by } => {
                            return Ok(ProcessStartOutcome::AlreadyStarted {
                                current: record,
                                by,
                            });
                        }
                        ProcessStartPlan::AttemptsExhausted {
                            attempts,
                            max_attempts,
                        } => {
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
                    let request = ProcessEventAppendRequest::first_started(
                        &process_id,
                        &started,
                        resumed_from_handover,
                    );
                    Self::append_event_conn(tx, &mut record, request, now, wake_delivery_config)?;
                    Ok(ProcessStartOutcome::Started(record))
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn request_process_cancel(
        &self,
        process_ref: &ProcessRef,
        origin: lash_core_execution::CancelOrigin,
        requester: String,
        attribution: Option<lash_core_execution::RuntimeReplayAttribution>,
    ) -> Result<ProcessRecord, lash_core_execution::PluginError> {
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
        origin: lash_core_execution::CancelOrigin,
        requester: String,
        attribution: Option<lash_core_execution::RuntimeReplayAttribution>,
    ) -> Result<
        (ProcessRecord, lash_core_execution::StoreRealization),
        lash_core_execution::PluginError,
    > {
        let process_ref = process_ref.clone();
        let now = self.clock.timestamp_ms();
        let request = lash_core_execution::CancelRequest::new(origin, requester, now);
        let wake_delivery_config = self.wake_delivery_config;
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_ref_conn(tx, &process_ref)?;
                    match lash_core_execution::runtime::prepare_process_transition(
                        &record,
                        ProcessTransition::RequestCancel(request),
                    )? {
                        ProcessTransitionPlan::Unchanged => {
                            return Ok((record, lash_core_execution::StoreRealization::Coalesced));
                        }
                        ProcessTransitionPlan::Append(mut append) => {
                            if let Some(replay) = append.replay.as_mut() {
                                replay.attribution = attribution;
                            }
                            Self::append_event_conn(
                                tx,
                                &mut record,
                                *append,
                                now,
                                wake_delivery_config,
                            )?;
                        }
                    }
                    Ok((record, lash_core_execution::StoreRealization::Realized))
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn request_process_abandon(
        &self,
        process_id: &ProcessId,
        request: AbandonRequest,
    ) -> Result<ProcessRecord, lash_core_execution::PluginError> {
        let process_id = process_id.clone();
        let now = self.clock.timestamp_ms();
        let wake_delivery_config = self.wake_delivery_config;
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_conn(tx, &process_id)?;
                    match lash_core_execution::runtime::prepare_process_transition(
                        &record,
                        ProcessTransition::RequestAbandon(request),
                    )? {
                        ProcessTransitionPlan::Unchanged => return Ok(record),
                        ProcessTransitionPlan::Append(append) => {
                            Self::append_event_conn(
                                tx,
                                &mut record,
                                *append,
                                now,
                                wake_delivery_config,
                            )?;
                        }
                    }
                    Ok(record)
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn record_caller_departure(
        &self,
        process_id: &ProcessId,
    ) -> Result<ProcessRecord, lash_core_execution::PluginError> {
        let process_id = process_id.clone();
        let now = self.clock.timestamp_ms();
        let wake_delivery_config = self.wake_delivery_config;
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_conn(tx, &process_id)?;
                    match lash_core_execution::runtime::prepare_process_transition(
                        &record,
                        ProcessTransition::RecordCallerDeparture,
                    )? {
                        ProcessTransitionPlan::Unchanged => return Ok(record),
                        ProcessTransitionPlan::Append(append) => {
                            Self::append_event_conn(
                                tx,
                                &mut record,
                                *append,
                                now,
                                wake_delivery_config,
                            )?;
                        }
                    }
                    Ok(record)
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn set_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        wait: lash_core_execution::WaitState,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, lash_core_execution::PluginError> {
        let process_id = process_id.clone();
        let authority = authority.clone();
        let now = self.clock.timestamp_ms();
        let wake_delivery_config = self.wake_delivery_config;
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_conn(tx, &process_id)?;
                    validate_process_execution_authority_conn(
                        tx,
                        &process_id,
                        &record,
                        &authority,
                        None,
                        now,
                    )?;
                    match lash_core_execution::runtime::prepare_process_transition(
                        &record,
                        ProcessTransition::EnterWait(wait),
                    )? {
                        ProcessTransitionPlan::Unchanged => return Ok(record),
                        ProcessTransitionPlan::Append(request) => {
                            Self::append_event_conn(
                                tx,
                                &mut record,
                                *request,
                                now,
                                wake_delivery_config,
                            )?;
                        }
                    }
                    Ok(record)
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn clear_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, lash_core_execution::PluginError> {
        let process_id = process_id.clone();
        let authority = authority.clone();
        let now = self.clock.timestamp_ms();
        let wake_delivery_config = self.wake_delivery_config;
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_conn(tx, &process_id)?;
                    validate_process_execution_authority_conn(
                        tx,
                        &process_id,
                        &record,
                        &authority,
                        None,
                        now,
                    )?;
                    match lash_core_execution::runtime::prepare_process_transition(
                        &record,
                        ProcessTransition::ClearWait,
                    )? {
                        ProcessTransitionPlan::Unchanged => return Ok(record),
                        ProcessTransitionPlan::Append(request) => {
                            Self::append_event_conn(
                                tx,
                                &mut record,
                                *request,
                                now,
                                wake_delivery_config,
                            )?;
                        }
                    }
                    Ok(record)
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessToolIntents for SqliteProcessRegistry {
    async fn admit_tool_intent_submission(
        &self,
        submission: lash_core_execution::ToolIntentSubmissionRecord,
    ) -> Result<lash_core_execution::ToolIntentSubmissionAdmission, lash_core_execution::PluginError>
    {
        tool_intent_submission::admit(self, submission).await
    }

    async fn complete_tool_intent_submission(
        &self,
        replay_key: &str,
        outcome: lash_core_execution::ToolIntentExecutionOutcome,
    ) -> Result<lash_core_execution::ToolIntentSubmissionRecord, lash_core_execution::PluginError>
    {
        tool_intent_submission::complete(self, replay_key, outcome).await
    }
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessWakeOutbox for SqliteProcessRegistry {
    fn wake_delivery_config(&self) -> lash_core_execution::WakeDeliveryConfig {
        self.wake_delivery_config
    }

    async fn claim_pending_wake_deliveries(
        &self,
        limit: usize,
    ) -> Result<Vec<lash_core_execution::WakeDelivery>, lash_core_execution::PluginError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let now = self.clock.timestamp_ms();
        let enqueuing_stale_after_ms = self.wake_delivery_config.enqueuing_stale_after_ms;
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    tx.execute(
                        process_sql().wake.reclaim_lapsed_claims.sql(),
                        params![now as i64],
                    )
                    .map_err(process_sqlite_error)?;
                    let ids = {
                        // The non-blocking discard reasons travel as a JSON
                        // array, the way every other bound id list does in
                        // this store: the set is generated from
                        // `WakeDiscardReason`, so a statement that spelled one
                        // placeholder per label would have to be rebuilt
                        // whenever a variant is added.
                        let non_blocking_labels = serde_json::to_string(
                            lash_core_execution::WakeDiscardReason::NON_BLOCKING_ORDERING_GROUP_LABELS,
                        )
                        .map_err(process_decode_error)?;
                        let mut stmt = tx
                            .prepare(process_sql().wake_sqlite.select_claimable.sql())
                            .map_err(process_sqlite_error)?;
                        stmt.query_map(
                            params![now as i64, non_blocking_labels, limit as i64],
                            |row| row.get::<_, String>(0),
                        )
                        .map_err(process_sqlite_error)?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(process_sqlite_error)?
                    };
                    for id in &ids {
                        let claim_token = uuid::Uuid::new_v4().to_string();
                        tx.execute(
                            process_sql().wake.start_enqueuing.sql(),
                            params![
                                id,
                                now as i64,
                                now.saturating_add(enqueuing_stale_after_ms) as i64,
                                claim_token,
                            ],
                        )
                        .map_err(process_sqlite_error)?;
                    }
                    ids.iter()
                        .map(|id| load_wake_delivery_conn(tx, id))
                        .collect()
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn list_wake_deliveries(
        &self,
        state: Option<lash_core_execution::WakeDeliveryState>,
    ) -> Result<Vec<lash_core_execution::WakeDelivery>, lash_core_execution::PluginError> {
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let wake = &process_sql().wake_sqlite;
                    let sql = if state.is_some() {
                        wake.list_delivery_ids_by_state.sql()
                    } else {
                        wake.list_delivery_ids.sql()
                    };
                    let mut stmt = conn.prepare(sql).map_err(process_sqlite_error)?;
                    let ids = if let Some(state) = state {
                        stmt.query_map(params![state.as_str()], |row| row.get::<_, String>(0))
                            .map_err(process_sqlite_error)?
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(process_sqlite_error)?
                    } else {
                        stmt.query_map([], |row| row.get::<_, String>(0))
                            .map_err(process_sqlite_error)?
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(process_sqlite_error)?
                    };
                    ids.iter()
                        .map(|id| load_wake_delivery_conn(conn, id))
                        .collect()
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn wake_delivery_report(
        &self,
    ) -> Result<lash_core_execution::WakeDeliveryReport, lash_core_execution::PluginError> {
        let deliveries = self.list_wake_deliveries(None).await?;
        Ok(wake_delivery_report(deliveries.iter()))
    }

    async fn mark_wake_enqueued(
        &self,
        delivery_id: &str,
        claim_token: &str,
    ) -> Result<lash_core_execution::WakeDeliveryClaimOutcome, lash_core_execution::PluginError>
    {
        let disposition = lash_core_execution::WakeDeliveryDisposition::Enqueued;
        update_wake_delivery_state(&self.conn, delivery_id, claim_token, disposition).await
    }

    async fn discard_wake_delivery(
        &self,
        delivery_id: &str,
        claim_token: &str,
        reason: lash_core_execution::WakeDiscardReason,
    ) -> Result<lash_core_execution::WakeDeliveryClaimOutcome, lash_core_execution::PluginError>
    {
        let disposition = lash_core_execution::WakeDeliveryDisposition::Discarded { reason };
        update_wake_delivery_state(&self.conn, delivery_id, claim_token, disposition).await
    }

    async fn redrive_wake_delivery(
        &self,
        delivery_id: &str,
    ) -> Result<(), lash_core_execution::PluginError> {
        let delivery_id = delivery_id.to_string();
        let expires_at_ms = self
            .clock
            .timestamp_ms()
            .saturating_add(self.wake_delivery_config.delivery_expiry_ms);
        let next_attempt_at_ms = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let changed = tx
                        .execute(
                            process_sql().wake.redrive_discarded.sql(),
                            params![delivery_id, expires_at_ms as i64, next_attempt_at_ms as i64],
                        )
                        .map_err(process_sqlite_error)?;
                    if changed == 0 {
                        return Err(lash_core_execution::PluginError::Session(format!(
                            "wake delivery `{delivery_id}` is not discarded or does not exist"
                        )));
                    }
                    Ok(())
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn defer_wake_delivery(
        &self,
        delivery_id: &str,
        claim_token: &str,
        next_attempt_at_ms: u64,
    ) -> Result<lash_core_execution::WakeDeliveryClaimOutcome, lash_core_execution::PluginError>
    {
        let delivery_id = delivery_id.to_string();
        let claim_token = claim_token.to_string();
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let changed = tx
                        .execute(
                            process_sql().wake.release_claim.sql(),
                            params![delivery_id, claim_token, next_attempt_at_ms as i64],
                        )
                        .map_err(process_sqlite_error)?;
                    if changed == 0 {
                        let delivery = load_wake_delivery_conn(tx, &delivery_id)?;
                        return Ok(lash_core_execution::WakeDeliveryClaimOutcome::ClaimLost {
                            state: delivery.state(),
                        });
                    }
                    Ok(lash_core_execution::WakeDeliveryClaimOutcome::Applied)
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }
}
impl lash_core_execution::ProcessClockRebind for SqliteProcessRegistry {
    fn with_runtime_clock(
        &self,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> Option<Arc<dyn ProcessRegistry>> {
        Some(Arc::new(Self {
            conn: self.conn.clone(),
            clock,
            process_session_catalog: self.process_session_catalog.clone(),
            wake_delivery_config: self.wake_delivery_config,
            scope_fence_hosts: self.scope_fence_hosts.clone(),
            location: self.location.clone(),
        }))
    }
}

fn validate_process_execution_authority_conn(
    conn: &rusqlite::Connection,
    process_id: &ProcessId,
    record: &ProcessRecord,
    authority: &ProcessExecutionWriteAuthority,
    start: Option<&ProcessStarted>,
    now: u64,
) -> Result<(), lash_core_execution::PluginError> {
    match authority {
        ProcessExecutionWriteAuthority::Invocation { .. } => {
            if let Some(started) = start {
                authority.validate_invocation_for_start(
                    process_id,
                    started,
                    record.first_started.as_deref(),
                )
            } else {
                authority.validate_invocation_for_write(process_id, record)
            }
        }
        ProcessExecutionWriteAuthority::Lease { lease, .. } => {
            // The process-id half of the fence is checked first so a lease for
            // another process is refused without reading this process's row.
            if lease.process_id != process_id {
                return Err(lash_core_execution::PluginError::ProcessLeaseSuperseded {
                    process_id: process_id.clone(),
                });
            }
            let current = SqliteProcessRegistry::load_process_lease_conn(conn, process_id)?;
            registry_transitions::authorize_process_lease_write(
                process_id,
                lease,
                current.as_ref(),
                now,
            )
        }
    }
}

#[cfg(any(test, feature = "testing"))]
#[async_trait::async_trait]
impl lash_core_execution::ProcessRegistryTestSupport for SqliteProcessRegistry {
    async fn wake_allocation_floor_for_testing(
        &self,
        target_session_id: &SessionId,
        process_id: &ProcessId,
    ) -> Result<Option<u64>, lash_core_execution::PluginError> {
        support::wake_allocation_floor_for_testing(self, target_session_id, process_id).await
    }
}
