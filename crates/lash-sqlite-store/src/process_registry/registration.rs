use super::*;
use lash_core_execution::ScopeId;

#[async_trait::async_trait]
impl lash_core_execution::ProcessRegistrar for SqliteProcessRegistry {
    async fn prepare_process_registration(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<lash_core_execution::PreparedProcessRegistration, lash_core_execution::PluginError>
    {
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
    ) -> Result<lash_core_execution::ProcessRegistrationReceipt, lash_core_execution::PluginError>
    {
        let (registration, observers, process_id, retained, now) = prepared.into_commit(anchor);
        self.conn
            .write_flow(move |tx| {
                let fleet_format = tx.fleet();
                Ok(tx_outcome(Self::apply_registration_conn(
                    tx,
                    registration,
                    observers,
                    process_id,
                    retained,
                    now,
                    fleet_format,
                )))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn set_external_ref(
        &self,
        process_id: &ProcessId,
        external_ref: ProcessExternalRef,
    ) -> Result<ProcessRecord, lash_core_execution::PluginError> {
        let process_id = process_id.clone();
        let now = self.clock.timestamp_ms();
        let record = self
            .conn
            .write_flow(move |tx| {
                let fleet_format = tx.fleet();
                Ok(tx_outcome((|| {
                    let mut record = Self::require_process_conn(tx, &process_id)?;
                    match lash_core_execution::runtime::prepare_process_transition(
                        &record,
                        ProcessTransition::SetExternalRef(external_ref),
                    )? {
                        ProcessTransitionPlan::Unchanged => return Ok(record),
                        ProcessTransitionPlan::Append(request) => {
                            Self::append_event_conn(tx, &mut record, *request, now, fleet_format)?;
                        }
                    }
                    Ok(record)
                })()))
            })
            .await
            .map_err(process_sqlite_error)??;
        Ok(record)
    }
}

impl SqliteProcessRegistry {
    /// Apply one prepared registration on `tx`, inside the caller's
    /// transaction (ADR 0132 §12). The row, its
    /// observers and its actor, ready, are written together.
    pub(crate) fn apply_registration_conn(
        tx: &rusqlite::Connection,
        registration: ProcessRegistration,
        observers: Vec<SessionId>,
        process_id: ProcessId,
        retained: bool,
        now: u64,
        fleet_format: lash_core_execution::FleetFormat,
    ) -> Result<lash_core_execution::ProcessRegistrationReceipt, lash_core_execution::PluginError>
    {
        let mut observers = observers;
        observers.sort();
        observers.dedup();
        let consumer_hold = registration.consumer_hold.clone();
        // While the process minted for a key is retained, a
        // start under the same key returns that process untouched
        // (ADR 0107); a host's key must also present its start,
        // with equal content.
        if let Some(start_key) = registration.start_key.as_ref()
            && let Some(existing) = Self::load_process_by_start_key_conn(tx, start_key)?
        {
            if retained && existing.id != process_id {
                return Err(
                    lash_core_execution::StoreError::PreparedProcessRegistrationStale {
                        process_id: process_id.clone(),
                    }
                    .into(),
                );
            }
            lash_core_execution::runtime::check_retained_start(&registration, &existing)?;
            return Ok(lash_core_execution::ProcessRegistrationReceipt::existing(
                existing,
            ));
        }
        if retained {
            return Err(
                lash_core_execution::StoreError::PreparedProcessRegistrationStale {
                    process_id: process_id.clone(),
                }
                .into(),
            );
        }
        let registration = prepare_process_registration(registration)?;
        // Admission against closure (FIG-3607 R11): a new start is
        // refused once its starter has ended, whatever its own
        // lifetime, once the scope its lifetime names has closed,
        // and once the session either lies inside has closed
        // (FIG-3948). All are read in this transaction, so a start
        // racing a close either commits first and is swept, or
        // sees the row and is refused.
        for scope in registration.closing_scopes() {
            if super::parent_end::plan_exists_conn(tx, &scope)? {
                return Err(lash_core_execution::PluginError::ParentEnded {
                    start_key: registration.start_key.clone(),
                    parent: scope,
                });
            }
        }
        // A start whose consuming call was abandoned is refused:
        // the call's opener already drained what the hold owed
        // (ADR 0116 §3.4).
        if let Some(hold) = consumer_hold.as_ref()
            && tx
                .query_row(
                    process_sql().abandoned_hold.exists.sql(),
                    params![hold.key.as_str()],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(process_sqlite_error)?
        {
            return Err(lash_core_execution::runtime::abandoned_consumer_refusal(
                registration.start_key.as_ref(),
                &hold.key,
            ));
        }
        if let Some(env) = registration.env_ref.as_ref() {
            crate::artifact_store::acquire_process_env_tx(tx, env, &process_id)?;
        }
        // Minted only once the start is admitted, so no refusal
        // names an id that was never registered.
        let change_seq = Self::next_change_seq_conn(tx)?;
        let record = ProcessRecord::from_prepared_registration(registration, process_id, now);
        let originator_id = record.originator_id();
        crate::conn::cached_execute(
            tx,
            process_sql().process_sqlite.insert_registration.sql(),
            params![
                record.id.as_str(),
                record
                    .start_key
                    .as_ref()
                    .map(lash_core_execution::StartKey::as_str),
                originator_id.as_str(),
                record.identity.kind.as_str(),
                record.identity.label.as_deref(),
                record.created_at_ms as i64,
                record.updated_at_ms as i64,
                change_seq as i64,
                record.lifetime.scope().map(ScopeId::storage_kind),
                record.lifetime.scope().map(ScopeId::storage_id),
                record.lifetime.storage_label(),
                process_encode_json(&record)?,
                consumer_hold.as_ref().map(|hold| hold.key.as_str()),
                consumer_hold.as_ref().map(|hold| hold.owner.storage_kind()),
                consumer_hold.as_ref().map(|hold| hold.owner.storage_id()),
                consumer_hold.as_ref().map(|hold| hold.cancels),
            ],
        )
        .map_err(process_sqlite_error)?;
        // The process's actor commits with its row, ready: the
        // start is a wake of the actor, never a relayed
        // obligation (ADR 0132 §12).
        crate::durable::processes::create_actor_within(
            tx,
            &record.id,
            record.input.unstarted_formats().as_str(),
            lash_durable::DurableInstant(i64::try_from(now).unwrap_or(i64::MAX)),
        )
        .map_err(process_sqlite_error)?
        .map_err(lash_core_execution::runtime::actor::process::registry_error)?;
        let mut record = record;
        let process_id = record.id.clone();
        for session_id in &observers {
            crate::conn::cached_execute(
                tx,
                process_sql().observer.insert.sql(),
                params![session_id.as_str(), record.id.as_str()],
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
                fleet_format,
            )?;
        }
        Ok(lash_core_execution::ProcessRegistrationReceipt::created(
            record,
        ))
    }
}
