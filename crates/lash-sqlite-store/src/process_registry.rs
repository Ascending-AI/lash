//! SQLite-backed [`ProcessRegistry`] (`SqliteProcessRegistry`).
//!
//! First-party SQLite implementation of the public async process-registry
//! surface. Every DB body is a *synchronous* rusqlite closure handed
//! to [`SqliteConnection::call`] (reads) or [`SqliteConnection::write_flow`]
//! (read-then-write).
//!
//! Transactional methods use `write_flow` so logical [`lash_core::PluginError`]
//! failures roll back instead of committing partial writes through rusqlite's
//! error channel. The `*_conn` helpers are synchronous and accept
//! `&rusqlite::Connection`, so they compose inside connection and transaction
//! closures.

use super::*;

mod segment_handover;
mod support;
mod wake_delivery;

use support::process_status_label;
pub(crate) use support::tx_outcome;
use wake_delivery::{load_wake_delivery_conn, update_wake_delivery_state, wake_delivery_report};

#[async_trait::async_trait]
impl ProcessRegistry for SqliteProcessRegistry {
    fn wake_delivery_config(&self) -> lash_core::WakeDeliveryConfig {
        self.wake_delivery_config
    }

    fn with_runtime_clock(
        &self,
        clock: Arc<dyn lash_core::Clock>,
    ) -> Option<Arc<dyn ProcessRegistry>> {
        Some(Arc::new(Self {
            conn: self.conn.clone(),
            clock,
            process_session_store_root: self.process_session_store_root.clone(),
            wake_delivery_config: self.wake_delivery_config,
        }))
    }

    async fn register_process_with_observers(
        &self,
        registration: ProcessRegistration,
        observers: &[String],
    ) -> Result<ProcessRecord, lash_core::PluginError> {
        let (registration, registration_hash) = prepare_process_registration(registration)?;
        let mut observers = observers.to_vec();
        observers.sort();
        observers.dedup();
        let registration_hash = lash_core::runtime::process_registration_with_observers_hash(
            registration_hash,
            &observers,
        )?;
        let wake_session_id = registration.wake_session_id.clone();
        let now = self.clock.timestamp_ms();
        let wake_delivery_config = self.wake_delivery_config;
        let record = self
            .conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    if let Some(existing) = Self::load_process_conn(tx, &registration.id)? {
                        if existing.registration_hash == registration_hash {
                            return Ok(existing);
                        }
                        return Err(lash_core::PluginError::Session(format!(
                            "process `{}` registration hash conflict: existing {}, new {}",
                            registration.id, existing.registration_hash, registration_hash
                        )));
                    }
                    let record = ProcessRecord::from_prepared_registration(
                        registration,
                        registration_hash,
                        now,
                    );
                    let originator_scope_id = record.originator_scope_id();
                    let change_seq = Self::next_change_seq_conn(tx)?;
                    tx.execute(
                        "INSERT INTO processes (
                            process_id, registration_hash, originator_scope_id, wake_session_id,
                            identity_kind, identity_label, is_waiting,
                            created_at_ms, updated_at_ms, change_seq, status, record_json
                         )
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                        params![
                            record.id.as_str(),
                            record.registration_hash.as_str(),
                            originator_scope_id.as_str(),
                            wake_session_id,
                            record.identity.kind.as_str(),
                            record.identity.label.as_deref(),
                            i64::from(record.wait.is_some()),
                            record.created_at_ms as i64,
                            record.updated_at_ms as i64,
                            change_seq as i64,
                            process_status_label(&record),
                            process_encode_json(&record)?,
                        ],
                    )
                    .map_err(process_sqlite_error)?;
                    let mut record = record;
                    let process_id = record.id.clone();
                    for session_id in &observers {
                        tx.execute(
                            "INSERT INTO process_observers (session_id, process_id)
                             VALUES (?1, ?2)",
                            params![session_id, record.id.as_str()],
                        )
                        .map_err(process_sqlite_error)?;
                        Self::append_event_conn(
                            tx,
                            &mut record,
                            ProcessEventAppendRequest::observer_added(
                                &process_id,
                                session_id,
                                &ProcessObserverBy::host("registration"),
                            ),
                            now,
                            wake_delivery_config,
                        )?;
                    }
                    Ok(record)
                })()))
            })
            .await
            .map_err(process_sqlite_error)??;
        Ok(record)
    }

    async fn set_external_ref(
        &self,
        process_id: &str,
        external_ref: ProcessExternalRef,
    ) -> Result<ProcessRecord, lash_core::PluginError> {
        let process_id = process_id.to_string();
        let now = self.clock.timestamp_ms();
        let wake_delivery_config = self.wake_delivery_config;
        let (record, _changed) = self
            .conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_conn(tx, &process_id)?;
                    if let Some(existing) = &record.external_ref {
                        if existing == &external_ref {
                            return Ok((record, false));
                        }
                        return Err(process_external_ref_conflict(
                            &process_id,
                            existing,
                            &external_ref,
                        ));
                    }
                    let request =
                        ProcessEventAppendRequest::external_ref_set(&process_id, &external_ref);
                    Self::append_event_conn(tx, &mut record, request, now, wake_delivery_config)?;
                    Ok((record, true))
                })()))
            })
            .await
            .map_err(process_sqlite_error)??;
        Ok(record)
    }

    async fn add_observer(
        &self,
        session_id: &str,
        process_id: &str,
        by: ProcessObserverBy,
    ) -> Result<(), lash_core::PluginError> {
        self.set_observer(session_id, process_id, by, true).await
    }

    async fn remove_observer(
        &self,
        session_id: &str,
        process_id: &str,
        by: ProcessObserverBy,
    ) -> Result<(), lash_core::PluginError> {
        self.set_observer(session_id, process_id, by, false).await
    }

    async fn transfer_observers(
        &self,
        from_session_id: &str,
        to_session_id: &str,
        process_ids: &[String],
        by: ProcessObserverBy,
    ) -> Result<(), lash_core::PluginError> {
        let from_session_id = from_session_id.to_string();
        let to_session_id = to_session_id.to_string();
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
                                "DELETE FROM process_observers
                                 WHERE session_id = ?1 AND process_id = ?2",
                                params![from_session_id, process_id],
                            )
                            .map_err(process_sqlite_error)?;
                        if removed == 0 {
                            return Err(lash_core::PluginError::Session(format!(
                                "process `{process_id}` is not observed by `{from_session_id}`"
                            )));
                        }
                        tx.execute(
                            "INSERT OR IGNORE INTO process_observers (session_id, process_id)
                             VALUES (?1, ?2)",
                            params![to_session_id, process_id],
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
        session_id: &str,
    ) -> Result<Vec<ProcessRecord>, lash_core::PluginError> {
        let session_id = session_id.to_string();
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT p.record_json
                     FROM process_observers o
                     JOIN processes p ON p.process_id = o.process_id
                     WHERE o.session_id = ?1
                     ORDER BY p.process_id",
                )?;
                let rows = stmt.query_map(params![session_id], |row| row.get::<_, String>(0))?;
                rows.map(|row| {
                    serde_json::from_str(&row?).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(err),
                        )
                    })
                })
                .collect()
            })
            .await
            .map_err(process_sqlite_error)
    }

    async fn observers_for_process(
        &self,
        process_id: &str,
    ) -> Result<Vec<String>, lash_core::PluginError> {
        let process_id = process_id.to_string();
        self.conn
            .call(move |conn| {
                Ok((|| {
                    Self::require_process_conn(conn, &process_id)?;
                    let mut stmt = conn
                        .prepare(
                            "SELECT session_id FROM process_observers
                             WHERE process_id = ?1 ORDER BY session_id",
                        )
                        .map_err(process_sqlite_error)?;
                    stmt.query_map(params![process_id], |row| row.get(0))
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
        process_id: &str,
        target: Option<&str>,
    ) -> Result<(), lash_core::PluginError> {
        self.retarget_subscription_impl(process_id, target).await
    }

    async fn delete_session_process_state(
        &self,
        session_id: &str,
    ) -> Result<lash_core::ProcessSessionDeleteReport, lash_core::PluginError> {
        let session_id_owned = session_id.to_string();
        let (removed_observer_count, discarded_wake_delivery_count, cleared_subscription_count) =
            self.conn
                .write_flow(move |tx| {
                    Ok(tx_outcome((|| {
                        let session_id = session_id_owned;
                        let discarded_wake_delivery_count = tx
                            .execute(
                                "UPDATE process_wake_deliveries
                             SET state = 'discarded', discard_reason = 'target_gone'
                             WHERE target_session_id = ?1 AND state = 'pending'",
                                params![session_id],
                            )
                            .map_err(process_sqlite_error)?;
                        let removed_observer_count = tx
                            .execute(
                                "DELETE FROM process_observers WHERE session_id = ?1",
                                params![session_id],
                            )
                            .map_err(process_sqlite_error)?;
                        let cleared_subscription_count = tx
                            .execute(
                                "UPDATE processes SET wake_session_id = NULL
                             WHERE wake_session_id = ?1",
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
        Ok(lash_core::ProcessSessionDeleteReport {
            session_id: session_id.to_string(),
            removed_observer_count,
            discarded_wake_delivery_count,
            cleared_subscription_count,
        })
    }

    async fn append_event(
        &self,
        process_id: &str,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendResult, lash_core::PluginError> {
        let process_id = process_id.to_string();
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

    async fn append_event_with_authority(
        &self,
        process_id: &str,
        request: ProcessEventAppendRequest,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessEventAppendResult, lash_core::PluginError> {
        let process_id = process_id.to_string();
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

    async fn events_after(
        &self,
        process_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<ProcessEvent>, lash_core::PluginError> {
        let process_id = process_id.to_string();
        self.conn
            .call(move |conn| {
                Ok((|| {
                    Self::require_process_conn(conn, &process_id)?;
                    let mut stmt = conn
                        .prepare(
                            "SELECT event_json FROM process_events
                             WHERE process_id = ?1 AND sequence > ?2
                             ORDER BY sequence ASC",
                        )
                        .map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map(params![process_id, after_sequence as i64], |row| {
                            row.get::<_, String>(0)
                        })
                        .map_err(process_sqlite_error)?;
                    let mut events = Vec::new();
                    for row in rows {
                        events.push(
                            serde_json::from_str(&row.map_err(process_sqlite_error)?)
                                .map_err(process_decode_error)?,
                        );
                    }
                    Ok(events)
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn count_events_through(
        &self,
        process_id: &str,
        event_type: &str,
        up_to_sequence: u64,
    ) -> Result<u64, lash_core::PluginError> {
        let process_id = process_id.to_string();
        let event_type = event_type.to_string();
        self.conn
            .call(move |conn| {
                Ok((|| {
                    Self::require_process_conn(conn, &process_id)?;
                    conn.query_row(
                        "SELECT COUNT(*) FROM process_events
                         WHERE process_id = ?1 AND event_type = ?2 AND sequence <= ?3",
                        params![process_id, event_type, up_to_sequence as i64],
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
        process_id: &str,
        limit: usize,
    ) -> Result<Vec<ProcessEvent>, lash_core::PluginError> {
        let process_id = process_id.to_string();
        self.conn
            .call(move |conn| {
                Ok((|| {
                    Self::require_process_conn(conn, &process_id)?;
                    let mut stmt = conn
                        .prepare(
                            "SELECT event_json FROM process_events
                             WHERE process_id = ?1
                             ORDER BY sequence DESC
                             LIMIT ?2",
                        )
                        .map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map(params![process_id, limit as i64], |row| {
                            row.get::<_, String>(0)
                        })
                        .map_err(process_sqlite_error)?;
                    let mut events: Vec<ProcessEvent> = Vec::new();
                    for row in rows {
                        events.push(
                            serde_json::from_str(&row.map_err(process_sqlite_error)?)
                                .map_err(process_decode_error)?,
                        );
                    }
                    events.reverse();
                    Ok(events)
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn complete_process(
        &self,
        process_id: &str,
        await_output: ProcessAwaitOutput,
        authority: lash_core::ProcessCompletionAuthority,
    ) -> Result<lash_core::ProcessCompletionOutcome, lash_core::PluginError> {
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
    ) -> Result<lash_core::ProcessCompletionOutcome, lash_core::PluginError> {
        super::process_registry_completion::complete_process_with_lease(self, lease, await_output)
            .await
    }

    async fn record_first_started_with_authority(
        &self,
        process_id: &str,
        started: ProcessStarted,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessStartOutcome, lash_core::PluginError> {
        let process_id = process_id.to_string();
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
                    match lash_core::runtime::prepare_process_start(&record, &started, &authority)?
                    {
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

    async fn request_process_abandon(
        &self,
        process_id: &str,
        request: AbandonRequest,
    ) -> Result<ProcessRecord, lash_core::PluginError> {
        let process_id = process_id.to_string();
        let now = self.clock.timestamp_ms();
        let wake_delivery_config = self.wake_delivery_config;
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_conn(tx, &process_id)?;
                    if record.is_terminal() {
                        return Err(lash_core::PluginError::Session(format!(
                            "terminal process `{process_id}` cannot accept an abandon request"
                        )));
                    }
                    if record.abandon_request.is_some() {
                        return Ok(record);
                    }
                    let append =
                        ProcessEventAppendRequest::abandon_requested(&process_id, &request);
                    Self::append_event_conn(tx, &mut record, append, now, wake_delivery_config)?;
                    Ok(record)
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn set_process_wait_with_authority(
        &self,
        process_id: &str,
        wait: lash_core::WaitState,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, lash_core::PluginError> {
        let process_id = process_id.to_string();
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
                    if record.is_terminal() {
                        return Err(lash_core::PluginError::Session(format!(
                            "terminal process `{process_id}` cannot enter a wait state"
                        )));
                    }
                    if record.wait.as_ref() == Some(&wait) {
                        return Ok(record);
                    }
                    let request = ProcessEventAppendRequest::wait_entered(&process_id, &wait);
                    Self::append_event_conn(tx, &mut record, request, now, wake_delivery_config)?;
                    Ok(record)
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn clear_process_wait_with_authority(
        &self,
        process_id: &str,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, lash_core::PluginError> {
        let process_id = process_id.to_string();
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
                    let Some(wait) = record.wait.clone() else {
                        return Ok(record);
                    };
                    let request = ProcessEventAppendRequest::wait_cleared(&process_id, &wait);
                    Self::append_event_conn(tx, &mut record, request, now, wake_delivery_config)?;
                    Ok(record)
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn get_process(
        &self,
        process_id: &str,
    ) -> Result<Option<ProcessRecord>, lash_core::PluginError> {
        let process_id = process_id.to_string();
        self.conn
            .call(move |conn| {
                Ok((|| {
                    if let Some(record) = Self::load_process_conn(conn, &process_id)? {
                        return Ok(Some(record));
                    }
                    let tombstone: Option<(String, i64)> = conn
                        .query_row(
                            "SELECT terminal_label, pruned_at_ms
                             FROM process_tombstones WHERE process_id = ?1",
                            params![process_id],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()
                        .map_err(process_sqlite_error)?;
                    if let Some((terminal_label, pruned_at_ms)) = tombstone {
                        return Err(lash_core::PluginError::ProcessNoLongerRetained {
                            terminal_label,
                            pruned_at_ms: pruned_at_ms as u64,
                        });
                    }
                    Ok(None)
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn list_processes(
        &self,
        filter: &lash_core::ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, lash_core::PluginError> {
        let filter = filter.clone();
        let definition = filter
            .definition
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(process_decode_error)?;
        let status = filter.status.label().map(str::to_string);
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let mut stmt = conn
                        .prepare(
                            "SELECT record_json FROM processes
                             WHERE (?1 IS NULL OR status = ?1)
                               AND (?2 IS NULL OR is_waiting = ?2)
                               AND (?3 IS NULL OR originator_scope_id = ?3)
                               AND (?4 IS NULL OR identity_kind = ?4)
                               AND (?5 IS NULL OR identity_label = ?5)
                               AND (?6 IS NULL OR
                                    json_extract(record_json, '$.identity.definition') = json(?6))
                               AND (?7 IS NULL OR
                                    json_extract(record_json, '$.provenance.caused_by.occurrence_id') = ?7)
                               AND (?8 IS NULL OR
                                    json_extract(record_json, '$.provenance.caused_by.subscription_id') = ?8)
                               AND (?9 IS NULL OR created_at_ms >= ?9)
                               AND (?10 IS NULL OR created_at_ms < ?10)
                             ORDER BY process_id ASC",
                        )
                        .map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map(
                            params![
                                status,
                                filter.waiting.map(i64::from),
                                filter.originator_scope_id,
                                filter.identity_kind,
                                filter.identity_label,
                                definition,
                                filter.caused_by_occurrence_id,
                                filter.caused_by_subscription_id,
                                filter.created_at_start_ms.map(|value| value as i64),
                                filter.created_at_end_ms.map(|value| value as i64),
                            ],
                            |row| row.get::<_, String>(0),
                        )
                        .map_err(process_sqlite_error)?;
                    let mut records = Vec::new();
                    for row in rows {
                        let record: ProcessRecord =
                            serde_json::from_str(&row.map_err(process_sqlite_error)?)
                                .map_err(process_decode_error)?;
                        records.push(record);
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
    ) -> Result<(Vec<ProcessChange>, ProcessChangeCursor), lash_core::PluginError> {
        if limit == 0 {
            return Ok((Vec::new(), cursor));
        }
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

    async fn compact_process_tombstones(
        &self,
        cutoff_epoch_ms: u64,
    ) -> Result<usize, lash_core::PluginError> {
        self.conn
            .call(move |conn| {
                conn.execute(
                    "DELETE FROM process_tombstones WHERE pruned_at_ms < ?1",
                    params![cutoff_epoch_ms as i64],
                )
            })
            .await
            .map_err(process_sqlite_error)
    }

    async fn pending_wake_deliveries(
        &self,
        limit: usize,
    ) -> Result<Vec<lash_core::WakeDelivery>, lash_core::PluginError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let ids = {
                        let mut stmt = tx
                            .prepare(
                                "SELECT candidate.delivery_id
                                 FROM process_wake_deliveries AS candidate
                                 WHERE candidate.state = 'pending'
                                   AND candidate.next_attempt_at_ms <= ?2
                                   AND NOT EXISTS (
                                       SELECT 1
                                       FROM process_wake_deliveries AS earlier
                                       WHERE earlier.state <> 'enqueued'
                                         AND earlier.target_session_id =
                                             candidate.target_session_id
                                         AND earlier.process_id = candidate.process_id
                                         AND earlier.sequence < candidate.sequence
                                   )
                                 ORDER BY candidate.next_attempt_at_ms ASC,
                                          candidate.target_session_id ASC,
                                          candidate.process_id ASC,
                                          candidate.sequence ASC
                                 LIMIT ?1",
                            )
                            .map_err(process_sqlite_error)?;
                        stmt.query_map(params![limit as i64, now as i64], |row| {
                            row.get::<_, String>(0)
                        })
                        .map_err(process_sqlite_error)?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(process_sqlite_error)?
                    };
                    for id in &ids {
                        tx.execute(
                            "UPDATE process_wake_deliveries
                             SET attempts = attempts + 1,
                                 first_attempt_ms = COALESCE(first_attempt_ms, ?2)
                             WHERE delivery_id = ?1 AND state = 'pending'",
                            params![id, now as i64],
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
        state: Option<lash_core::WakeDeliveryState>,
    ) -> Result<Vec<lash_core::WakeDelivery>, lash_core::PluginError> {
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let mut sql = "SELECT delivery_id FROM process_wake_deliveries".to_string();
                    if state.is_some() {
                        sql.push_str(" WHERE state = ?1");
                    }
                    sql.push_str(" ORDER BY delivery_id ASC");
                    let mut stmt = conn.prepare(&sql).map_err(process_sqlite_error)?;
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
    ) -> Result<lash_core::WakeDeliveryReport, lash_core::PluginError> {
        let deliveries = self.list_wake_deliveries(None).await?;
        Ok(wake_delivery_report(deliveries.iter()))
    }

    async fn mark_wake_enqueued(&self, delivery_id: &str) -> Result<(), lash_core::PluginError> {
        update_wake_delivery_state(
            &self.conn,
            delivery_id,
            lash_core::WakeDeliveryState::Enqueued,
            None,
        )
        .await
    }

    async fn discard_wake_delivery(
        &self,
        delivery_id: &str,
        reason: lash_core::WakeDiscardReason,
    ) -> Result<(), lash_core::PluginError> {
        update_wake_delivery_state(
            &self.conn,
            delivery_id,
            lash_core::WakeDeliveryState::Discarded,
            Some(reason),
        )
        .await
    }

    async fn redrive_wake_delivery(&self, delivery_id: &str) -> Result<(), lash_core::PluginError> {
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
                            "UPDATE process_wake_deliveries
                             SET state = 'pending', attempts = 0, first_attempt_ms = NULL,
                                 next_attempt_at_ms = ?3, expires_at_ms = ?2,
                                 discard_reason = NULL
                             WHERE delivery_id = ?1 AND state = 'discarded'",
                            params![delivery_id, expires_at_ms as i64, next_attempt_at_ms as i64],
                        )
                        .map_err(process_sqlite_error)?;
                    if changed == 0 {
                        return Err(lash_core::PluginError::Session(format!(
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
        next_attempt_at_ms: u64,
    ) -> Result<(), lash_core::PluginError> {
        let delivery_id = delivery_id.to_string();
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let changed = tx
                        .execute(
                            "UPDATE process_wake_deliveries
                             SET next_attempt_at_ms = MAX(next_attempt_at_ms, ?2)
                             WHERE delivery_id = ?1 AND state = 'pending'",
                            params![delivery_id, next_attempt_at_ms as i64],
                        )
                        .map_err(process_sqlite_error)?;
                    if changed == 0 {
                        let delivery = load_wake_delivery_conn(tx, &delivery_id)?;
                        return Err(lash_core::PluginError::WakeDeliveryNotPending {
                            delivery_id,
                            state: delivery.state,
                        });
                    }
                    Ok(())
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn list_non_terminal(&self) -> Result<Vec<ProcessRecord>, lash_core::PluginError> {
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let mut stmt = conn
                        .prepare(
                            "SELECT record_json FROM processes
                             WHERE status = 'running'
                             ORDER BY process_id ASC",
                        )
                        .map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map([], |row| row.get::<_, String>(0))
                        .map_err(process_sqlite_error)?;
                    let mut records = Vec::new();
                    for row in rows {
                        let record: ProcessRecord =
                            serde_json::from_str(&row.map_err(process_sqlite_error)?)
                                .map_err(process_decode_error)?;
                        records.push(record);
                    }
                    Ok(records)
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn filter_unregistered_process_ids(
        &self,
        process_ids: &[String],
    ) -> Result<Vec<String>, lash_core::PluginError> {
        if process_ids.is_empty() {
            return Ok(Vec::new());
        }
        let process_ids_json = serde_json::to_string(process_ids).map_err(process_decode_error)?;
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let mut stmt = conn
                        .prepare(
                            "SELECT candidate.value
                             FROM json_each(?1) AS candidate
                             WHERE NOT EXISTS (
                                 SELECT 1 FROM processes p
                                 WHERE p.process_id = candidate.value
                             )
                             ORDER BY candidate.key ASC",
                        )
                        .map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map(params![process_ids_json], |row| row.get::<_, String>(0))
                        .map_err(process_sqlite_error)?;
                    rows.collect::<Result<Vec<_>, _>>()
                        .map_err(process_sqlite_error)
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn live_reference_summary(
        &self,
    ) -> Result<Vec<ProcessLiveReferenceSummary>, lash_core::PluginError> {
        let records = self.list_non_terminal().await?;
        Ok(ProcessLiveReferenceSummary::from_records(records.iter()))
    }

    async fn claim_process_lease(
        &self,
        process_id: &str,
        owner: &LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLeaseClaimOutcome, lash_core::PluginError> {
        let process_id = process_id.to_string();
        let owner = owner.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    Self::require_process_conn(tx, &process_id)?;
                    let current = Self::load_process_lease_conn(tx, &process_id)?;
                    if let Some(current) = current.as_ref()
                        && current.expires_at_epoch_ms > now
                    {
                        if current.owner.same_incarnation(&owner) {
                            // Same incarnation re-enters its own live lease:
                            // extend the expiry, keep token and fencing token.
                            let expires_at = now.saturating_add(lease_ttl_ms);
                            tx.execute(
                                "UPDATE process_leases
                                 SET lease_expires_at_ms = ?2
                                 WHERE process_id = ?1",
                                params![process_id, expires_at as i64],
                            )
                            .map_err(process_sqlite_error)?;
                            return Ok(ProcessLeaseClaimOutcome::Acquired(ProcessLease {
                                expires_at_epoch_ms: expires_at,
                                ..current.clone()
                            }));
                        }
                        return Ok(ProcessLeaseClaimOutcome::Busy {
                            holder: current.clone(),
                        });
                    }
                    // Read the raw fencing token directly: a completed/abandoned
                    // lease nulls the owner/token columns but retains the
                    // monotonically-increasing `lease_fencing_token`, so a
                    // re-claim never reuses a stale writer's token.
                    let fencing_token: u64 = tx
                        .query_row(
                            "SELECT lease_fencing_token FROM process_leases WHERE process_id = ?1",
                            params![process_id],
                            |row| row.get::<_, i64>(0),
                        )
                        .optional()
                        .map_err(process_sqlite_error)?
                        .unwrap_or(0) as u64
                        + 1;
                    Ok(ProcessLeaseClaimOutcome::Acquired(
                        Self::acquire_process_lease_conn(
                            tx,
                            &process_id,
                            &owner,
                            fencing_token,
                            now,
                            lease_ttl_ms,
                        )?,
                    ))
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn reclaim_process_lease(
        &self,
        process_id: &str,
        owner: &LeaseOwnerIdentity,
        _observed_holder: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLeaseClaimOutcome, lash_core::PluginError> {
        let process_id = process_id.to_string();
        let owner = owner.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    Self::require_process_conn(tx, &process_id)?;
                    let current = Self::load_process_lease_conn(tx, &process_id)?;
                    let Some(current) = current else {
                        // Free (or released) lease: acquire on the retained
                        // fencing token like a plain claim would.
                        let fencing_token: u64 = tx
                            .query_row(
                                "SELECT lease_fencing_token FROM process_leases WHERE process_id = ?1",
                                params![process_id],
                                |row| row.get::<_, i64>(0),
                            )
                            .optional()
                            .map_err(process_sqlite_error)?
                            .unwrap_or(0) as u64
                            + 1;
                        return Ok(ProcessLeaseClaimOutcome::Acquired(
                            Self::acquire_process_lease_conn(
                                tx,
                                &process_id,
                                &owner,
                                fencing_token,
                                now,
                                lease_ttl_ms,
                            )?,
                        ));
                    };
                    if current.expires_at_epoch_ms <= now {
                        return Ok(ProcessLeaseClaimOutcome::Acquired(
                            Self::acquire_process_lease_conn(
                                tx,
                                &process_id,
                                &owner,
                                current.fencing_token.saturating_add(1),
                                now,
                                lease_ttl_ms,
                            )?,
                        ));
                    }
                    Ok(ProcessLeaseClaimOutcome::Busy { holder: current })
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn renew_process_lease(
        &self,
        lease: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLease, lash_core::PluginError> {
        let lease = lease.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let current = Self::load_process_lease_conn(tx, &lease.process_id)?;
                    if !guard_lease(current.as_ref(), &lease.lease_token, now)
                        || !current.as_ref().is_some_and(|current| {
                            current.owner.same_incarnation(&lease.owner)
                                && current.fencing_token == lease.fencing_token
                        })
                    {
                        return Err(process_lease_expired(&lease.process_id));
                    }
                    let renewed = ProcessLease {
                        expires_at_epoch_ms: now.saturating_add(lease_ttl_ms),
                        ..lease.clone()
                    };
                    tx.execute(
                        "UPDATE process_leases
                         SET lease_expires_at_ms = ?2
                         WHERE process_id = ?1 AND lease_token = ?3",
                        params![
                            renewed.process_id.as_str(),
                            renewed.expires_at_epoch_ms as i64,
                            renewed.lease_token.as_str(),
                        ],
                    )
                    .map_err(process_sqlite_error)?;
                    Ok(renewed)
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn get_process_lease(
        &self,
        process_id: &str,
    ) -> Result<Option<ProcessLease>, lash_core::PluginError> {
        let process_id = process_id.to_string();
        self.conn
            .call(move |conn| Ok(Self::load_process_lease_conn(conn, &process_id)))
            .await
            .map_err(process_sqlite_error)?
    }

    async fn complete_process_lease(
        &self,
        completion: &ProcessLeaseCompletion,
    ) -> Result<(), lash_core::PluginError> {
        let process_id = completion.process_id.clone();
        let lease_token = completion.lease_token.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE process_leases
                     SET lease_owner_id = NULL,
                         lease_token = NULL,
                         lease_claimed_at_ms = 0,
                         lease_expires_at_ms = 0
                     WHERE process_id = ?1 AND lease_token = ?2",
                    params![process_id, lease_token],
                )
            })
            .await
            .map_err(process_sqlite_error)?;
        Ok(())
    }

    async fn prune_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: lash_core::ProjectionWatermark,
    ) -> Result<ProcessPruneReport, lash_core::PluginError> {
        let cutoff = cutoff_epoch_ms as i64;
        let max_change_seq = match watermark {
            lash_core::ProjectionWatermark::UpTo(cursor) => Some(cursor.store_sequence()),
            lash_core::ProjectionWatermark::NoProjector => None,
        };
        if let Some(root) = self.process_session_store_root.as_ref() {
            let selection_filter = filter.clone();
            let prunable = self
                .conn
                .call(move |conn| {
                    crate::process_registry_change::prunable_terminal_process_ids_conn(
                        conn,
                        cutoff,
                        selection_filter,
                        max_change_seq,
                    )
                    .map_err(|err| {
                        rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                            err.to_string(),
                        )))
                    })
                })
                .await
                .map_err(process_sqlite_error)?;
            // Delete process-owned session stores first. If this fails, the
            // terminal process row remains and the prune leaks conservatively;
            // the final transaction below revalidates eligibility before it
            // removes any process row.
            for process_id in prunable {
                for session_id in lash_core::process_runtime_session_ids(&process_id) {
                    delete_session_from_catalog(root, &session_id, false)
                        .await
                        .map_err(lash_core::PluginError::Session)?;
                }
            }
        }
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome(
                    crate::process_registry_change::prune_terminal_processes_conn(
                        tx,
                        cutoff,
                        filter,
                        max_change_seq,
                    ),
                ))
            })
            .await
            .map_err(process_sqlite_error)?
    }
}

#[async_trait::async_trait]
impl ProcessContinuationStore for SqliteProcessRegistry {
    async fn put_segment_handover(
        &self,
        process_id: &str,
        handover: PersistedSegmentHandover,
    ) -> Result<(), lash_core::PluginError> {
        self.put_segment_handover_impl(process_id, handover).await
    }

    async fn get_segment_handover(
        &self,
        process_id: &str,
        segment_ordinal: u64,
    ) -> Result<Option<PersistedSegmentHandover>, lash_core::PluginError> {
        self.get_segment_handover_impl(process_id, segment_ordinal)
            .await
    }

    async fn latest_segment_handover(
        &self,
        process_id: &str,
    ) -> Result<Option<PersistedSegmentHandover>, lash_core::PluginError> {
        self.latest_segment_handover_impl(process_id).await
    }

    async fn delete_segment_handovers(
        &self,
        process_id: &str,
    ) -> Result<(), lash_core::PluginError> {
        self.delete_segment_handovers_impl(process_id).await
    }
}

/// Loud, stable error for a superseded or expired process lease.
pub(super) fn process_lease_expired(process_id: &str) -> lash_core::PluginError {
    lash_core::PluginError::ProcessLeaseSuperseded {
        process_id: process_id.to_string(),
    }
}

fn validate_process_execution_authority_conn(
    conn: &rusqlite::Connection,
    process_id: &str,
    record: &ProcessRecord,
    authority: &ProcessExecutionWriteAuthority,
    start: Option<&ProcessStarted>,
    now: u64,
) -> Result<(), lash_core::PluginError> {
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
        ProcessExecutionWriteAuthority::Lease(lease) => {
            if lease.process_id != process_id {
                return Err(process_lease_expired(process_id));
            }
            let current = SqliteProcessRegistry::load_process_lease_conn(conn, process_id)?;
            if guard_lease(current.as_ref(), &lease.lease_token, now)
                && current.as_ref().is_some_and(|current| {
                    current.owner.same_incarnation(&lease.owner)
                        && current.fencing_token == lease.fencing_token
                })
            {
                Ok(())
            } else {
                Err(process_lease_expired(process_id))
            }
        }
    }
}

fn process_lease_owner_from_columns(
    owner_id: String,
    incarnation_id: Option<String>,
) -> LeaseOwnerIdentity {
    LeaseOwnerIdentity {
        incarnation_id: incarnation_id.unwrap_or_else(|| owner_id.clone()),
        owner_id,
    }
}

fn process_external_ref_conflict(
    process_id: &str,
    existing: &ProcessExternalRef,
    new: &ProcessExternalRef,
) -> lash_core::PluginError {
    lash_core::PluginError::Session(format!(
        "process `{process_id}` external ref conflict: existing {existing:?}, new {new:?}"
    ))
}
