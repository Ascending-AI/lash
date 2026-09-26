use super::*;
use lash_core_execution::ScopeId;

#[async_trait::async_trait]
impl lash_core_execution::ProcessRegistrar for SqliteProcessRegistry {
    async fn register_process_reporting_disposition(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<lash_core_execution::ProcessRegistrationOutcome, lash_core_execution::PluginError>
    {
        let mut observers = observers.to_vec();
        observers.sort();
        observers.dedup();
        let wake_session_id = registration.wake_session_id.clone();
        let now = self.clock.timestamp_ms();
        let wake_delivery_config = self.wake_delivery_config;
        let process_id_mint = self.process_id_mint.clone();
        let fleet_format = self.fleet_format;
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    // While the process minted for a key is retained, a
                    // start under the same key returns that process untouched
                    // (ADR 0107); a host's key must also present its content.
                    if let Some(start_key) = registration.start_key.as_ref()
                        && let Some(existing) = Self::load_process_by_start_key_conn(tx, start_key)?
                    {
                        lash_core_execution::runtime::check_retained_start(
                            &registration,
                            &existing,
                        )?;
                        return Ok(lash_core_execution::ProcessRegistrationOutcome::existing(
                            existing,
                        ));
                    }
                    let registration = prepare_process_registration(registration)?;
                    // Admission against closure (FIG-3607 R11): a new start is
                    // refused once its starter has ended, whatever its own
                    // lifetime, and once the scope its lifetime names has
                    // closed. Both are read in this transaction, so a start
                    // racing a close either commits first and is swept, or
                    // sees the row and is refused.
                    for scope in registration
                        .ancestry
                        .starter()
                        .into_iter()
                        .chain(registration.lifetime.scope())
                    {
                        if super::parent_end::plan_exists_conn(tx, scope)? {
                            return Err(lash_core_execution::PluginError::ParentEnded {
                                start_key: registration.start_key.clone(),
                                parent: scope.clone(),
                            });
                        }
                    }
                    // Minted only once the start is admitted, so no refusal
                    // names an id that was never registered.
                    let process_id = process_id_mint.mint();
                    let change_seq = Self::next_change_seq_conn(tx)?;
                    let record =
                        ProcessRecord::from_prepared_registration(registration, process_id, now);
                    let originator_id = record.originator_id();
                    tx.execute(
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
                        ],
                    )
                    .map_err(process_sqlite_error)?;
                    let mut record = record;
                    let process_id = record.id.clone();
                    for session_id in &observers {
                        tx.execute(
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
                    Ok(lash_core_execution::ProcessRegistrationOutcome::created(
                        record,
                    ))
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    fn bind_effect_host(&self, effect_host: &Arc<dyn lash_core_execution::EffectHost>) {
        self.scope_fence_hosts.bind(
            effect_host,
            lash_core_execution::ProcessRegistryBinding {
                registrations: Arc::new(support::SqliteRegistrationProbe {
                    conn: self.conn.clone(),
                }),
            },
        );
    }

    async fn set_external_ref(
        &self,
        process_id: &ProcessId,
        external_ref: ProcessExternalRef,
    ) -> Result<ProcessRecord, lash_core_execution::PluginError> {
        let process_id = process_id.clone();
        let now = self.clock.timestamp_ms();
        let wake_delivery_config = self.wake_delivery_config;
        let fleet_format = self.fleet_format;
        let record = self
            .conn
            .write_flow(move |tx| {
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
