use super::*;
use lash_core::facade_support;
#[path = "process_registry/continuation_store.rs"]
mod continuation_store;
mod leases;
#[path = "process_registry/parent_end.rs"]
mod parent_end;
mod segment_handover;
mod support;
#[path = "process_registry/tool_intent_submission.rs"]
mod tool_intent_submission;
mod wake_delivery;
mod worklist;

pub(crate) use support::{ProcessEventAppendArm, ProcessEventWriteAuthorization, tx_outcome};
use support::{process_status_label, wake_allocation_floor_for_testing};
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
        let registration = prepare_process_registration(registration)?;
        let mut observers = observers.to_vec();
        observers.sort();
        observers.dedup();
        let registration_fingerprint =
            lash_core::runtime::process_registration_fingerprint(&registration, &observers);
        let wake_session_id = registration.wake_session_id.clone();
        let now = self.clock.timestamp_ms();
        let wake_delivery_config = self.wake_delivery_config;
        let record = self
            .conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    if let Some(existing) = Self::load_process_conn(tx, &registration.id)? {
                        if existing.registration_fingerprint == registration_fingerprint {
                            return Ok(existing);
                        }
                        return Err(lash_core::PluginError::Session(format!(
                            "process `{}` registration fingerprint conflict: existing {}, new {}",
                            registration.id,
                            existing.registration_fingerprint,
                            registration_fingerprint
                        )));
                    }
                    let record = ProcessRecord::from_prepared_registration(
                        registration,
                        registration_fingerprint,
                        now,
                    );
                    let originator_id = record.originator_id();
                    let change_seq = Self::next_change_seq_conn(tx)?;
                    tx.execute(
                        "INSERT INTO processes (
                            process_id, registration_fingerprint, originator_id, wake_session_id,
                            identity_kind, identity_label, is_waiting,
                            created_at_ms, updated_at_ms, change_seq, status, record_json
                         )
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                        params![
                            record.id.as_str(),
                            record.registration_fingerprint.as_str(),
                            originator_id.as_str(),
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
        let record = self
            .conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_conn(tx, &process_id)?;
                    match lash_core::runtime::prepare_process_transition(
                        &record,
                        ProcessTransition::SetExternalRef(external_ref),
                    )? {
                        ProcessTransitionPlan::Unchanged => return Ok(record),
                        ProcessTransitionPlan::Append(request) => {
                            Self::append_event_conn(
                                tx,
                                &mut record,
                                request,
                                now,
                                wake_delivery_config,
                            )?;
                        }
                    }
                    Ok(record)
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

    async fn is_observer(
        &self,
        session_id: &str,
        process_id: &str,
    ) -> Result<bool, lash_core::PluginError> {
        let session_id = session_id.to_string();
        let process_id = process_id.to_string();
        let queried_process_id = process_id.clone();
        let (retained, observer) = self
            .conn
            .call(move |conn| {
                let retained = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM processes WHERE process_id = ?1)",
                    params![queried_process_id],
                    |row| row.get::<_, bool>(0),
                )?;
                let observer = conn.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM process_observers
                         WHERE session_id = ?1 AND process_id = ?2
                     )",
                    params![session_id, queried_process_id],
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
                        tx.execute(
                            "DELETE FROM wake_allocation_floors
                             WHERE target_session_id = ?1",
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

    async fn wake_allocation_floor_for_testing(
        &self,
        target_session_id: &str,
        process_id: &str,
    ) -> Result<Option<u64>, lash_core::PluginError> {
        wake_allocation_floor_for_testing(self, target_session_id, process_id).await
    }

    async fn append_event(
        &self,
        process_id: &str,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendReceipt, lash_core::PluginError> {
        facade_support::validate_generic_process_event_append(&request)?;
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
    ) -> Result<ProcessEventAppendReceipt, lash_core::PluginError> {
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
        support::recent_events(self, process_id, limit).await
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
            Vec::new(),
        )
        .await
    }

    async fn complete_process_with_parent_end(
        &self,
        process_id: &str,
        await_output: ProcessAwaitOutput,
        authority: lash_core::ProcessCompletionAuthority,
        actions: Vec<lash_core::ToolIntentParentEndAction>,
    ) -> Result<lash_core::ProcessCompletionOutcome, lash_core::PluginError> {
        super::process_registry_completion::complete_process(
            self,
            process_id,
            await_output,
            authority,
            actions,
        )
        .await
    }

    async fn complete_process_with_lease(
        &self,
        lease: &ProcessLease,
        await_output: ProcessAwaitOutput,
    ) -> Result<lash_core::ProcessCompletionOutcome, lash_core::PluginError> {
        super::process_registry_completion::complete_process_with_lease(
            self,
            lease,
            await_output,
            Vec::new(),
        )
        .await
    }

    async fn complete_process_with_lease_and_parent_end(
        &self,
        lease: &ProcessLease,
        await_output: ProcessAwaitOutput,
        actions: Vec<lash_core::ToolIntentParentEndAction>,
    ) -> Result<lash_core::ProcessCompletionOutcome, lash_core::PluginError> {
        super::process_registry_completion::complete_process_with_lease(
            self,
            lease,
            await_output,
            actions,
        )
        .await
    }

    async fn list_pending_parent_end_plans(
        &self,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<lash_core::ProcessParentEndPlan>, lash_core::PluginError> {
        parent_end::list(self, limit).await
    }

    async fn get_pending_parent_end_plan(
        &self,
        process_id: &str,
    ) -> Result<Option<lash_core::ProcessParentEndPlan>, lash_core::PluginError> {
        parent_end::get(self, process_id).await
    }

    async fn complete_parent_end_plan(
        &self,
        process_id: &str,
    ) -> Result<(), lash_core::PluginError> {
        parent_end::complete(self, process_id).await
    }

    async fn admit_tool_intent_submission(
        &self,
        submission: lash_core::ToolIntentSubmissionRecord,
    ) -> Result<lash_core::ToolIntentSubmissionAdmission, lash_core::PluginError> {
        tool_intent_submission::admit(self, submission).await
    }

    async fn complete_tool_intent_submission(
        &self,
        replay_key: &str,
        outcome: lash_core::ToolIntentExecutionOutcome,
    ) -> Result<lash_core::ToolIntentSubmissionRecord, lash_core::PluginError> {
        tool_intent_submission::complete(self, replay_key, outcome).await
    }

    async fn pending_tool_intent_parent_end(
        &self,
        session_id: &str,
        execution_scope_id: &str,
    ) -> Result<Vec<lash_core::ToolIntentSubmissionRecord>, lash_core::PluginError> {
        tool_intent_submission::pending_parent_end(self, session_id, execution_scope_id).await
    }

    async fn complete_tool_intent_parent_end(
        &self,
        replay_key: &str,
    ) -> Result<(), lash_core::PluginError> {
        tool_intent_submission::complete_parent_end(self, replay_key).await
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
                    match lash_core::runtime::prepare_process_transition(
                        &record,
                        ProcessTransition::RequestAbandon(request),
                    )? {
                        ProcessTransitionPlan::Unchanged => return Ok(record),
                        ProcessTransitionPlan::Append(append) => {
                            Self::append_event_conn(
                                tx,
                                &mut record,
                                append,
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
        process_id: &str,
    ) -> Result<ProcessRecord, lash_core::PluginError> {
        let process_id = process_id.to_string();
        let now = self.clock.timestamp_ms();
        let wake_delivery_config = self.wake_delivery_config;
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_conn(tx, &process_id)?;
                    match lash_core::runtime::prepare_process_transition(
                        &record,
                        ProcessTransition::RecordCallerDeparture,
                    )? {
                        ProcessTransitionPlan::Unchanged => return Ok(record),
                        ProcessTransitionPlan::Append(append) => {
                            Self::append_event_conn(
                                tx,
                                &mut record,
                                append,
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
                    match lash_core::runtime::prepare_process_transition(
                        &record,
                        ProcessTransition::EnterWait(wait),
                    )? {
                        ProcessTransitionPlan::Unchanged => return Ok(record),
                        ProcessTransitionPlan::Append(request) => {
                            Self::append_event_conn(
                                tx,
                                &mut record,
                                request,
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
                    match lash_core::runtime::prepare_process_transition(
                        &record,
                        ProcessTransition::ClearWait,
                    )? {
                        ProcessTransitionPlan::Unchanged => return Ok(record),
                        ProcessTransitionPlan::Append(request) => {
                            Self::append_event_conn(
                                tx,
                                &mut record,
                                request,
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

    async fn list_processes(
        &self,
        filter: &lash_core::ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, lash_core::PluginError> {
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
        let status = filter.status.label().map(str::to_string);
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let mut stmt = conn
                        .prepare(
                            "SELECT record_json FROM processes
                             WHERE (?1 IS NULL OR status = ?1)
                               AND (?2 IS NULL OR is_waiting = ?2)
                               AND (?3 IS NULL OR originator_id = ?3)
                               AND (?4 IS NULL OR identity_kind = ?4)
                               AND (?5 IS NULL OR identity_label = ?5)
                               AND (?6 IS NULL OR
                                    (json_type(record_json, '$.identity.definition') IS NOT NULL
                                     AND json_type(
                                             record_json, '$.identity.definition'
                                         ) = json_type(?6, '$')
                                     AND (
                                         json_type(?6, '$') IN ('null', 'true', 'false')
                                         OR json_quote(json_extract(
                                             record_json, '$.identity.definition'
                                         )) IS json(?6)
                                     )))
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
                                filter.originator_id,
                                filter.identity_kind,
                                filter.identity_label,
                                definition,
                                filter.caused_by_occurrence_id,
                                filter.caused_by_subscription_id,
                                filter.created_at_start_ms.map(crate::clamp_epoch_ms),
                                filter.created_at_end_ms.map(crate::clamp_epoch_ms),
                            ],
                            |row| row.get::<_, String>(0),
                        )
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
        watermark: lash_core::ProjectionWatermark,
        trigger_store: Option<&dyn lash_core::TriggerStore>,
    ) -> Result<usize, lash_core::PluginError> {
        let max_change_seq = crate::process_registry_change::max_change_sequence(watermark);
        let cutoff_epoch_ms = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        let outstanding_trigger_delivery_process_ids = match trigger_store {
            Some(trigger_store) => trigger_store.list_delivery_process_ids().await?,
            None => Vec::new(),
        };
        self.conn
            .call(move |conn| {
                Ok(
                    crate::process_registry_change::compact_process_tombstones_conn(
                        conn,
                        cutoff_epoch_ms,
                        max_change_seq,
                        &outstanding_trigger_delivery_process_ids,
                    ),
                )
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn claim_pending_wake_deliveries(
        &self,
        limit: usize,
    ) -> Result<Vec<lash_core::WakeDelivery>, lash_core::PluginError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let now = self.clock.timestamp_ms();
        let enqueuing_stale_after_ms = self.wake_delivery_config.enqueuing_stale_after_ms;
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    tx.execute(
                        "UPDATE process_wake_deliveries
                         SET state = 'pending', claim_token = NULL
                         WHERE state = 'enqueuing' AND next_attempt_at_ms <= ?1",
                        params![now as i64],
                    )
                    .map_err(process_sqlite_error)?;
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
                                         AND NOT (earlier.state = 'discarded' AND earlier.discard_reason = 'sequence_rewound')
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
                        let claim_token = uuid::Uuid::new_v4().to_string();
                        tx.execute(
                            "UPDATE process_wake_deliveries
                             SET state = 'enqueuing',
                                 claim_token = ?4,
                                 attempts = attempts + 1,
                                 first_attempt_ms = COALESCE(first_attempt_ms, ?2),
                                 next_attempt_at_ms = ?3
                             WHERE delivery_id = ?1 AND state = 'pending'",
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

    async fn mark_wake_enqueued(
        &self,
        delivery_id: &str,
        claim_token: &str,
    ) -> Result<lash_core::WakeDeliveryClaimOutcome, lash_core::PluginError> {
        update_wake_delivery_state(
            &self.conn,
            delivery_id,
            claim_token,
            lash_core::WakeDeliveryState::Enqueued,
            None,
        )
        .await
    }

    async fn discard_wake_delivery(
        &self,
        delivery_id: &str,
        claim_token: &str,
        reason: lash_core::WakeDiscardReason,
    ) -> Result<lash_core::WakeDeliveryClaimOutcome, lash_core::PluginError> {
        update_wake_delivery_state(
            &self.conn,
            delivery_id,
            claim_token,
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
                                 claim_token = NULL, next_attempt_at_ms = ?3, expires_at_ms = ?2,
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
        claim_token: &str,
        next_attempt_at_ms: u64,
    ) -> Result<lash_core::WakeDeliveryClaimOutcome, lash_core::PluginError> {
        let delivery_id = delivery_id.to_string();
        let claim_token = claim_token.to_string();
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    let changed = tx
                        .execute(
                            "UPDATE process_wake_deliveries
                             SET state = 'pending', claim_token = NULL, next_attempt_at_ms = ?3
                             WHERE delivery_id = ?1 AND state = 'enqueuing' AND claim_token = ?2",
                            params![delivery_id, claim_token, next_attempt_at_ms as i64],
                        )
                        .map_err(process_sqlite_error)?;
                    if changed == 0 {
                        let delivery = load_wake_delivery_conn(tx, &delivery_id)?;
                        return Ok(lash_core::WakeDeliveryClaimOutcome::ClaimLost {
                            state: delivery.state,
                        });
                    }
                    Ok(lash_core::WakeDeliveryClaimOutcome::Applied)
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn list_non_terminal_page(
        &self,
        limit: std::num::NonZeroUsize,
        continuation: Option<lash_core::ProcessWorklistCursor>,
    ) -> Result<lash_core::ProcessWorklistPage, lash_core::PluginError> {
        worklist::list_non_terminal_page(self, limit, continuation).await
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
                               AND NOT EXISTS (
                                 SELECT 1 FROM process_tombstones t
                                 WHERE t.process_id = candidate.value
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

    async fn filter_tombstoned_process_ids(
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
                             WHERE EXISTS (
                                 SELECT 1 FROM process_tombstones t
                                 WHERE t.process_id = candidate.value
                             )
                               AND NOT EXISTS (
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
    ) -> Result<Vec<ProcessLiveReferenceView>, lash_core::PluginError> {
        let records = worklist::collect_non_terminal_records(self).await?;
        Ok(ProcessLiveReferenceView::from_records(records.iter()))
    }

    async fn count_non_terminal_processes(&self) -> Result<usize, lash_core::PluginError> {
        worklist::count_non_terminal_processes(self).await
    }

    async fn claim_process_lease(
        &self,
        process_id: &str,
        owner: &LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLeaseClaimOutcome, lash_core::PluginError> {
        leases::claim_process_lease(self, process_id, owner, lease_ttl_ms).await
    }

    async fn reclaim_process_lease(
        &self,
        process_id: &str,
        owner: &LeaseOwnerIdentity,
        observed_holder: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLeaseClaimOutcome, lash_core::PluginError> {
        leases::reclaim_process_lease(self, process_id, owner, observed_holder, lease_ttl_ms).await
    }

    async fn renew_process_lease(
        &self,
        lease: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLease, lash_core::PluginError> {
        leases::renew_process_lease(self, lease, lease_ttl_ms).await
    }

    async fn get_process_lease(
        &self,
        process_id: &str,
    ) -> Result<Option<ProcessLease>, lash_core::PluginError> {
        leases::get_process_lease(self, process_id).await
    }

    async fn complete_process_lease(
        &self,
        completion: &ProcessLeaseCompletion,
    ) -> Result<(), lash_core::PluginError> {
        leases::complete_process_lease(self, completion).await
    }

    async fn prune_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: lash_core::ProjectionWatermark,
    ) -> Result<ProcessPruneReport, lash_core::PluginError> {
        let cutoff = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        let pruned_at_ms = self.clock.timestamp_ms() as i64;
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
                for session_id in facade_support::process_runtime_session_ids(&process_id) {
                    delete_session_from_catalog(root, &session_id)
                        .await
                        .map_err(|error| lash_core::PluginError::Session(error.to_string()))?;
                }
            }
        }
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome(
                    crate::process_registry_change::prune_terminal_processes_conn(
                        tx,
                        cutoff,
                        pruned_at_ms,
                        filter,
                        max_change_seq,
                    ),
                ))
            })
            .await
            .map_err(process_sqlite_error)?
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
            // The process-id half of the fence is checked first so a lease for
            // another process is refused without reading this process's row.
            if lease.process_id != process_id {
                return Err(lash_core::PluginError::ProcessLeaseSuperseded {
                    process_id: process_id.to_string(),
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
