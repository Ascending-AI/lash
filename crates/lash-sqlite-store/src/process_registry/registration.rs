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
        let mut observers = observers.to_vec();
        observers.sort();
        observers.dedup();
        let wake_session_id = registration.wake_session_id.clone();
        let consumer_hold = registration.consumer_hold.clone();
        let trigger_delivery_pin = registration.trigger_delivery_pin.clone();
        let wake_delivery_config = self.wake_delivery_config;
        self.conn
            .write_flow(move |tx| {
                let fleet_format = tx.fleet();
                Ok(tx_outcome((|| {
                    // While the process minted for a key is retained, a
                    // start under the same key returns that process untouched
                    // (ADR 0107); a host's key must also present its start,
                    // wake target included.
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
                        lash_core_execution::runtime::check_retained_start(
                            &registration,
                            &existing,
                            Self::wake_session_id_conn(tx, &existing.id)?.as_ref(),
                        )?;
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
                    // A delivery's key finds its process only while that
                    // process is retained. A delivery already bound, or gone,
                    // had its process pruned, and starts nothing: its row is
                    // read in this transaction, after the key found nothing,
                    // so a bind and prune that landed since this start's
                    // ingest are seen here (ADR 0107 §5, FIG-4369).
                    if let Some(pin) = trigger_delivery_pin.as_ref() {
                        super::delivery_binding::check_start_conn(tx, pin)?;
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
                    // Minted only once the start is admitted, so no refusal
                    // names an id that was never registered.
                    let change_seq = Self::next_change_seq_conn(tx)?;
                    let record =
                        ProcessRecord::from_prepared_registration(registration, process_id, now);
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
                            wake_session_id.as_deref(),
                            record.identity.kind.as_str(),
                            record.identity.label.as_deref(),
                            record.created_at_ms as i64,
                            record.updated_at_ms as i64,
                            record.last_event_sequence as i64,
                            change_seq as i64,
                            process_status_label(&record),
                            record.lifetime.scope().map(ScopeId::storage_kind),
                            record.lifetime.scope().map(ScopeId::storage_id),
                            record.lifetime.storage_label(),
                            cancel_requested_at_ms(&record),
                            process_encode_json(&record)?,
                            consumer_hold.as_ref().map(|hold| hold.key.as_str()),
                            consumer_hold.as_ref().map(|hold| hold.owner.storage_kind()),
                            consumer_hold.as_ref().map(|hold| hold.owner.storage_id()),
                            consumer_hold.as_ref().map(|hold| hold.cancels),
                            trigger_delivery_pin
                                .as_ref()
                                .map(|pin| pin.occurrence_id.as_str()),
                            trigger_delivery_pin
                                .as_ref()
                                .map(|pin| pin.subscription_id.as_str()),
                        ],
                    )
                    .map_err(process_sqlite_error)?;
                    crate::obligation_ledger::arm_obligation_tx(
                        tx,
                        &lash_core_execution::store::ObligationKey::ProcessStart {
                            process_id: record.id.clone(),
                        },
                        crate::obligation_ledger::DUE_AT_ONCE_MS,
                    )
                    .map_err(lash_core_execution::PluginError::from)?;
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
                            wake_delivery_config,
                            fleet_format,
                        )?;
                    }
                    Ok(lash_core_execution::ProcessRegistrationReceipt::created(
                        record,
                    ))
                })()))
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
        let wake_delivery_config = self.wake_delivery_config;
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
                            Self::append_event_conn(
                                tx,
                                &mut record,
                                *request,
                                now,
                                wake_delivery_config,
                                fleet_format,
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
}
