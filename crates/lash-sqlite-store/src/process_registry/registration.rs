use super::*;

#[async_trait::async_trait]
impl lash_core::ProcessRegistrar for SqliteProcessRegistry {
    async fn register_process_with_observers(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
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
                    // FIG-2963: ledger-based refusal replaces this
                    if registration.lifecycle.on_parent_end == lash_core::OnParentEnd::Cancel
                        && let lash_core::ParentScope::Process { process_id, incarnation } = &registration.lifecycle.parent
                        && let Some(parent) = Self::load_process_conn(tx, process_id)?.as_ref()
                        && parent.incarnation == *incarnation
                        && parent.is_terminal()
                    {
                        return Err(lash_core::PluginError::ParentEnded {
                            process_id: registration.id.clone(),
                            parent: registration.lifecycle.parent.clone(),
                        });
                    }
                    let change_seq = Self::next_change_seq_conn(tx)?;
                    let record = ProcessRecord::from_prepared_registration(
                        registration,
                        registration_fingerprint,
                        ProcessIncarnation::from_registration_sequence(change_seq),
                        now,
                    );
                    let originator_id = record.originator_id();
                    tx.execute(
                        "INSERT INTO processes (
                            process_id, incarnation, registration_fingerprint, originator_id, wake_session_id,
                            identity_kind, identity_label,
                            created_at_ms, updated_at_ms, last_event_sequence,
                            change_seq, status, record_json
                         )
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                        params![
                            record.id.as_str(),
                            record.incarnation.registration_sequence() as i64,
                            record.registration_fingerprint.as_str(),
                            originator_id.as_str(),
                            wake_session_id.as_deref(),
                            record.identity.kind.as_str(),
                            record.identity.label.as_deref(),
                            record.created_at_ms as i64,
                            record.updated_at_ms as i64,
                            record.last_event_sequence as i64,
                            change_seq as i64,
                            process_status_label(&record),
                            process_encode_json(&record)?,
                        ],
                    )
                    .map_err(process_sqlite_error)?;
                    // The owner is back: lift the scope fence a prune left in
                    // this file, in this same single-file transaction, so the
                    // process row and the fence's absence become durable
                    // together and a registration that fails keeps the id
                    // fenced (ADR 0049).
                    tx.execute(
                        "DELETE FROM effect_scope_retirements WHERE scope_id = ?1",
                        params![process_scope_fence_key(&record.id)?],
                    )
                    .map_err(process_sqlite_error)?;
                    let mut record = record;
                    let process_id = record.id.clone();
                    for session_id in &observers {
                        tx.execute(
                            "INSERT INTO process_observers (session_id, process_id, process_incarnation)
                             VALUES (?1, ?2, ?3)",
                            params![
                                session_id.as_str(),
                                record.id.as_str(),
                                record.incarnation.registration_sequence() as i64
                            ],
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
        // Hosts whose fence is not in this file (a fence written into a
        // journal before it attached this registry, or an engine-held one)
        // are lifted now that the row is durable; the call is idempotent for
        // a host that keeps its process-scope fences here.
        self.scope_fence_hosts
            .reinstate_process_scope(&record.id)
            .await?;
        Ok(record)
    }

    fn bind_effect_host(&self, effect_host: &Arc<dyn lash_core::EffectHost>) {
        self.scope_fence_hosts.bind(
            effect_host,
            lash_core::ProcessRegistryBinding {
                fence_database: self.path.clone(),
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
    ) -> Result<ProcessRecord, lash_core::PluginError> {
        let process_id = process_id.clone();
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
            .map_err(process_sqlite_error)??;
        Ok(record)
    }
}
