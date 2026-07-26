use crate::*;

#[async_trait::async_trait]
impl ProcessRegistry for PostgresProcessRegistry {
    fn durability_tier(&self) -> DurabilityTier {
        DurabilityTier::Durable
    }

    async fn register_process(
        &self,
        registration: ProcessRegistration,
    ) -> Result<ProcessRecord, PluginError> {
        let (registration, registration_hash) =
            lash_core::runtime::prepare_process_registration(registration)?;
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        if let Some(existing) = load_process_tx(&mut tx, &registration.id).await? {
            if existing.registration_hash == registration_hash {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(existing);
            }
            return Err(PluginError::Session(format!(
                "process `{}` registration hash conflict: existing {}, new {}",
                registration.id, existing.registration_hash, registration_hash
            )));
        }
        let now = current_epoch_ms();
        let record =
            ProcessRecord::from_prepared_registration(registration, registration_hash, now);
        let record_json = serde_json::to_string(&record).map_err(process_decode_error)?;
        let change_seq = next_process_change_seq_tx(&mut tx).await?;
        sqlx::query(
            "INSERT INTO lash_processes (
                process_id, registration_hash, owner_scope_id,
                created_at_ms, updated_at_ms, change_seq, status, record_json
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&record.id)
        .bind(&record.registration_hash)
        .bind(record.originator_scope_id().as_str())
        .bind(record.created_at_ms as i64)
        .bind(record.updated_at_ms as i64)
        .bind(change_seq as i64)
        .bind(process_status_label(&record))
        .bind(record_json)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(record)
    }

    async fn put_segment_handover(
        &self,
        process_id: &str,
        handover: PersistedSegmentHandover,
    ) -> Result<(), PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        if load_process_tx(&mut tx, process_id).await?.is_none() {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        }
        let encoded = serde_json::to_string(&handover).map_err(process_decode_error)?;
        let existing = sqlx::query(
            "SELECT handover_json FROM lash_process_segment_handovers
             WHERE process_id = $1 AND segment_ordinal = $2 FOR UPDATE",
        )
        .bind(process_id)
        .bind(handover.segment_ordinal as i64)
        .fetch_optional(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        if let Some(existing) = existing {
            let existing: String = existing.get("handover_json");
            if existing != encoded {
                return Err(PluginError::Session(format!(
                    "process `{process_id}` segment {} handover conflict",
                    handover.segment_ordinal
                )));
            }
        } else {
            sqlx::query(
                "DELETE FROM lash_process_segment_handovers
                 WHERE process_id = $1 AND segment_ordinal < $2 - 1",
            )
            .bind(process_id)
            .bind(handover.segment_ordinal as i64)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
            sqlx::query(
                "INSERT INTO lash_process_segment_handovers
                 (process_id, segment_ordinal, handover_json) VALUES ($1, $2, $3)",
            )
            .bind(process_id)
            .bind(handover.segment_ordinal as i64)
            .bind(encoded)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(())
    }

    async fn get_segment_handover(
        &self,
        process_id: &str,
        segment_ordinal: u64,
    ) -> Result<Option<PersistedSegmentHandover>, PluginError> {
        let row = sqlx::query(
            "SELECT handover_json FROM lash_process_segment_handovers
             WHERE process_id = $1 AND segment_ordinal = $2",
        )
        .bind(process_id)
        .bind(segment_ordinal as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        row.map(|row| {
            serde_json::from_str(row.get::<String, _>("handover_json").as_str())
                .map_err(process_decode_error)
        })
        .transpose()
    }

    async fn latest_segment_handover(
        &self,
        process_id: &str,
    ) -> Result<Option<PersistedSegmentHandover>, PluginError> {
        let row = sqlx::query(
            "SELECT handover_json FROM lash_process_segment_handovers
             WHERE process_id = $1 ORDER BY segment_ordinal DESC LIMIT 1",
        )
        .bind(process_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        row.map(|row| {
            serde_json::from_str(row.get::<String, _>("handover_json").as_str())
                .map_err(process_decode_error)
        })
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

    async fn set_external_ref(
        &self,
        process_id: &str,
        external_ref: ProcessExternalRef,
    ) -> Result<ProcessRecord, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = load_process_tx(&mut tx, process_id)
            .await?
            .ok_or_else(|| PluginError::Session(format!("unknown process `{process_id}`")))?;
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
        append_process_event_tx(&mut tx, &mut record, request, current_epoch_ms()).await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(record)
    }

    async fn grant_handle(
        &self,
        session_scope: &SessionScope,
        process_id: &str,
        descriptor: ProcessHandleDescriptor,
    ) -> Result<ProcessHandleGrant, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        if load_process_tx(&mut tx, process_id).await?.is_none() {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        }
        sqlx::query(
            "INSERT INTO lash_process_handle_grants (session_id, scope_id, process_id, descriptor_json)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (scope_id, process_id) DO UPDATE SET
                session_id = EXCLUDED.session_id,
                descriptor_json = EXCLUDED.descriptor_json",
        )
        .bind(&session_scope.session_id)
        .bind(session_scope.id().as_str())
        .bind(process_id)
        .bind(serde_json::to_string(&descriptor).map_err(process_decode_error)?)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(ProcessHandleGrant {
            session_id: session_scope.session_id.clone(),
            process_id: process_id.to_string(),
            descriptor,
        })
    }

    async fn revoke_handle(
        &self,
        session_scope: &SessionScope,
        process_id: &str,
    ) -> Result<(), PluginError> {
        sqlx::query(
            "DELETE FROM lash_process_handle_grants WHERE scope_id = $1 AND process_id = $2",
        )
        .bind(session_scope.id().as_str())
        .bind(process_id)
        .execute(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        Ok(())
    }

    async fn transfer_handle_grants(
        &self,
        from_scope: &SessionScope,
        to_scope: &SessionScope,
        process_ids: &[String],
    ) -> Result<(), PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        for process_id in process_ids {
            let descriptor_json: Option<String> = sqlx::query_scalar(
                "SELECT descriptor_json FROM lash_process_handle_grants
                 WHERE scope_id = $1 AND process_id = $2",
            )
            .bind(from_scope.id().as_str())
            .bind(process_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
            let Some(descriptor_json) = descriptor_json else {
                return Err(PluginError::Session(format!(
                    "process handle `{process_id}` is not granted to session `{}`",
                    from_scope.session_id
                )));
            };
            sqlx::query(
                "DELETE FROM lash_process_handle_grants WHERE scope_id = $1 AND process_id = $2",
            )
            .bind(from_scope.id().as_str())
            .bind(process_id)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
            sqlx::query(
                "INSERT INTO lash_process_handle_grants (session_id, scope_id, process_id, descriptor_json)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (scope_id, process_id) DO UPDATE SET
                    session_id = EXCLUDED.session_id,
                    descriptor_json = EXCLUDED.descriptor_json",
            )
            .bind(&to_scope.session_id)
            .bind(to_scope.id().as_str())
            .bind(process_id)
            .bind(descriptor_json)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)
    }

    async fn list_handle_grants(
        &self,
        session_scope: &SessionScope,
    ) -> Result<Vec<ProcessHandleGrantEntry>, PluginError> {
        list_grants_for_scope(&self.pool, session_scope, false).await
    }

    async fn list_live_handle_grants(
        &self,
        session_scope: &SessionScope,
    ) -> Result<Vec<ProcessHandleGrantEntry>, PluginError> {
        list_grants_for_scope(&self.pool, session_scope, true).await
    }

    async fn has_handle_grant(
        &self,
        session_scope: &SessionScope,
        process_id: &str,
    ) -> Result<bool, PluginError> {
        let exists: Option<i64> = sqlx::query_scalar(
            "SELECT 1::BIGINT FROM lash_process_handle_grants
             WHERE scope_id = $1 AND process_id = $2
             LIMIT 1",
        )
        .bind(session_scope.id().as_str())
        .bind(process_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        Ok(exists.is_some())
    }

    async fn handle_grants_for_process(
        &self,
        process_id: &str,
    ) -> Result<Vec<ProcessHandleGrant>, PluginError> {
        if load_process(&self.pool, process_id).await?.is_none() {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        }
        let rows = sqlx::query(
            "SELECT session_id, descriptor_json
             FROM lash_process_handle_grants
             WHERE process_id = $1
             ORDER BY session_id ASC, scope_id ASC",
        )
        .bind(process_id)
        .fetch_all(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        let mut grants = Vec::new();
        for row in rows {
            let descriptor_json: String = row.get(1);
            grants.push(ProcessHandleGrant {
                session_id: row.get(0),
                process_id: process_id.to_string(),
                descriptor: serde_json::from_str(&descriptor_json).map_err(process_decode_error)?,
            });
        }
        Ok(grants)
    }

    async fn delete_session_process_state(
        &self,
        session_id: &str,
    ) -> Result<lash_core::ProcessSessionDeleteReport, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let rows = sqlx::query(
            "SELECT g.process_id, p.record_json
             FROM lash_process_handle_grants g
             JOIN lash_processes p ON p.process_id = g.process_id
             WHERE g.session_id = $1
             ORDER BY g.process_id ASC",
        )
        .bind(session_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        let mut removed = Vec::new();
        for row in rows {
            let process_id: String = row.get(0);
            let record_json: String = row.get(1);
            let record: ProcessRecord =
                serde_json::from_str(&record_json).map_err(process_decode_error)?;
            removed.push((process_id, record));
        }
        // Wake acknowledgements are process-scoped consumed-event markers. Session
        // deletion removes materialized session-addressed deliveries through the
        // session store; clearing these rows would re-expose already-consumed wakes
        // to surviving grants or future host readers.
        let deleted_wake_count = 0;
        let revoked = sqlx::query("DELETE FROM lash_process_handle_grants WHERE session_id = $1")
            .bind(session_id)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected() as usize;
        let mut orphaned_process_ids = Vec::new();
        let mut preserved_process_ids = Vec::new();
        for (process_id, record) in removed {
            if record.is_terminal() {
                continue;
            }
            let remaining: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM lash_process_handle_grants WHERE process_id = $1",
            )
            .bind(&process_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
            if remaining == 0 {
                orphaned_process_ids.push(process_id);
            } else {
                preserved_process_ids.push(process_id);
            }
        }
        let rows = sqlx::query(
            "SELECT process_id, record_json
             FROM lash_processes
             ORDER BY process_id ASC
             FOR UPDATE",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        for row in rows {
            let record_json: String = row.get(1);
            let mut record: ProcessRecord =
                serde_json::from_str(&record_json).map_err(process_decode_error)?;
            if record.clear_wake_target_for_session(session_id) {
                save_process_tx(&mut tx, &record).await?;
            }
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        orphaned_process_ids.sort();
        orphaned_process_ids.dedup();
        preserved_process_ids.sort();
        preserved_process_ids.dedup();
        Ok(lash_core::ProcessSessionDeleteReport {
            session_id: session_id.to_string(),
            revoked_handle_count: revoked,
            deleted_wake_count,
            orphaned_process_ids,
            preserved_process_ids,
        })
    }

    async fn append_event(
        &self,
        process_id: &str,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendResult, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = load_process_tx(&mut tx, process_id)
            .await?
            .ok_or_else(|| PluginError::Session(format!("unknown process `{process_id}`")))?;
        let occurred_at_ms = current_epoch_ms();
        let result =
            append_process_event_tx(&mut tx, &mut record, request, occurred_at_ms).await?;
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
        let mut record = load_process_tx(&mut tx, process_id)
            .await?
            .ok_or_else(|| PluginError::Session(format!("unknown process `{process_id}`")))?;
        let now_ms = process_lease_now_epoch_ms_tx(&mut tx).await?;
        validate_process_execution_authority_tx(
            &mut tx,
            process_id,
            &record,
            authority,
            None,
            now_ms,
        )
        .await?;
        let occurred_at_ms = current_epoch_ms();
        let result =
            append_process_event_tx(&mut tx, &mut record, request, occurred_at_ms).await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(result)
    }

    async fn events_after(
        &self,
        process_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<ProcessEvent>, PluginError> {
        if load_process(&self.pool, process_id).await?.is_none() {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
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
        if load_process(&self.pool, process_id).await?.is_none() {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
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
        if load_process(&self.pool, process_id).await?.is_none() {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
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

    async fn wake_events_after(
        &self,
        process_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<ProcessEvent>, PluginError> {
        let rows = sqlx::query("SELECT sequence FROM lash_process_wake_acks WHERE process_id = $1")
            .bind(process_id)
            .fetch_all(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        let acked = rows
            .into_iter()
            .map(|row| row.get::<i64, _>(0) as u64)
            .collect::<HashSet<_>>();
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
        let mut record = load_process_tx(&mut tx, process_id)
            .await?
            .ok_or_else(|| PluginError::Session(format!("unknown process `{process_id}`")))?;
        if record.is_terminal() {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(lash_core::ProcessCompletionOutcome::from_stored(
                record,
                &await_output,
            ));
        }
        authority.validate(process_id, record.disposition, &await_output)?;
        let request =
            lash_core::terminal_append_request(process_id, &await_output, Some(&authority));
        let replay_lookup =
            if let Some(replay_key) = request.replay.as_ref().map(|r| r.key.as_str()) {
                load_event_by_key_tx(&mut tx, process_id, replay_key).await?
            } else {
                None
            };
        let sequence: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM lash_process_events WHERE process_id = $1",
        )
        .bind(process_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        let occurred_at_ms = current_epoch_ms();
        let prepared = lash_core::runtime::prepare_process_event_append(
            &record,
            request,
            sequence as u64,
            replay_lookup,
            occurred_at_ms,
        )?;
        match prepared {
            lash_core::ProcessEventAppendPlan::Replay {
                repair_record,
                ..
            } => {
                if let Some(repaired) = repair_record {
                    record = repaired;
                    save_process_tx(&mut tx, &record).await?;
                }
                tx.commit().await.map_err(plugin_sqlx_error)?;
                Ok(lash_core::ProcessCompletionOutcome::AlreadyApplied {
                    stored: record,
                })
            }
            lash_core::ProcessEventAppendPlan::Insert {
                event,
                payload_hash,
                projected_record,
                occurred_at_ms,
                ..
            } => {
                sqlx::query(
                    "INSERT INTO lash_process_events (
                        process_id, sequence, event_type, payload_hash, idempotency_key,
                        occurred_at_ms, event_json
                     )
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(process_id)
                .bind(sequence)
                .bind(event.event_type.as_str())
                .bind(&payload_hash)
                .bind(event.invocation.replay_key())
                .bind(occurred_at_ms as i64)
                .bind(serde_json::to_string(&event).map_err(process_decode_error)?)
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?;
                record = projected_record;
                save_process_tx(&mut tx, &record).await?;
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
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let process_id = lease.process_id.as_str();
        let mut record = load_process_tx(&mut tx, process_id)
            .await?
            .ok_or_else(|| PluginError::Session(format!("unknown process `{process_id}`")))?;
        if record.is_terminal() {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(lash_core::ProcessCompletionOutcome::from_stored(
                record,
                &await_output,
            ));
        }
        let request = lash_core::terminal_append_request(process_id, &await_output, None);
        let replay_lookup =
            if let Some(replay_key) = request.replay.as_ref().map(|r| r.key.as_str()) {
                load_event_by_key_tx(&mut tx, process_id, replay_key).await?
            } else {
                None
            };
        let sequence: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM lash_process_events WHERE process_id = $1",
        )
        .bind(process_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        let prepared = lash_core::runtime::prepare_process_event_append(
            &record,
            request,
            sequence as u64,
            replay_lookup,
            now,
        )?;
        if matches!(prepared, lash_core::ProcessEventAppendPlan::Replay { .. }) {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(lash_core::ProcessCompletionOutcome::AlreadyApplied {
                stored: record,
            });
        }

        let current = load_process_lease_tx(&mut tx, process_id).await?;
        if !guard_lease(current.as_ref(), &lease.lease_token, now)
            || !current.as_ref().is_some_and(|current| {
                current.owner.same_incarnation(&lease.owner)
                    && current.fencing_token == lease.fencing_token
            })
        {
            return Err(process_lease_expired(process_id));
        }

        let lash_core::ProcessEventAppendPlan::Insert {
            event,
            payload_hash,
            projected_record,
            occurred_at_ms,
            ..
        } = prepared
        else {
            unreachable!("replay returned above")
        };
        sqlx::query(
            "INSERT INTO lash_process_events (
                process_id, sequence, event_type, payload_hash, idempotency_key,
                occurred_at_ms, event_json
             ) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(process_id)
        .bind(sequence)
        .bind(event.event_type.as_str())
        .bind(&payload_hash)
        .bind(event.invocation.replay_key())
        .bind(occurred_at_ms as i64)
        .bind(serde_json::to_string(&event).map_err(process_decode_error)?)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        record = projected_record;
        save_process_tx(&mut tx, &record).await?;
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
            return Err(process_lease_expired(process_id));
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lash_core::ProcessCompletionOutcome::Committed(record))
    }

    async fn record_first_started_with_authority(
        &self,
        process_id: &str,
        started: ProcessStarted,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessStartOutcome, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = load_process_tx(&mut tx, process_id)
            .await?
            .ok_or_else(|| PluginError::Session(format!("unknown process `{process_id}`")))?;
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
        append_process_event_tx(&mut tx, &mut record, request, now).await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(ProcessStartOutcome::Started(record))
    }

    async fn request_process_abandon(
        &self,
        process_id: &str,
        request: AbandonRequest,
    ) -> Result<ProcessRecord, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = load_process_tx(&mut tx, process_id)
            .await?
            .ok_or_else(|| PluginError::Session(format!("unknown process `{process_id}`")))?;
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
        append_process_event_tx(&mut tx, &mut record, append, current_epoch_ms()).await?;
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
        let mut record = load_process_tx(&mut tx, process_id)
            .await?
            .ok_or_else(|| PluginError::Session(format!("unknown process `{process_id}`")))?;
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        validate_process_execution_authority_tx(
            &mut tx,
            process_id,
            &record,
            authority,
            None,
            now,
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
        append_process_event_tx(&mut tx, &mut record, request, now).await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(record)
    }

    async fn clear_process_wait_with_authority(
        &self,
        process_id: &str,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let mut record = load_process_tx(&mut tx, process_id)
            .await?
            .ok_or_else(|| PluginError::Session(format!("unknown process `{process_id}`")))?;
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        validate_process_execution_authority_tx(
            &mut tx,
            process_id,
            &record,
            authority,
            None,
            now,
        )
        .await?;
        let Some(wait) = record.wait.clone() else {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(record);
        };
        let request = ProcessEventAppendRequest::wait_cleared(process_id, &wait);
        append_process_event_tx(&mut tx, &mut record, request, now).await?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(record)
    }

    async fn get_process(&self, process_id: &str) -> Option<ProcessRecord> {
        load_process(&self.pool, process_id).await.ok().flatten()
    }

    async fn try_get_process(
        &self,
        process_id: &str,
    ) -> Result<Option<ProcessRecord>, PluginError> {
        load_process(&self.pool, process_id).await
    }

    async fn list_processes(
        &self,
        filter: &lash_core::ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        let rows = sqlx::query(
            "SELECT record_json FROM lash_processes
             ORDER BY process_id ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        let mut records = Vec::new();
        for row in rows {
            let json: String = row.get(0);
            let record: ProcessRecord =
                serde_json::from_str(&json).map_err(process_decode_error)?;
            if filter.matches_record(&record) {
                records.push(record);
            }
        }
        Ok(records)
    }

    async fn processes_changed_since(
        &self,
        cursor: ProcessChangeCursor,
        limit: usize,
    ) -> Result<(Vec<ProcessRecord>, ProcessChangeCursor), PluginError> {
        if limit == 0 {
            return Ok((Vec::new(), cursor));
        }
        let rows = sqlx::query(
            "SELECT change_seq, record_json FROM lash_processes
             WHERE change_seq > $1
             ORDER BY change_seq ASC, process_id ASC
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
            let json: String = row.get(1);
            records.push(serde_json::from_str(&json).map_err(process_decode_error)?);
            next_cursor = ProcessChangeCursor::from_store_sequence(change_seq as u64);
        }
        Ok((records, next_cursor))
    }

    async fn ack_wake(&self, process_id: &str, sequence: u64) -> Result<(), PluginError> {
        if load_process(&self.pool, process_id).await?.is_none() {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        }
        sqlx::query(
            "INSERT INTO lash_process_wake_acks (process_id, sequence)
             VALUES ($1, $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(process_id)
        .bind(sequence as i64)
        .execute(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        Ok(())
    }

    async fn list_non_terminal(&self) -> Result<Vec<ProcessRecord>, PluginError> {
        let rows = sqlx::query(
            "SELECT record_json FROM lash_processes
             WHERE status = 'running'
             ORDER BY process_id ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        let mut records = Vec::new();
        for row in rows {
            let json: String = row.get(0);
            records.push(serde_json::from_str(&json).map_err(process_decode_error)?);
        }
        Ok(records)
    }

    async fn filter_unregistered_process_ids(
        &self,
        process_ids: &[String],
    ) -> Result<Vec<String>, PluginError> {
        if process_ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_scalar(
            "SELECT candidate.process_id
             FROM UNNEST($1::TEXT[]) WITH ORDINALITY AS candidate(process_id, ordinal)
             WHERE NOT EXISTS (
                 SELECT 1 FROM lash_processes p
                 WHERE p.process_id = candidate.process_id
             )
             ORDER BY candidate.ordinal ASC",
        )
        .bind(process_ids)
        .fetch_all(&self.pool)
        .await
        .map_err(plugin_sqlx_error)
    }

    async fn live_reference_summary(
        &self,
    ) -> Result<Vec<ProcessLiveReferenceSummary>, PluginError> {
        let records = self.list_non_terminal().await?;
        Ok(ProcessLiveReferenceSummary::from_records(records.iter()))
    }

    async fn claim_process_lease(
        &self,
        process_id: &str,
        owner: &LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::ProcessLeaseClaimOutcome, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        if load_process_tx(&mut tx, process_id).await?.is_none() {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        }
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        let current = load_process_lease_tx(&mut tx, process_id).await?;
        if let Some(current) = current.as_ref()
            && current.expires_at_epoch_ms > now
        {
            if current.owner.same_incarnation(owner) {
                // Same incarnation re-enters its own live lease: extend the
                // expiry, keep token and fencing token.
                let expires_at = now.saturating_add(lease_ttl_ms);
                sqlx::query(
                    "UPDATE lash_process_leases
                     SET lease_expires_at_ms = $2
                     WHERE process_id = $1",
                )
                .bind(process_id)
                .bind(expires_at as i64)
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?;
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(lash_core::ProcessLeaseClaimOutcome::Acquired(
                    ProcessLease {
                        expires_at_epoch_ms: expires_at,
                        ..current.clone()
                    },
                ));
            }
            let holder = current.clone();
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(lash_core::ProcessLeaseClaimOutcome::Busy { holder });
        }
        let fencing_token = retained_process_lease_fencing_token(&mut tx, process_id).await? + 1;
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
        observed_holder: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::ProcessLeaseClaimOutcome, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        if load_process_tx(&mut tx, process_id).await?.is_none() {
            return Err(PluginError::Session(format!(
                "unknown process `{process_id}`"
            )));
        }
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        let current = load_process_lease_tx(&mut tx, process_id).await?;
        let Some(current) = current else {
            // Free (or released) lease: acquire on the retained fencing token
            // like a plain claim would.
            let fencing_token =
                retained_process_lease_fencing_token(&mut tx, process_id).await? + 1;
            let lease = acquire_process_lease_tx(
                &mut tx,
                process_id,
                owner,
                fencing_token,
                now,
                lease_ttl_ms,
            )
            .await?;
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(lash_core::ProcessLeaseClaimOutcome::Acquired(lease));
        };
        if current.expires_at_epoch_ms <= now {
            let lease = acquire_process_lease_tx(
                &mut tx,
                process_id,
                owner,
                current.fencing_token.saturating_add(1),
                now,
                lease_ttl_ms,
            )
            .await?;
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(lash_core::ProcessLeaseClaimOutcome::Acquired(lease));
        }
        // Fenced CAS on the observed holder: identity, token, and fencing token
        // must all still match, and the holder must be definitely dead for this
        // claimant.
        if observed_holder.process_id == process_id
            && current.owner.same_incarnation(&observed_holder.owner)
            && current.lease_token == observed_holder.lease_token
            && current.fencing_token == observed_holder.fencing_token
            && current.owner.is_definitely_dead_for_claimant(owner)
        {
            let fencing_token = current.fencing_token.saturating_add(1);
            let lease_token = format!(
                "{:x}",
                Sha256::digest(
                    format!(
                        "{process_id}:{}:{}:{now}:{fencing_token}",
                        owner.owner_id, owner.incarnation_id
                    )
                    .as_bytes()
                )
            );
            let expires_at = now.saturating_add(lease_ttl_ms);
            let changed = sqlx::query(
                "UPDATE lash_process_leases
                 SET lease_owner_id = $1,
                     lease_owner_incarnation_id = $2,
                     lease_owner_liveness_json = $3,
                     lease_token = $4,
                     lease_fencing_token = $5,
                     lease_claimed_at_ms = $6,
                     lease_expires_at_ms = $7
                 WHERE process_id = $8
                   AND lease_owner_id = $9
                   AND lease_owner_incarnation_id = $10
                   AND lease_token = $11
                   AND lease_fencing_token = $12",
            )
            .bind(&owner.owner_id)
            .bind(&owner.incarnation_id)
            .bind(encode_process_lease_liveness(&owner.liveness)?)
            .bind(&lease_token)
            .bind(fencing_token as i64)
            .bind(now as i64)
            .bind(expires_at as i64)
            .bind(process_id)
            .bind(&observed_holder.owner.owner_id)
            .bind(&observed_holder.owner.incarnation_id)
            .bind(&observed_holder.lease_token)
            .bind(observed_holder.fencing_token as i64)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected();
            if changed == 1 {
                let lease = ProcessLease {
                    schema_version: PROCESS_LEASE_SCHEMA_VERSION,
                    process_id: process_id.to_string(),
                    owner: owner.clone(),
                    lease_token,
                    fencing_token,
                    claimed_at_epoch_ms: now,
                    expires_at_epoch_ms: expires_at,
                };
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(lash_core::ProcessLeaseClaimOutcome::Acquired(lease));
            }
            // Lost the CAS race: re-read and report the winner.
            if let Some(current) = load_process_lease_tx(&mut tx, process_id).await?
                && current.expires_at_epoch_ms > now
            {
                tx.commit().await.map_err(plugin_sqlx_error)?;
                return Ok(lash_core::ProcessLeaseClaimOutcome::Busy { holder: current });
            }
            return Err(process_lease_expired(process_id));
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lash_core::ProcessLeaseClaimOutcome::Busy { holder: current })
    }

    async fn renew_process_lease(
        &self,
        lease: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLease, PluginError> {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let now = process_lease_now_epoch_ms_tx(&mut tx).await?;
        let current = load_process_lease_tx(&mut tx, &lease.process_id).await?;
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
        up_to_change_seq: Option<ProcessChangeCursor>,
    ) -> Result<ProcessPruneReport, PluginError> {
        let cutoff = cutoff_epoch_ms as i64;
        let max_change_seq = up_to_change_seq.map(|cursor| cursor.store_sequence() as i64);
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let rows = sqlx::query(
            "SELECT process_id, record_json FROM lash_processes
             WHERE status != 'running'
               AND updated_at_ms < $1
               AND ($2::BIGINT IS NULL OR change_seq <= $2)
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

        let mut pruned_events = 0;
        let mut pruned_processes = 0;
        for process_id in prunable {
            for session_id in lash_core::process_runtime_session_ids(&process_id) {
                delete_session_tx(&mut tx, &session_id)
                    .await
                    .map_err(|err| PluginError::Session(err.to_string()))?;
            }
            pruned_events += sqlx::query("DELETE FROM lash_process_events WHERE process_id = $1")
                .bind(&process_id)
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?
                .rows_affected() as usize;
            sqlx::query("DELETE FROM lash_process_wake_acks WHERE process_id = $1")
                .bind(&process_id)
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?;
            sqlx::query("DELETE FROM lash_process_handle_grants WHERE process_id = $1")
                .bind(&process_id)
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?;
            sqlx::query("DELETE FROM lash_process_leases WHERE process_id = $1")
                .bind(&process_id)
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?;
            sqlx::query("DELETE FROM lash_trigger_deliveries WHERE process_id = $1")
                .bind(&process_id)
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?;
            pruned_processes += sqlx::query("DELETE FROM lash_processes WHERE process_id = $1")
                .bind(&process_id)
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?
                .rows_affected() as usize;
        }
        sqlx::query("DELETE FROM lash_trigger_mutation_receipts WHERE created_at_ms < $1")
            .bind(cutoff)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(ProcessPruneReport {
            pruned_processes,
            pruned_events,
        })
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
