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

use support::process_status_label;
pub(crate) use support::tx_outcome;

#[async_trait::async_trait]
impl ProcessRegistry for SqliteProcessRegistry {
    async fn register_process(
        &self,
        registration: ProcessRegistration,
    ) -> Result<ProcessRecord, lash_core::PluginError> {
        let (registration, registration_hash) = prepare_process_registration(registration)?;
        let now = self.clock.timestamp_ms();
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
                            process_id, registration_hash, owner_scope_id,
                            created_at_ms, updated_at_ms, change_seq, status, record_json
                         )
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            record.id.as_str(),
                            record.registration_hash.as_str(),
                            originator_scope_id.as_str(),
                            record.created_at_ms as i64,
                            record.updated_at_ms as i64,
                            change_seq as i64,
                            process_status_label(&record),
                            process_encode_json(&record)?,
                        ],
                    )
                    .map_err(process_sqlite_error)?;
                    Ok(record)
                })()))
            })
            .await
            .map_err(process_sqlite_error)??;
        Ok(record)
    }

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

    async fn set_external_ref(
        &self,
        process_id: &str,
        external_ref: ProcessExternalRef,
    ) -> Result<ProcessRecord, lash_core::PluginError> {
        let process_id = process_id.to_string();
        let now = self.clock.timestamp_ms();
        let (record, _changed) = self
            .conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record =
                        Self::load_process_conn(tx, &process_id)?.ok_or_else(|| {
                            lash_core::PluginError::Session(format!(
                                "unknown process `{process_id}`"
                            ))
                        })?;
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
                    Self::append_event_conn(tx, &mut record, request, now)?;
                    Ok((record, true))
                })()))
            })
            .await
            .map_err(process_sqlite_error)??;
        Ok(record)
    }

    async fn grant_handle(
        &self,
        session_scope: &SessionScope,
        process_id: &str,
        descriptor: ProcessHandleDescriptor,
    ) -> Result<ProcessHandleGrant, lash_core::PluginError> {
        let session_scope = session_scope.clone();
        let process_id = process_id.to_string();
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let session_scope_id = session_scope.id();
                    if Self::load_process_conn(tx, &process_id)?.is_none() {
                        return Err(lash_core::PluginError::Session(format!(
                            "unknown process `{process_id}`"
                        )));
                    }
                    tx.execute(
                        "INSERT INTO process_handle_grants (session_id, scope_id, process_id, descriptor_json)
                         VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(scope_id, process_id) DO UPDATE SET
                            session_id = excluded.session_id,
                            descriptor_json = excluded.descriptor_json",
                        params![
                            session_scope.session_id.as_str(),
                            session_scope_id.as_str(),
                            process_id.as_str(),
                            process_encode_json(&descriptor)?
                        ],
                    )
                    .map_err(process_sqlite_error)?;
                    Ok(ProcessHandleGrant {
                        session_id: session_scope.session_id.clone(),
                        process_id: process_id.clone(),
                        descriptor,
                    })
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn revoke_handle(
        &self,
        session_scope: &SessionScope,
        process_id: &str,
    ) -> Result<(), lash_core::PluginError> {
        let session_scope_id = session_scope.id().as_str().to_string();
        let process_id = process_id.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "DELETE FROM process_handle_grants WHERE scope_id = ?1 AND process_id = ?2",
                    params![session_scope_id, process_id],
                )
            })
            .await
            .map_err(process_sqlite_error)?;
        Ok(())
    }

    async fn transfer_handle_grants(
        &self,
        from_scope: &SessionScope,
        to_scope: &SessionScope,
        process_ids: &[String],
    ) -> Result<(), lash_core::PluginError> {
        let from_scope = from_scope.clone();
        let to_scope = to_scope.clone();
        let process_ids = process_ids.to_vec();
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let from_scope_id = from_scope.id();
                    let to_scope_id = to_scope.id();
                    for process_id in &process_ids {
                        let descriptor_json: Option<String> = tx
                            .query_row(
                                "SELECT descriptor_json
                                 FROM process_handle_grants
                                 WHERE scope_id = ?1 AND process_id = ?2",
                                params![from_scope_id.as_str(), process_id.as_str()],
                                |row| row.get(0),
                            )
                            .optional()
                            .map_err(process_sqlite_error)?;
                        let Some(descriptor_json) = descriptor_json else {
                            return Err(lash_core::PluginError::Session(format!(
                                "process handle `{process_id}` is not granted to session `{}`",
                                from_scope.session_id
                            )));
                        };
                        tx.execute(
                            "DELETE FROM process_handle_grants
                             WHERE scope_id = ?1 AND process_id = ?2",
                            params![from_scope_id.as_str(), process_id.as_str()],
                        )
                        .map_err(process_sqlite_error)?;
                        tx.execute(
                            "INSERT INTO process_handle_grants (session_id, scope_id, process_id, descriptor_json)
                             VALUES (?1, ?2, ?3, ?4)
                             ON CONFLICT(scope_id, process_id) DO UPDATE SET
                                session_id = excluded.session_id,
                                descriptor_json = excluded.descriptor_json",
                            params![
                                to_scope.session_id.as_str(),
                                to_scope_id.as_str(),
                                process_id.as_str(),
                                descriptor_json
                            ],
                        )
                        .map_err(process_sqlite_error)?;
                    }
                    Ok(())
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn list_handle_grants(
        &self,
        session_scope: &SessionScope,
    ) -> Result<Vec<ProcessHandleGrantEntry>, lash_core::PluginError> {
        let session_scope = session_scope.clone();
        self.conn
            .call(move |conn| {
                Ok(Self::list_grants_for_scope_conn(
                    conn,
                    &session_scope,
                    false,
                ))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn list_live_handle_grants(
        &self,
        session_scope: &SessionScope,
    ) -> Result<Vec<ProcessHandleGrantEntry>, lash_core::PluginError> {
        let session_scope = session_scope.clone();
        self.conn
            .call(move |conn| Ok(Self::list_grants_for_scope_conn(conn, &session_scope, true)))
            .await
            .map_err(process_sqlite_error)?
    }

    async fn has_handle_grant(
        &self,
        session_scope: &SessionScope,
        process_id: &str,
    ) -> Result<bool, lash_core::PluginError> {
        let session_scope_id = session_scope.id().as_str().to_string();
        let process_id = process_id.to_string();
        self.conn
            .call(move |conn| {
                let exists = conn
                    .query_row(
                        "SELECT 1
                         FROM process_handle_grants g
                         JOIN processes p ON p.process_id = g.process_id
                         WHERE g.scope_id = ?1 AND g.process_id = ?2
                         LIMIT 1",
                        params![session_scope_id, process_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                Ok(exists)
            })
            .await
            .map_err(process_sqlite_error)
    }

    async fn handle_grants_for_process(
        &self,
        process_id: &str,
    ) -> Result<Vec<ProcessHandleGrant>, lash_core::PluginError> {
        let process_id = process_id.to_string();
        self.conn
            .call(move |conn| {
                Ok((|| {
                    if Self::load_process_conn(conn, &process_id)?.is_none() {
                        return Err(lash_core::PluginError::Session(format!(
                            "unknown process `{process_id}`"
                        )));
                    }
                    let mut stmt = conn
                        .prepare(
                            "SELECT session_id, descriptor_json
                             FROM process_handle_grants
                             WHERE process_id = ?1
                             ORDER BY session_id ASC, scope_id ASC",
                        )
                        .map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map(params![process_id], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })
                        .map_err(process_sqlite_error)?;
                    let mut grants = Vec::new();
                    for row in rows {
                        let (session_id, descriptor_json) = row.map_err(process_sqlite_error)?;
                        let descriptor: ProcessHandleDescriptor =
                            serde_json::from_str(&descriptor_json).map_err(process_decode_error)?;
                        grants.push(ProcessHandleGrant {
                            session_id,
                            process_id: process_id.clone(),
                            descriptor,
                        });
                    }
                    Ok(grants)
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn delete_session_process_state(
        &self,
        session_id: &str,
    ) -> Result<lash_core::ProcessSessionDeleteReport, lash_core::PluginError> {
        let session_id_owned = session_id.to_string();
        let (
            revoked_handle_count,
            deleted_wake_count,
            mut orphaned_process_ids,
            mut preserved_process_ids,
        ) = self
            .conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let session_id = session_id_owned;
                    let removed = {
                        let mut stmt = tx
                            .prepare(
                                "SELECT g.process_id, p.record_json
                                 FROM process_handle_grants g
                                 JOIN processes p ON p.process_id = g.process_id
                                 WHERE g.session_id = ?1
                                 ORDER BY g.process_id ASC",
                            )
                            .map_err(process_sqlite_error)?;
                        let rows = stmt
                            .query_map(params![session_id], |row| {
                                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                            })
                            .map_err(process_sqlite_error)?;
                        let mut removed = Vec::new();
                        for row in rows {
                            let (process_id, record_json) = row.map_err(process_sqlite_error)?;
                            let record: ProcessRecord =
                                serde_json::from_str(&record_json).map_err(process_decode_error)?;
                            removed.push((process_id, record));
                        }
                        removed
                    };

                    // Wake acknowledgements are process-scoped consumed-event markers.
                    // Session deletion removes materialized session-addressed deliveries
                    // through the session store; clearing these rows would re-expose
                    // already-consumed wakes to surviving grants or future host readers.
                    let deleted_wake_count = 0;
                    let revoked_handle_count = tx
                        .execute(
                            "DELETE FROM process_handle_grants WHERE session_id = ?1",
                            params![session_id],
                        )
                        .map_err(process_sqlite_error)?;
                    let mut orphaned_process_ids = Vec::new();
                    let mut preserved_process_ids = Vec::new();
                    for (process_id, record) in removed {
                        if record.is_terminal() {
                            continue;
                        }
                        let remaining_grants: i64 = tx
                            .query_row(
                                "SELECT COUNT(*) FROM process_handle_grants WHERE process_id = ?1",
                                params![process_id],
                                |row| row.get(0),
                            )
                            .map_err(process_sqlite_error)?;
                        if remaining_grants == 0 {
                            orphaned_process_ids.push(process_id);
                        } else {
                            preserved_process_ids.push(process_id);
                        }
                    }
                    let wake_targeted = {
                        let mut stmt = tx
                            .prepare("SELECT process_id, record_json FROM processes ORDER BY process_id ASC")
                            .map_err(process_sqlite_error)?;
                        let rows = stmt
                            .query_map([], |row| {
                                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                            })
                            .map_err(process_sqlite_error)?;
                        let mut records = Vec::new();
                        for row in rows {
                            let (process_id, record_json) = row.map_err(process_sqlite_error)?;
                            let record: ProcessRecord =
                                serde_json::from_str(&record_json).map_err(process_decode_error)?;
                            records.push((process_id, record));
                        }
                        records
                    };
                    for (_process_id, mut record) in wake_targeted {
                        if record.clear_wake_target_for_session(&session_id) {
                            Self::save_process_conn(tx, &record)?;
                        }
                    }
                    Ok((
                        revoked_handle_count,
                        deleted_wake_count,
                        orphaned_process_ids,
                        preserved_process_ids,
                    ))
                })()))
            })
            .await
            .map_err(process_sqlite_error)??;
        orphaned_process_ids.sort();
        orphaned_process_ids.dedup();
        preserved_process_ids.sort();
        preserved_process_ids.dedup();
        Ok(lash_core::ProcessSessionDeleteReport {
            session_id: session_id.to_string(),
            revoked_handle_count,
            deleted_wake_count,
            orphaned_process_ids,
            preserved_process_ids,
        })
    }

    async fn append_event(
        &self,
        process_id: &str,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendResult, lash_core::PluginError> {
        let process_id = process_id.to_string();
        let occurred_at_ms = self.clock.timestamp_ms();
        let (result, _appended) = self
            .conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record =
                        Self::load_process_conn(tx, &process_id)?.ok_or_else(|| {
                            lash_core::PluginError::Session(format!(
                                "unknown process `{process_id}`"
                            ))
                        })?;
                    Self::append_event_conn(tx, &mut record, request, occurred_at_ms)
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
        let (result, _appended) = self
            .conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record =
                        Self::load_process_conn(tx, &process_id)?.ok_or_else(|| {
                            lash_core::PluginError::Session(format!(
                                "unknown process `{process_id}`"
                            ))
                        })?;
                    validate_process_execution_authority_conn(
                        tx,
                        &process_id,
                        &record,
                        &authority,
                        None,
                        occurred_at_ms,
                    )?;
                    Self::append_event_conn(tx, &mut record, request, occurred_at_ms)
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
                    if Self::load_process_conn(conn, &process_id)?.is_none() {
                        return Err(lash_core::PluginError::Session(format!(
                            "unknown process `{process_id}`"
                        )));
                    }
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
                    if Self::load_process_conn(conn, &process_id)?.is_none() {
                        return Err(lash_core::PluginError::Session(format!(
                            "unknown process `{process_id}`"
                        )));
                    }
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
                    if Self::load_process_conn(conn, &process_id)?.is_none() {
                        return Err(lash_core::PluginError::Session(format!(
                            "unknown process `{process_id}`"
                        )));
                    }
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

    async fn wake_events_after(
        &self,
        process_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<ProcessEvent>, lash_core::PluginError> {
        let acked: std::collections::HashSet<u64> = {
            let process_id = process_id.to_string();
            self.conn
                .call(move |conn| {
                    Ok(
                        (|| -> Result<std::collections::HashSet<u64>, lash_core::PluginError> {
                            let mut stmt = conn
                                .prepare(
                                    "SELECT sequence FROM process_wake_acks WHERE process_id = ?1",
                                )
                                .map_err(process_sqlite_error)?;
                            let rows = stmt
                                .query_map(params![process_id], |row| row.get::<_, i64>(0))
                                .map_err(process_sqlite_error)?;
                            let mut set = std::collections::HashSet::new();
                            for row in rows {
                                set.insert(row.map_err(process_sqlite_error)? as u64);
                            }
                            Ok(set)
                        })(),
                    )
                })
                .await
                .map_err(process_sqlite_error)??
        };
        Ok(self
            .events_after(process_id, after_sequence)
            .await?
            .into_iter()
            .filter(|event| event.semantics.wake.is_some() && !acked.contains(&event.sequence))
            .collect())
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
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record =
                        Self::load_process_conn(tx, &process_id)?.ok_or_else(|| {
                            lash_core::PluginError::Session(format!(
                                "unknown process `{process_id}`"
                            ))
                        })?;
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
                    Self::append_event_conn(tx, &mut record, request, now)?;
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
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record =
                        Self::load_process_conn(tx, &process_id)?.ok_or_else(|| {
                            lash_core::PluginError::Session(format!(
                                "unknown process `{process_id}`"
                            ))
                        })?;
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
                    Self::append_event_conn(tx, &mut record, append, now)?;
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
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record =
                        Self::load_process_conn(tx, &process_id)?.ok_or_else(|| {
                            lash_core::PluginError::Session(format!(
                                "unknown process `{process_id}`"
                            ))
                        })?;
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
                    Self::append_event_conn(tx, &mut record, request, now)?;
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
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record =
                        Self::load_process_conn(tx, &process_id)?.ok_or_else(|| {
                            lash_core::PluginError::Session(format!(
                                "unknown process `{process_id}`"
                            ))
                        })?;
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
                    Self::append_event_conn(tx, &mut record, request, now)?;
                    Ok(record)
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn get_process(&self, process_id: &str) -> Option<ProcessRecord> {
        let process_id = process_id.to_string();
        self.conn
            .call(move |conn| Ok(Self::load_process_conn(conn, &process_id).ok().flatten()))
            .await
            .ok()
            .flatten()
    }

    async fn try_get_process(
        &self,
        process_id: &str,
    ) -> Result<Option<ProcessRecord>, lash_core::PluginError> {
        let process_id = process_id.to_string();
        self.conn
            .call(move |conn| Ok(Self::load_process_conn(conn, &process_id)))
            .await
            .map_err(process_sqlite_error)?
    }

    async fn list_processes(
        &self,
        filter: &lash_core::ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, lash_core::PluginError> {
        let filter = filter.clone();
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let mut stmt = conn
                        .prepare(
                            "SELECT record_json FROM processes
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
    ) -> Result<(Vec<ProcessRecord>, ProcessChangeCursor), lash_core::PluginError> {
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

    async fn ack_wake(
        &self,
        process_id: &str,
        sequence: u64,
    ) -> Result<(), lash_core::PluginError> {
        let process_id = process_id.to_string();
        self.conn
            .call(move |conn| {
                Ok((|| {
                    if Self::load_process_conn(conn, &process_id)?.is_none() {
                        return Err(lash_core::PluginError::Session(format!(
                            "unknown process `{process_id}`"
                        )));
                    }
                    conn.execute(
                        "INSERT OR IGNORE INTO process_wake_acks (process_id, sequence) VALUES (?1, ?2)",
                        params![process_id, sequence as i64],
                    )
                    .map_err(process_sqlite_error)?;
                    Ok(())
                })())
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
                    if Self::load_process_conn(tx, &process_id)?.is_none() {
                        return Err(lash_core::PluginError::Session(format!(
                            "unknown process `{process_id}`"
                        )));
                    }
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
        observed_holder: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLeaseClaimOutcome, lash_core::PluginError> {
        let process_id = process_id.to_string();
        let owner = owner.clone();
        let observed_holder = observed_holder.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    if Self::load_process_conn(tx, &process_id)?.is_none() {
                        return Err(lash_core::PluginError::Session(format!(
                            "unknown process `{process_id}`"
                        )));
                    }
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
                    // Fenced CAS on the observed holder: identity, token, and
                    // fencing token must all still match, and the holder must
                    // be definitely dead for this claimant.
                    if observed_holder.process_id == process_id
                        && current.owner.same_incarnation(&observed_holder.owner)
                        && current.lease_token == observed_holder.lease_token
                        && current.fencing_token == observed_holder.fencing_token
                        && current.owner.is_definitely_dead_for_claimant(&owner)
                    {
                        let fencing_token = current.fencing_token.saturating_add(1);
                        let lease = ProcessLease {
                            schema_version: PROCESS_LEASE_SCHEMA_VERSION,
                            process_id: process_id.clone(),
                            owner: owner.clone(),
                            lease_token: format!(
                                "{:x}",
                                Sha256::digest(
                                    format!(
                                        "{process_id}:{}:{}:{now}:{fencing_token}",
                                        owner.owner_id, owner.incarnation_id
                                    )
                                    .as_bytes()
                                )
                            ),
                            fencing_token,
                            claimed_at_epoch_ms: now,
                            expires_at_epoch_ms: now.saturating_add(lease_ttl_ms),
                        };
                        let changed = tx
                            .execute(
                                "UPDATE process_leases
                                 SET lease_owner_id = ?1,
                                     lease_owner_incarnation_id = ?2,
                                     lease_owner_liveness_json = ?3,
                                     lease_token = ?4,
                                     lease_fencing_token = ?5,
                                     lease_claimed_at_ms = ?6,
                                     lease_expires_at_ms = ?7
                                 WHERE process_id = ?8
                                   AND lease_owner_id = ?9
                                   AND lease_owner_incarnation_id = ?10
                                   AND lease_token = ?11
                                   AND lease_fencing_token = ?12",
                                params![
                                    lease.owner.owner_id,
                                    lease.owner.incarnation_id,
                                    encode_process_lease_liveness(&lease.owner.liveness)?,
                                    lease.lease_token,
                                    lease.fencing_token as i64,
                                    lease.claimed_at_epoch_ms as i64,
                                    lease.expires_at_epoch_ms as i64,
                                    process_id,
                                    observed_holder.owner.owner_id,
                                    observed_holder.owner.incarnation_id,
                                    observed_holder.lease_token,
                                    observed_holder.fencing_token as i64,
                                ],
                            )
                            .map_err(process_sqlite_error)?;
                        if changed == 1 {
                            return Ok(ProcessLeaseClaimOutcome::Acquired(lease));
                        }
                        // Lost the CAS race: re-read and report the winner.
                        if let Some(current) = Self::load_process_lease_conn(tx, &process_id)?
                            && current.expires_at_epoch_ms > now
                        {
                            return Ok(ProcessLeaseClaimOutcome::Busy { holder: current });
                        }
                        return Err(process_lease_expired(&process_id));
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
        up_to_change_seq: Option<ProcessChangeCursor>,
    ) -> Result<ProcessPruneReport, lash_core::PluginError> {
        let cutoff = cutoff_epoch_ms as i64;
        let max_change_seq = up_to_change_seq.map(ProcessChangeCursor::store_sequence);
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
    liveness_json: Option<String>,
) -> LeaseOwnerIdentity {
    LeaseOwnerIdentity {
        incarnation_id: incarnation_id.unwrap_or_else(|| owner_id.clone()),
        owner_id,
        liveness: liveness_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or(LeaseOwnerLiveness::Opaque),
    }
}

fn encode_process_lease_liveness(
    liveness: &LeaseOwnerLiveness,
) -> Result<String, lash_core::PluginError> {
    serde_json::to_string(liveness).map_err(|err| {
        lash_core::PluginError::Session(format!("failed to encode process lease liveness: {err}"))
    })
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
