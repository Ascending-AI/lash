use crate::*;
use lash_core::facade_support;
#[path = "process_registry/parent_end.rs"]
mod parent_end;
mod prune;
mod retention;
mod wake_delivery;
#[path = "process_registry/worklist.rs"]
mod worklist;

use prune::prune_process_rows_tx;
use retention::{filter_tombstoned_process_ids, filter_unregistered_process_ids};
use wake_delivery::{
    claim_pending_wake_deliveries, decode_wake_delivery_row, load_wake_delivery_tx,
    update_wake_delivery_state, wake_delivery_report,
};

#[async_trait::async_trait]
impl ProcessRegistry for PostgresProcessRegistry {
    fn wake_delivery_config(&self) -> lash_core::WakeDeliveryConfig {
        self.wake_delivery_config
    }

    fn with_runtime_clock(
        &self,
        clock: Arc<dyn lash_core::Clock>,
    ) -> Option<Arc<dyn ProcessRegistry>> {
        Some(Arc::new(self.clone().with_clock(clock)))
    }

    async fn register_process_with_observers(
        &self,
        registration: ProcessRegistration,
        observers: &[String],
    ) -> Result<ProcessRecord, PluginError> {
        let registration = lash_core::runtime::prepare_process_registration(registration)?;
        let mut observers = observers.to_vec();
        observers.sort();
        observers.dedup();
        let registration_fingerprint =
            lash_core::runtime::process_registration_fingerprint(&registration, &observers);
        let wake_session_id = registration.wake_session_id.clone();
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        if let Some(existing) = load_process_tx(&mut tx, &registration.id).await? {
            if existing.registration_fingerprint == registration_fingerprint {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(existing);
            }
            return Err(PluginError::Session(format!(
                "process `{}` registration fingerprint conflict: existing {}, new {}",
                registration.id, existing.registration_fingerprint, registration_fingerprint
            )));
        }
        let now = self.clock.timestamp_ms();
        let mut record =
            ProcessRecord::from_prepared_registration(registration, registration_fingerprint, now);
        let record_json = serde_json::to_string(&record).map_err(process_decode_error)?;
        let change_seq = next_process_change_seq_tx(&mut tx).await?;
        sqlx::query(
            "INSERT INTO lash_processes (
                process_id, registration_fingerprint, originator_id, wake_session_id,
                identity_kind, identity_label, is_waiting,
                created_at_ms, updated_at_ms, change_seq, status, record_json
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(&record.id)
        .bind(&record.registration_fingerprint)
        .bind(record.originator_id().as_str())
        .bind(wake_session_id)
        .bind(&record.identity.kind)
        .bind(&record.identity.label)
        .bind(record.wait.is_some())
        .bind(record.created_at_ms as i64)
        .bind(record.updated_at_ms as i64)
        .bind(change_seq as i64)
        .bind(process_status_label(&record))
        .bind(record_json)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        let process_id = record.id.clone();
        for session_id in observers {
            sqlx::query(
                "INSERT INTO lash_process_observers (session_id, process_id)
                 VALUES ($1, $2)",
            )
            .bind(&session_id)
            .bind(&process_id)
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
            )
            .await?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(record)
    }

    async fn set_external_ref(
        &self,
        process_id: &str,
        external_ref: ProcessExternalRef,
    ) -> Result<ProcessRecord, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        if let Some(existing) = &record.external_ref {
            if existing == &external_ref {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(record);
            }
            return Err(process_external_ref_conflict(
                process_id,
                existing,
                &external_ref,
            ));
        }
        let request = ProcessEventAppendRequest::external_ref_set(process_id, &external_ref);
        append_process_event_tx(
            &mut tx,
            &mut record,
            request,
            self.clock.timestamp_ms(),
            self.wake_delivery_config,
        )
        .await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(record)
    }

    async fn add_observer(
        &self,
        session_id: &str,
        process_id: &str,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let changed = sqlx::query(
            "INSERT INTO lash_process_observers (session_id, process_id)
             VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(session_id)
        .bind(process_id)
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
            )
            .await?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(())
    }

    async fn remove_observer(
        &self,
        session_id: &str,
        process_id: &str,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let changed = sqlx::query(
            "DELETE FROM lash_process_observers WHERE session_id = $1 AND process_id = $2",
        )
        .bind(session_id)
        .bind(process_id)
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
            )
            .await?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(())
    }

    async fn transfer_observers(
        &self,
        from_session_id: &str,
        to_session_id: &str,
        process_ids: &[String],
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        for process_id in process_ids {
            let mut record = require_process_tx(&mut tx, process_id).await?;
            let removed = sqlx::query(
                "DELETE FROM lash_process_observers
                 WHERE session_id = $1 AND process_id = $2",
            )
            .bind(from_session_id)
            .bind(process_id)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected();
            if removed == 0 {
                return Err(PluginError::Session(format!(
                    "process `{process_id}` is not observed by `{from_session_id}`"
                )));
            }
            sqlx::query(
                "INSERT INTO lash_process_observers (session_id, process_id)
                 VALUES ($1, $2) ON CONFLICT DO NOTHING",
            )
            .bind(to_session_id)
            .bind(process_id)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
            append_process_event_tx(
                &mut tx,
                &mut record,
                ProcessEventAppendRequest::observer_removed(process_id, from_session_id, &by),
                self.clock.timestamp_ms(),
                self.wake_delivery_config,
            )
            .await?;
            append_process_event_tx(
                &mut tx,
                &mut record,
                ProcessEventAppendRequest::observer_added(process_id, to_session_id, &by),
                self.clock.timestamp_ms(),
                self.wake_delivery_config,
            )
            .await?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)
    }

    async fn list_observed_by(&self, session_id: &str) -> Result<Vec<ProcessRecord>, PluginError> {
        let rows = sqlx::query(
            "SELECT p.record_json
             FROM lash_process_observers o
             JOIN lash_processes p ON p.process_id = o.process_id
             WHERE o.session_id = $1
             ORDER BY p.process_id",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        rows.into_iter()
            .map(|row| serde_json::from_str(&row.get::<String, _>(0)).map_err(process_decode_error))
            .collect()
    }

    async fn is_observer(&self, session_id: &str, process_id: &str) -> Result<bool, PluginError> {
        let (retained, observer): (bool, bool) = sqlx::query_as(
            "SELECT
                 EXISTS(SELECT 1 FROM lash_processes WHERE process_id = $2),
                 EXISTS(
                     SELECT 1 FROM lash_process_observers
                     WHERE session_id = $1 AND process_id = $2
                 )",
        )
        .bind(session_id)
        .bind(process_id)
        .fetch_one(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        if retained {
            return Ok(observer);
        }
        self.get_process(process_id).await?;
        Ok(false)
    }

    async fn observers_for_process(&self, process_id: &str) -> Result<Vec<String>, PluginError> {
        if self.get_process(process_id).await?.is_none() {
            return Err(registry_transitions::unknown_process(process_id));
        }
        sqlx::query_scalar(
            "SELECT session_id FROM lash_process_observers
             WHERE process_id = $1 ORDER BY session_id",
        )
        .bind(process_id)
        .fetch_all(&self.pool)
        .await
        .map_err(plugin_sqlx_error)
    }

    async fn retarget_subscription(
        &self,
        process_id: &str,
        target: Option<&str>,
    ) -> Result<(), PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let previous: Option<String> =
            sqlx::query_scalar("SELECT wake_session_id FROM lash_processes WHERE process_id = $1")
                .bind(process_id)
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
        )
        .await?;
        sqlx::query("UPDATE lash_processes SET wake_session_id = $2 WHERE process_id = $1")
            .bind(process_id)
            .bind(target)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        if let Some(previous) = previous {
            sqlx::query(
                "UPDATE lash_process_wake_deliveries
                 SET state = 'discarded', discard_reason = 'retargeted'
                 WHERE process_id = $1 AND target_session_id = $2 AND state = 'pending'",
            )
            .bind(process_id)
            .bind(previous)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)
    }

    async fn delete_session_process_state(
        &self,
        session_id: &str,
    ) -> Result<lash_core::ProcessSessionDeleteReport, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let discarded_wake_delivery_count = sqlx::query(
            "UPDATE lash_process_wake_deliveries
             SET state = 'discarded', discard_reason = 'target_gone'
             WHERE target_session_id = $1 AND state = 'pending'",
        )
        .bind(session_id)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?
        .rows_affected() as usize;
        let removed_observer_count =
            sqlx::query("DELETE FROM lash_process_observers WHERE session_id = $1")
                .bind(session_id)
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?
                .rows_affected() as usize;
        let cleared_subscription_count = sqlx::query(
            "UPDATE lash_processes SET wake_session_id = NULL WHERE wake_session_id = $1",
        )
        .bind(session_id)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?
        .rows_affected() as usize;
        sqlx::query("DELETE FROM lash_wake_allocation_floors WHERE target_session_id = $1")
            .bind(session_id)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
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
    ) -> Result<Option<u64>, PluginError> {
        sqlx::query_scalar::<_, i64>(
            "SELECT allocation_floor FROM lash_wake_allocation_floors
             WHERE target_session_id = $1 AND process_id = $2",
        )
        .bind(target_session_id)
        .bind(process_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?
        .map(|value| plugin_u64_from_sql("WakeAllocationFloor", "allocation_floor", value))
        .transpose()
    }

    async fn append_event(
        &self,
        process_id: &str,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendResult, PluginError> {
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
        )
        .await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(result)
    }

    async fn append_event_with_authority(
        &self,
        process_id: &str,
        request: ProcessEventAppendRequest,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessEventAppendResult, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let now_ms = process_lease_now_epoch_ms_tx(&mut tx).await?;
        validate_process_execution_authority_tx(
            &mut tx, process_id, &record, authority, None, now_ms,
        )
        .await?;
        // `occurred_at_ms` provenance is inconsistent in this backend: four
        // mutating paths stamp it from the server clock while the others (like
        // this one) use the injected clock. Decision-inert today — no fence or
        // retention predicate reads it — tracked as FIG-971.
        let occurred_at_ms = self.clock.timestamp_ms();
        let result = append_process_event_tx(
            &mut tx,
            &mut record,
            request,
            occurred_at_ms,
            self.wake_delivery_config,
        )
        .await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(result)
    }

    async fn events_after(
        &self,
        process_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<ProcessEvent>, PluginError> {
        if self.get_process(process_id).await?.is_none() {
            return Err(registry_transitions::unknown_process(process_id));
        }
        let rows = sqlx::query(
            "SELECT event_json FROM lash_process_events
             WHERE process_id = $1 AND sequence > $2
             ORDER BY sequence ASC",
        )
        .bind(process_id)
        .bind(after_sequence as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        let mut events = Vec::new();
        for row in rows {
            let json: String = row.get(0);
            events.push(serde_json::from_str(&json).map_err(process_decode_error)?);
        }
        Ok(events)
    }

    async fn count_events_through(
        &self,
        process_id: &str,
        event_type: &str,
        up_to_sequence: u64,
    ) -> Result<u64, PluginError> {
        if self.get_process(process_id).await?.is_none() {
            return Err(registry_transitions::unknown_process(process_id));
        }
        let row = sqlx::query(
            "SELECT COUNT(*) FROM lash_process_events
             WHERE process_id = $1 AND event_type = $2 AND sequence <= $3",
        )
        .bind(process_id)
        .bind(event_type)
        .bind(up_to_sequence as i64)
        .fetch_one(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        let count: i64 = row.get(0);
        Ok(count as u64)
    }

    async fn recent_events(
        &self,
        process_id: &str,
        limit: usize,
    ) -> Result<Vec<ProcessEvent>, PluginError> {
        if self.get_process(process_id).await?.is_none() {
            return Err(registry_transitions::unknown_process(process_id));
        }
        let rows = sqlx::query(
            "SELECT event_json FROM lash_process_events
             WHERE process_id = $1
             ORDER BY sequence DESC
             LIMIT $2",
        )
        .bind(process_id)
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

    async fn complete_process(
        &self,
        process_id: &str,
        await_output: ProcessAwaitOutput,
        authority: lash_core::ProcessCompletionAuthority,
    ) -> Result<lash_core::ProcessCompletionOutcome, PluginError> {
        self.complete_process_with_parent_end(process_id, await_output, authority, Vec::new())
            .await
    }

    async fn complete_process_with_parent_end(
        &self,
        process_id: &str,
        await_output: ProcessAwaitOutput,
        authority: lash_core::ProcessCompletionAuthority,
        parent_end_actions: Vec<lash_core::ToolIntentParentEndAction>,
    ) -> Result<lash_core::ProcessCompletionOutcome, PluginError> {
        // Load (FOR UPDATE), validate the authority against the row's declared
        // disposition, and append the terminal event as one transaction. The
        // `FOR UPDATE` row lock held from the load through the commit is the
        // guard: under READ COMMITTED a concurrent complete→prune→re-register
        // would otherwise change the disposition between a separate read and the
        // append. Locking the row means the disposition we validate is the
        // disposition we append against — the re-registration serialises either
        // fully before our load or fully after our commit.
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        if record.is_terminal() {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(lash_core::ProcessCompletionOutcome::from_stored(
                record,
                &await_output,
            ));
        }
        authority.validate(process_id, record.disposition, &await_output)?;
        let request =
            facade_support::terminal_append_request(process_id, &await_output, Some(&authority));
        let replay_lookup =
            if let Some(replay_key) = request.replay.as_ref().map(|r| r.key.as_str()) {
                load_event_by_key_tx(&mut tx, process_id, replay_key).await?
            } else {
                None
            };
        let occurred_at_ms = self.clock.timestamp_ms();
        let wake_session_id = wake_session_id_tx(&mut tx, process_id).await?;
        let (last_sequence, sequence) =
            next_process_event_sequence_tx(&mut tx, process_id, wake_session_id.as_deref()).await?;
        let prepared = lash_core::runtime::prepare_process_event_append(
            &record,
            request,
            sequence,
            last_sequence,
            replay_lookup,
            occurred_at_ms,
            wake_session_id.as_deref(),
        )?;
        match prepared {
            facade_support::ProcessEventAppendPlan::Replay {
                repair_record,
                wake_delivery,
                ..
            } => {
                insert_wake_delivery_tx(&mut tx, wake_delivery.as_ref(), self.wake_delivery_config)
                    .await?;
                if let Some(repaired) = repair_record {
                    record = repaired;
                    save_process_tx(&mut tx, &record).await?;
                }
                tx.commit().await.map_err(plugin_sqlx_error)?;
                Ok(lash_core::ProcessCompletionOutcome::AlreadyApplied { stored: record })
            }
            facade_support::ProcessEventAppendPlan::Insert {
                event,
                projected_record,
                occurred_at_ms,
                wake_delivery,
            } => {
                sqlx::query(
                    "INSERT INTO lash_process_events (
                        process_id, sequence, event_type, idempotency_key,
                        occurred_at_ms, event_json
                     )
                     VALUES ($1, $2, $3, $4, $5, $6)",
                )
                .bind(process_id)
                .bind(sequence as i64)
                .bind(event.event_type.as_str())
                .bind(event.invocation.replay_key())
                .bind(occurred_at_ms as i64)
                .bind(serde_json::to_string(&event).map_err(process_decode_error)?)
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?;
                record = projected_record;
                save_process_tx(&mut tx, &record).await?;
                parent_end::insert(&mut tx, process_id, &parent_end_actions).await?;
                insert_wake_delivery_tx(&mut tx, wake_delivery.as_ref(), self.wake_delivery_config)
                    .await?;
                advance_wake_allocation_floor_tx(
                    &mut tx,
                    wake_session_id.as_deref(),
                    process_id,
                    sequence,
                )
                .await?;
                tx.commit().await.map_err(plugin_sqlx_error)?;
                Ok(lash_core::ProcessCompletionOutcome::Committed(record))
            }
        }
    }

    async fn complete_process_with_lease(
        &self,
        lease: &ProcessLease,
        await_output: ProcessAwaitOutput,
    ) -> Result<lash_core::ProcessCompletionOutcome, PluginError> {
        self.complete_process_with_lease_and_parent_end(lease, await_output, Vec::new())
            .await
    }

    async fn complete_process_with_lease_and_parent_end(
        &self,
        lease: &ProcessLease,
        await_output: ProcessAwaitOutput,
        parent_end_actions: Vec<lash_core::ToolIntentParentEndAction>,
    ) -> Result<lash_core::ProcessCompletionOutcome, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let process_id = lease.process_id.as_str();
        let mut record = require_process_tx(&mut tx, process_id).await?;
        if record.is_terminal() {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(lash_core::ProcessCompletionOutcome::from_stored(
                record,
                &await_output,
            ));
        }
        let request = facade_support::terminal_append_request(process_id, &await_output, None);
        let replay_lookup =
            if let Some(replay_key) = request.replay.as_ref().map(|r| r.key.as_str()) {
                load_event_by_key_tx(&mut tx, process_id, replay_key).await?
            } else {
                None
            };
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        let wake_session_id = wake_session_id_tx(&mut tx, process_id).await?;
        let (last_sequence, sequence) =
            next_process_event_sequence_tx(&mut tx, process_id, wake_session_id.as_deref()).await?;
        let prepared = lash_core::runtime::prepare_process_event_append(
            &record,
            request,
            sequence,
            last_sequence,
            replay_lookup,
            now,
            wake_session_id.as_deref(),
        )?;
        if let facade_support::ProcessEventAppendPlan::Replay {
            repair_record,
            wake_delivery,
            ..
        } = prepared
        {
            insert_wake_delivery_tx(&mut tx, wake_delivery.as_ref(), self.wake_delivery_config)
                .await?;
            apply_process_replay_repair_tx(&mut tx, &mut record, repair_record).await?;
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(lash_core::ProcessCompletionOutcome::AlreadyApplied { stored: record });
        }

        let current = load_process_lease_tx(&mut tx, process_id).await?;
        registry_transitions::authorize_process_lease_write(
            process_id,
            lease,
            current.as_ref(),
            now,
        )?;

        let facade_support::ProcessEventAppendPlan::Insert {
            event,
            projected_record,
            occurred_at_ms,
            wake_delivery,
        } = prepared
        else {
            unreachable!("replay returned above")
        };
        sqlx::query(
            "INSERT INTO lash_process_events (
                process_id, sequence, event_type, idempotency_key,
                occurred_at_ms, event_json
             ) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(process_id)
        .bind(sequence as i64)
        .bind(event.event_type.as_str())
        .bind(event.invocation.replay_key())
        .bind(occurred_at_ms as i64)
        .bind(serde_json::to_string(&event).map_err(process_decode_error)?)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        record = projected_record;
        save_process_tx(&mut tx, &record).await?;
        parent_end::insert(&mut tx, process_id, &parent_end_actions).await?;
        insert_wake_delivery_tx(&mut tx, wake_delivery.as_ref(), self.wake_delivery_config).await?;
        advance_wake_allocation_floor_tx(&mut tx, wake_session_id.as_deref(), process_id, sequence)
            .await?;
        let released = sqlx::query(
            "UPDATE lash_process_leases
             SET lease_owner_id = NULL,
                 lease_owner_incarnation_id = NULL,
                 lease_owner_liveness_json = NULL,
                 lease_token = NULL,
                 lease_claimed_at_ms = 0,
                 lease_expires_at_ms = 0
             WHERE process_id = $1
               AND lease_token = $2
               AND lease_fencing_token = $3",
        )
        .bind(process_id)
        .bind(&lease.lease_token)
        .bind(lease.fencing_token as i64)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?
        .rows_affected();
        if released != 1 {
            // PostgreSQL-only post-write assertion: the row is held under the
            // `FOR UPDATE` taken by `load_process_lease_tx`, so a fence that
            // authorized the write above cannot have moved. Preserved as it
            // shipped rather than mirrored onto SQLite, whose `BEGIN IMMEDIATE`
            // write lock makes the same guarantee positionally.
            return Err(PluginError::ProcessLeaseSuperseded {
                process_id: process_id.to_string(),
            });
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lash_core::ProcessCompletionOutcome::Committed(record))
    }

    async fn list_pending_parent_end_plans(
        &self,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<lash_core::ProcessParentEndPlan>, PluginError> {
        parent_end::list(&self.pool, limit).await
    }

    async fn get_pending_parent_end_plan(
        &self,
        process_id: &str,
    ) -> Result<Option<lash_core::ProcessParentEndPlan>, PluginError> {
        parent_end::get(&self.pool, process_id).await
    }

    async fn complete_parent_end_plan(&self, process_id: &str) -> Result<(), PluginError> {
        parent_end::complete(&self.pool, process_id).await
    }

    async fn record_first_started_with_authority(
        &self,
        process_id: &str,
        started: ProcessStarted,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessStartOutcome, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        validate_process_execution_authority_tx(
            &mut tx,
            process_id,
            &record,
            authority,
            Some(&started),
            now,
        )
        .await?;
        match lash_core::runtime::prepare_process_start(&record, &started, authority)? {
            ProcessStartPlan::AlreadyApplied => {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(ProcessStartOutcome::AlreadyApplied(record));
            }
            ProcessStartPlan::AlreadyStarted { by } => {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(ProcessStartOutcome::AlreadyStarted {
                    current: record,
                    by,
                });
            }
            ProcessStartPlan::AttemptsExhausted {
                attempts,
                max_attempts,
            } => {
                tx.commit().await.map_err(plugin_sqlx_error)?;
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
        let request =
            ProcessEventAppendRequest::first_started(process_id, &started, resumed_from_handover);
        append_process_event_tx(
            &mut tx,
            &mut record,
            request,
            now,
            self.wake_delivery_config,
        )
        .await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(ProcessStartOutcome::Started(record))
    }

    async fn request_process_abandon(
        &self,
        process_id: &str,
        request: AbandonRequest,
    ) -> Result<ProcessRecord, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        if record.is_terminal() {
            return Err(PluginError::Session(format!(
                "terminal process `{process_id}` cannot accept an abandon request"
            )));
        }
        if record.abandon_request.is_some() {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(record);
        }
        let append = ProcessEventAppendRequest::abandon_requested(process_id, &request);
        append_process_event_tx(
            &mut tx,
            &mut record,
            append,
            self.clock.timestamp_ms(),
            self.wake_delivery_config,
        )
        .await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(record)
    }

    async fn set_process_wait_with_authority(
        &self,
        process_id: &str,
        wait: lash_core::WaitState,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let lease_now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        validate_process_execution_authority_tx(
            &mut tx, process_id, &record, authority, None, lease_now,
        )
        .await?;
        if record.is_terminal() {
            return Err(PluginError::Session(format!(
                "terminal process `{process_id}` cannot enter a wait state"
            )));
        }
        if record.wait.as_ref() == Some(&wait) {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(record);
        }
        let request = ProcessEventAppendRequest::wait_entered(process_id, &wait);
        append_process_event_tx(
            &mut tx,
            &mut record,
            request,
            self.clock.timestamp_ms(),
            self.wake_delivery_config,
        )
        .await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(record)
    }

    async fn clear_process_wait_with_authority(
        &self,
        process_id: &str,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = require_process_tx(&mut tx, process_id).await?;
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        validate_process_execution_authority_tx(&mut tx, process_id, &record, authority, None, now)
            .await?;
        let Some(wait) = record.wait.clone() else {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(record);
        };
        let request = ProcessEventAppendRequest::wait_cleared(process_id, &wait);
        append_process_event_tx(
            &mut tx,
            &mut record,
            request,
            now,
            self.wake_delivery_config,
        )
        .await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(record)
    }

    async fn get_process(&self, process_id: &str) -> Result<Option<ProcessRecord>, PluginError> {
        if let Some(record) = load_process(&self.pool, process_id).await? {
            return Ok(Some(record));
        }
        let row = sqlx::query(
            "SELECT terminal_label, pruned_at_ms
             FROM lash_process_tombstones WHERE process_id = $1",
        )
        .bind(process_id)
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
        filter: &lash_core::ProcessListFilter,
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
        let rows = sqlx::query(
            "SELECT record_json FROM lash_processes
             WHERE ($1::TEXT IS NULL OR status = $1)
               AND ($2::BOOLEAN IS NULL OR is_waiting = $2)
               AND ($3::TEXT IS NULL OR originator_id = $3)
               AND ($4::TEXT IS NULL OR identity_kind = $4)
               AND ($5::TEXT IS NULL OR identity_label = $5)
               AND ($6::JSONB IS NULL OR
                    (record_json::JSONB #> '{identity,definition}') = $6)
               AND ($7::TEXT IS NULL OR
                    (record_json::JSONB #>> '{provenance,caused_by,occurrence_id}') = $7)
               AND ($8::TEXT IS NULL OR
                    (record_json::JSONB #>> '{provenance,caused_by,subscription_id}') = $8)
               AND ($9::BIGINT IS NULL OR created_at_ms >= $9)
               AND ($10::BIGINT IS NULL OR created_at_ms < $10)
             ORDER BY process_id ASC",
        )
        .bind(filter.status.label())
        .bind(filter.waiting)
        .bind(filter.originator_id.as_deref())
        .bind(filter.identity_kind.as_deref())
        .bind(filter.identity_label.as_deref())
        .bind(definition)
        .bind(filter.caused_by_occurrence_id.as_deref())
        .bind(filter.caused_by_subscription_id.as_deref())
        .bind(filter.created_at_start_ms.map(|value| value as i64))
        .bind(
            filter
                .created_at_end_ms
                .filter(|value| *value <= i64::MAX as u64)
                .map(|value| value as i64),
        )
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
        if limit == 0 {
            return Ok((Vec::new(), cursor));
        }
        let rows = sqlx::query(
            "SELECT change_seq, kind, payload FROM (
                 SELECT change_seq, 'upsert' AS kind, record_json AS payload
                 FROM lash_processes WHERE change_seq > $1
                 UNION ALL
                 SELECT pruned_change_seq, 'deleted' AS kind,
                        json_build_object(
                            'process_id', process_id,
                            'terminal_label', terminal_label,
                            'pruned_at_ms', pruned_at_ms,
                            'pruned_change_seq', pruned_change_seq
                        )::TEXT AS payload
                 FROM lash_process_tombstones WHERE pruned_change_seq > $1
             ) changes
             ORDER BY change_seq ASC
             LIMIT $2",
        )
        .bind(cursor.store_sequence() as i64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
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
        Ok((records, next_cursor))
    }

    async fn compact_process_tombstones(
        &self,
        cutoff_epoch_ms: u64,
        watermark: lash_core::ProjectionWatermark,
        trigger_store: Option<&dyn lash_core::TriggerStore>,
    ) -> Result<usize, PluginError> {
        let max_change_seq = match watermark {
            lash_core::ProjectionWatermark::UpTo(cursor) => Some(cursor.store_sequence() as i64),
            lash_core::ProjectionWatermark::NoProjector => None,
        };
        let outstanding_trigger_delivery_process_ids = match trigger_store {
            Some(trigger_store) => trigger_store.list_delivery_process_ids().await?,
            None => Vec::new(),
        };
        let cutoff_epoch_ms = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        Ok(sqlx::query(
            "DELETE FROM lash_process_tombstones
                 WHERE pruned_at_ms < $1
                   AND ($2::BIGINT IS NULL OR pruned_change_seq <= $2)
                   AND NOT (process_id = ANY($3::TEXT[]))",
        )
        .bind(cutoff_epoch_ms)
        .bind(max_change_seq)
        .bind(&outstanding_trigger_delivery_process_ids)
        .execute(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?
        .rows_affected() as usize)
    }

    async fn claim_pending_wake_deliveries(
        &self,
        limit: usize,
    ) -> Result<Vec<lash_core::WakeDelivery>, PluginError> {
        claim_pending_wake_deliveries(self, limit).await
    }

    async fn list_wake_deliveries(
        &self,
        state: Option<lash_core::WakeDeliveryState>,
    ) -> Result<Vec<lash_core::WakeDelivery>, PluginError> {
        let rows = if let Some(state) = state {
            sqlx::query(
                "SELECT delivery_id, state, claim_token, attempts, first_attempt_ms,
                        next_attempt_at_ms, expires_at_ms, discard_reason, delivery_json
                 FROM lash_process_wake_deliveries
                 WHERE state = $1
                 ORDER BY delivery_id ASC",
            )
            .bind(state.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?
        } else {
            sqlx::query(
                "SELECT delivery_id, state, claim_token, attempts, first_attempt_ms,
                        next_attempt_at_ms, expires_at_ms, discard_reason, delivery_json
                 FROM lash_process_wake_deliveries
                 ORDER BY delivery_id ASC",
            )
            .fetch_all(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?
        };
        rows.into_iter().map(decode_wake_delivery_row).collect()
    }

    async fn wake_delivery_report(&self) -> Result<lash_core::WakeDeliveryReport, PluginError> {
        let deliveries = self.list_wake_deliveries(None).await?;
        Ok(wake_delivery_report(deliveries.iter()))
    }

    async fn mark_wake_enqueued(
        &self,
        delivery_id: &str,
        claim_token: &str,
    ) -> Result<lash_core::WakeDeliveryClaimOutcome, PluginError> {
        update_wake_delivery_state(
            &self.pool,
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
    ) -> Result<lash_core::WakeDeliveryClaimOutcome, PluginError> {
        update_wake_delivery_state(
            &self.pool,
            delivery_id,
            claim_token,
            lash_core::WakeDeliveryState::Discarded,
            Some(reason),
        )
        .await
    }

    async fn redrive_wake_delivery(&self, delivery_id: &str) -> Result<(), PluginError> {
        let expires_at_ms = self
            .clock
            .timestamp_ms()
            .saturating_add(self.wake_delivery_config.delivery_expiry_ms);
        let next_attempt_at_ms = self.clock.timestamp_ms();
        let changed = sqlx::query(
            "UPDATE lash_process_wake_deliveries
             SET state = 'pending', attempts = 0, first_attempt_ms = NULL,
                 claim_token = NULL, next_attempt_at_ms = $3, expires_at_ms = $2,
                 discard_reason = NULL
             WHERE delivery_id = $1 AND state = 'discarded'",
        )
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
    ) -> Result<lash_core::WakeDeliveryClaimOutcome, PluginError> {
        let changed = sqlx::query(
            "UPDATE lash_process_wake_deliveries
             SET state = 'pending', claim_token = NULL, next_attempt_at_ms = $3
             WHERE delivery_id = $1 AND state = 'enqueuing' AND claim_token = $2",
        )
        .bind(delivery_id)
        .bind(claim_token)
        .bind(next_attempt_at_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?
        .rows_affected();
        if changed == 0 {
            let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
            let delivery = load_wake_delivery_tx(&mut tx, delivery_id).await?;
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(lash_core::WakeDeliveryClaimOutcome::ClaimLost {
                state: delivery.state,
            });
        }
        Ok(lash_core::WakeDeliveryClaimOutcome::Applied)
    }

    async fn list_non_terminal_page(
        &self,
        limit: std::num::NonZeroUsize,
        continuation: Option<lash_core::ProcessWorklistCursor>,
    ) -> Result<lash_core::ProcessWorklistPage, PluginError> {
        worklist::list_non_terminal_page(self, limit, continuation).await
    }

    async fn filter_unregistered_process_ids(
        &self,
        process_ids: &[String],
    ) -> Result<Vec<String>, PluginError> {
        filter_unregistered_process_ids(&self.pool, process_ids).await
    }

    async fn filter_tombstoned_process_ids(
        &self,
        process_ids: &[String],
    ) -> Result<Vec<String>, PluginError> {
        filter_tombstoned_process_ids(&self.pool, process_ids).await
    }

    async fn live_reference_summary(
        &self,
    ) -> Result<Vec<ProcessLiveReferenceSummary>, PluginError> {
        let records = worklist::collect_non_terminal_records(self).await?;
        Ok(ProcessLiveReferenceSummary::from_records(records.iter()))
    }

    async fn count_non_terminal_processes(&self) -> Result<usize, PluginError> {
        worklist::count_non_terminal_processes(self).await
    }

    async fn claim_process_lease(
        &self,
        process_id: &str,
        owner: &LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::ProcessLeaseClaimOutcome, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        require_process_tx(&mut tx, process_id).await?;
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        let current = load_process_lease_tx(&mut tx, process_id).await?;
        let fencing_token = match registry_transitions::decide_process_lease_claim(
            current.as_ref(),
            owner,
            now,
            lease_ttl_ms,
        ) {
            registry_transitions::ProcessLeaseClaimDecision::ExtendHeldLease { lease } => {
                // Same incarnation re-enters its own live lease: extend the
                // expiry, keep token and fencing token.
                sqlx::query(
                    "UPDATE lash_process_leases
                     SET lease_expires_at_ms = $2
                     WHERE process_id = $1",
                )
                .bind(process_id)
                .bind(lease.expires_at_epoch_ms as i64)
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?;
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(lash_core::ProcessLeaseClaimOutcome::Acquired(lease));
            }
            registry_transitions::ProcessLeaseClaimDecision::ReportBusy { holder } => {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(lash_core::ProcessLeaseClaimOutcome::Busy { holder });
            }
            registry_transitions::ProcessLeaseClaimDecision::AcquireOnRetainedFence => {
                registry_transitions::next_process_lease_fencing_token(
                    retained_process_lease_fencing_token(&mut tx, process_id).await?,
                )?
            }
        };
        let lease =
            acquire_process_lease_tx(&mut tx, process_id, owner, fencing_token, now, lease_ttl_ms)
                .await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lash_core::ProcessLeaseClaimOutcome::Acquired(lease))
    }

    async fn reclaim_process_lease(
        &self,
        process_id: &str,
        owner: &LeaseOwnerIdentity,
        _observed_holder: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::ProcessLeaseClaimOutcome, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        require_process_tx(&mut tx, process_id).await?;
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        let current = load_process_lease_tx(&mut tx, process_id).await?;
        let fencing_token =
            match registry_transitions::decide_process_lease_reclaim(current.as_ref(), now)? {
                registry_transitions::ProcessLeaseReclaimDecision::AcquireOnRetainedFence => {
                    // Free (or released) lease: acquire on the retained fencing
                    // token like a plain claim would.
                    registry_transitions::next_process_lease_fencing_token(
                        retained_process_lease_fencing_token(&mut tx, process_id).await?,
                    )?
                }
                registry_transitions::ProcessLeaseReclaimDecision::AcquireOnObservedFence {
                    fencing_token,
                } => fencing_token,
                registry_transitions::ProcessLeaseReclaimDecision::ReportBusy { holder } => {
                    tx.commit().await.map_err(plugin_sqlx_error)?;
                    return Ok(lash_core::ProcessLeaseClaimOutcome::Busy { holder });
                }
            };
        let lease =
            acquire_process_lease_tx(&mut tx, process_id, owner, fencing_token, now, lease_ttl_ms)
                .await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lash_core::ProcessLeaseClaimOutcome::Acquired(lease))
    }

    async fn renew_process_lease(
        &self,
        lease: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLease, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        let current = load_process_lease_tx(&mut tx, &lease.process_id).await?;
        registry_transitions::authorize_process_lease_write(
            &lease.process_id,
            lease,
            current.as_ref(),
            now,
        )?;
        let renewed = ProcessLease {
            expires_at_epoch_ms: now.saturating_add(lease_ttl_ms),
            ..lease.clone()
        };
        sqlx::query(
            "UPDATE lash_process_leases
             SET lease_expires_at_ms = $2
             WHERE process_id = $1 AND lease_token = $3",
        )
        .bind(&renewed.process_id)
        .bind(renewed.expires_at_epoch_ms as i64)
        .bind(&renewed.lease_token)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(renewed)
    }

    async fn get_process_lease(
        &self,
        process_id: &str,
    ) -> Result<Option<ProcessLease>, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let lease = load_process_lease_tx(&mut tx, process_id).await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lease)
    }

    async fn complete_process_lease(
        &self,
        completion: &ProcessLeaseCompletion,
    ) -> Result<(), PluginError> {
        sqlx::query(
            "UPDATE lash_process_leases
             SET lease_owner_id = NULL,
                 lease_token = NULL,
                 lease_claimed_at_ms = 0,
                 lease_expires_at_ms = 0
             WHERE process_id = $1 AND lease_token = $2",
        )
        .bind(&completion.process_id)
        .bind(&completion.lease_token)
        .execute(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        Ok(())
    }

    async fn prune_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<lash_core::ProcessListFilter>,
        watermark: lash_core::ProjectionWatermark,
    ) -> Result<ProcessPruneReport, PluginError> {
        let cutoff = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        let pruned_at_ms = self.clock.timestamp_ms() as i64;
        let max_change_seq = match watermark {
            lash_core::ProjectionWatermark::UpTo(cursor) => Some(cursor.store_sequence() as i64),
            lash_core::ProjectionWatermark::NoProjector => None,
        };
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let rows = sqlx::query(
            "SELECT process_id, record_json FROM lash_processes
             WHERE status NOT IN ('running', 'waiting')
               AND updated_at_ms < $1
               AND ($2::BIGINT IS NULL OR change_seq <= $2)
               AND NOT EXISTS (
                   SELECT 1 FROM lash_process_wake_deliveries AS delivery
                   WHERE delivery.process_id = lash_processes.process_id
                     AND delivery.state IN ('pending', 'enqueuing')
               )
             ORDER BY process_id ASC
             FOR UPDATE",
        )
        .bind(cutoff)
        .bind(max_change_seq)
        .fetch_all(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        let mut prunable = Vec::new();
        for row in rows {
            let process_id: String = row.get(0);
            let record_json: String = row.get(1);
            let record: ProcessRecord =
                serde_json::from_str(&record_json).map_err(process_decode_error)?;
            if filter
                .as_ref()
                .is_none_or(|filter| filter.matches_record(&record))
            {
                prunable.push(process_id);
            }
        }

        if prunable.is_empty() {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(ProcessPruneReport {
                pruned_processes: 0,
                pruned_events: 0,
                pruned_trigger_deliveries: 0,
            });
        }

        let process_ids = prunable;
        let session_ids = process_ids
            .iter()
            .flat_map(|process_id| facade_support::process_runtime_session_ids(process_id))
            .collect::<Vec<_>>();
        delete_process_sessions_tx(&mut tx, &session_ids)
            .await
            .map_err(|err| PluginError::Session(err.to_string()))?;

        let report = prune_process_rows_tx(&mut tx, &process_ids, pruned_at_ms).await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(report)
    }
}

#[async_trait::async_trait]
impl ProcessContinuationStore for PostgresProcessRegistry {
    async fn put_segment_handover(
        &self,
        process_id: &str,
        handover: PersistedSegmentHandover,
    ) -> Result<(), PluginError> {
        let encoded = serde_json::to_string(&handover).map_err(process_decode_error)?;
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let result = sqlx::query(
            "INSERT INTO lash_process_segment_handovers
             (process_id, segment_ordinal, handover_json) VALUES ($1, $2, $3)
             ON CONFLICT (process_id, segment_ordinal) DO UPDATE
             SET handover_json = EXCLUDED.handover_json
             WHERE lash_process_segment_handovers.handover_json = EXCLUDED.handover_json",
        )
        .bind(process_id)
        .bind(handover.segment_ordinal as i64)
        .bind(encoded)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        if result.rows_affected() == 0 {
            return Err(PluginError::Session(format!(
                "process `{process_id}` segment {} handover conflict",
                handover.segment_ordinal
            )));
        }
        sqlx::query(
            "DELETE FROM lash_process_segment_handovers
             WHERE process_id = $1 AND segment_ordinal < $2 - 1",
        )
        .bind(process_id)
        .bind(handover.segment_ordinal as i64)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(())
    }

    async fn get_segment_handover(
        &self,
        process_id: &str,
        segment_ordinal: u64,
    ) -> Result<Option<PersistedSegmentHandover>, PluginError> {
        let json: Option<String> = sqlx::query_scalar(
            "SELECT handover_json FROM lash_process_segment_handovers
             WHERE process_id = $1 AND segment_ordinal = $2",
        )
        .bind(process_id)
        .bind(segment_ordinal as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        json.map(|json| serde_json::from_str(&json).map_err(process_decode_error))
            .transpose()
    }

    async fn latest_segment_handover(
        &self,
        process_id: &str,
    ) -> Result<Option<PersistedSegmentHandover>, PluginError> {
        let json: Option<String> = sqlx::query_scalar(
            "SELECT handover_json FROM lash_process_segment_handovers
             WHERE process_id = $1 ORDER BY segment_ordinal DESC LIMIT 1",
        )
        .bind(process_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        json.map(|json| serde_json::from_str(&json).map_err(process_decode_error))
            .transpose()
    }

    async fn delete_segment_handovers(&self, process_id: &str) -> Result<(), PluginError> {
        sqlx::query("DELETE FROM lash_process_segment_handovers WHERE process_id = $1")
            .bind(process_id)
            .execute(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        Ok(())
    }
}

fn process_external_ref_conflict(
    process_id: &str,
    existing: &ProcessExternalRef,
    new: &ProcessExternalRef,
) -> PluginError {
    PluginError::Session(format!(
        "process `{process_id}` external ref conflict: existing {existing:?}, new {new:?}"
    ))
}
