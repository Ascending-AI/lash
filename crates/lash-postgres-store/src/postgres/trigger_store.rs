use crate::*;

#[async_trait::async_trait]
impl TriggerStore for PostgresTriggerStore {
    async fn execute_command(
        &self,
        operation_id: &str,
        command: lash_core::TriggerCommand,
    ) -> Result<lash_core::TriggerEffectResult, PluginError> {
        if let lash_core::TriggerCommand::List {
            owner_scope,
            mut filter,
        } = command
        {
            filter.registrant_scope_id = Some(owner_scope.namespace());
            return self
                .list_subscriptions(filter)
                .await
                .map(|records| Ok(lash_core::TriggerCommandOutcome::List { records }));
        }

        let request_fingerprint = lash_core::facade_support::trigger_command_fingerprint(&command);
        let receipt_owner_scope = command.owner_scope().clone();
        let receipt_id = lash_core::facade_support::trigger_operation_receipt_id(
            command.owner_scope(),
            operation_id,
        );
        let subscription_key = command.subscription_key().unwrap_or_default().to_string();
        let subscription_id = lash_core::facade_support::deterministic_subscription_id(
            command.owner_scope(),
            &subscription_key,
        );
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(&subscription_id)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;

        let stored = sqlx::query(
            "SELECT request_fingerprint, result_json FROM lash_trigger_mutation_receipts
             WHERE operation_id = $1",
        )
        .bind(&receipt_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        if let Some(row) = stored {
            let stored_hash: String = row.get(0);
            let result_json: String = row.get(1);
            tx.commit().await.map_err(plugin_sqlx_error)?;
            if stored_hash != request_fingerprint {
                return Ok(Err(lash_core::TriggerOperationError::Conflict {
                    subscription_key,
                    existing_revision: None,
                    existing_definition_fingerprint: Some(stored_hash),
                    requested_definition_fingerprint: Some(request_fingerprint),
                    reason: format!(
                        "operation id `{operation_id}` was reused with different content"
                    ),
                }));
            }
            return serde_json::from_str(&result_json).map_err(process_decode_error);
        }

        let now = self.clock.timestamp_ms();
        let result = if let lash_core::TriggerCommand::Prune {
            owner_scope,
            actor,
            subscription_keys,
        } = &command
        {
            let rows = sqlx::query(
                "SELECT record_json FROM lash_trigger_subscriptions
                 WHERE owner_scope = $1 AND tombstoned = FALSE FOR UPDATE",
            )
            .bind(owner_scope.namespace())
            .fetch_all(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
            let records = rows
                .into_iter()
                .map(|row| {
                    let json: String = row.get(0);
                    serde_json::from_str(&json).map_err(process_decode_error)
                })
                .collect::<Result<Vec<_>, _>>()?;
            lash_core::facade_support::evaluate_trigger_prune(
                records,
                owner_scope.clone(),
                actor.clone(),
                subscription_keys.clone(),
                now,
            )
        } else {
            let current_json: Option<String> = sqlx::query_scalar(
                "SELECT record_json FROM lash_trigger_subscriptions
                 WHERE subscription_id = $1 FOR UPDATE",
            )
            .bind(&subscription_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
            let current = current_json
                .map(|json| serde_json::from_str(&json).map_err(process_decode_error))
                .transpose()?;
            if let Some(incarnation) = &self.fixed_incarnation {
                lash_core::facade_support::evaluate_trigger_mutation_with_incarnation(
                    current,
                    command,
                    now,
                    incarnation.clone(),
                )?
            } else {
                lash_core::facade_support::evaluate_trigger_mutation(current, command, now)?
            }
        };
        let records = match &result {
            Ok(lash_core::TriggerCommandOutcome::Mutation { receipt }) => {
                vec![&receipt.record_snapshot]
            }
            Ok(lash_core::TriggerCommandOutcome::Prune { receipts }) => receipts
                .iter()
                .map(|receipt| &receipt.record_snapshot)
                .collect(),
            Ok(lash_core::TriggerCommandOutcome::List { .. }) | Err(_) => Vec::new(),
        };
        for record in records {
            let sql_revision =
                plugin_sql_counter_value("trigger_subscription_revision", record.revision)?;
            sqlx::query(
                "INSERT INTO lash_trigger_subscriptions (
                    subscription_id, owner_scope, subscription_key, incarnation, revision,
                    definition_fingerprint, source_type, source_key, enabled, tombstoned,
                    created_at_ms, updated_at_ms, record_json
                 ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                 ON CONFLICT (subscription_id) DO UPDATE SET
                    owner_scope = EXCLUDED.owner_scope,
                    subscription_key = EXCLUDED.subscription_key,
                    incarnation = EXCLUDED.incarnation,
                    revision = EXCLUDED.revision,
                    definition_fingerprint = EXCLUDED.definition_fingerprint,
                    source_type = EXCLUDED.source_type,
                    source_key = EXCLUDED.source_key,
                    enabled = EXCLUDED.enabled,
                    tombstoned = EXCLUDED.tombstoned,
                    updated_at_ms = EXCLUDED.updated_at_ms,
                    record_json = EXCLUDED.record_json",
            )
            .bind(&record.subscription_id)
            .bind(record.owner_scope.namespace())
            .bind(&record.subscription_key)
            .bind(&record.incarnation)
            .bind(sql_revision)
            .bind(&record.definition_fingerprint)
            .bind(&record.source_type)
            .bind(&record.source_key)
            .bind(record.enabled)
            .bind(record.tombstoned)
            .bind(record.created_at_ms as i64)
            .bind(record.updated_at_ms as i64)
            .bind(serde_json::to_string(record).map_err(process_decode_error)?)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        }
        sqlx::query(
            "INSERT INTO lash_trigger_mutation_receipts (
                operation_id, request_fingerprint, result_json, created_at_ms
             ) VALUES ($1, $2, $3, $4)",
        )
        .bind(&receipt_id)
        .bind(&request_fingerprint)
        .bind(
            lash_core::facade_support::encode_trigger_effect_result_receipt(
                &receipt_owner_scope,
                &result,
            )?,
        )
        .bind(now as i64)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(result)
    }

    async fn list_subscriptions(
        &self,
        filter: TriggerSubscriptionFilter,
    ) -> Result<Vec<TriggerSubscriptionRecord>, PluginError> {
        let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
            "SELECT subscription_id, record_json FROM lash_trigger_subscriptions
             WHERE tombstoned = FALSE",
        );
        if let Some(owner_scope) = filter.effective_registrant_scope_id() {
            query.push(" AND owner_scope = ").push_bind(owner_scope);
        }
        if let Some(subscription_key) = filter.subscription_key.as_ref() {
            query
                .push(" AND subscription_key = ")
                .push_bind(subscription_key);
        }
        if let Some(source_type) = filter.source_type.as_ref() {
            query.push(" AND source_type = ").push_bind(source_type);
        }
        if let Some(source_key) = filter.source_key.as_ref() {
            query.push(" AND source_key = ").push_bind(source_key);
        }
        if let Some(enabled) = filter.enabled {
            query.push(" AND enabled = ").push_bind(enabled);
        }
        query.push(" ORDER BY owner_scope ASC, subscription_key ASC");
        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        let mut records = Vec::new();
        for row in rows {
            let subscription_id: String = row.get(0);
            let json: String = row.get(1);
            match serde_json::from_str(&json) {
                Ok(record) if filter.matches(&record) => records.push(record),
                Ok(_) => {}
                Err(err) => tracing::warn!(
                    error = %err,
                    subscription_id,
                    "skipping malformed trigger subscription during listing"
                ),
            }
        }
        Ok(records)
    }

    async fn delete_session_subscriptions(&self, session_id: &str) -> Result<usize, PluginError> {
        let owner_scope = lash_core::TriggerOwnerScope::session(session_id).namespace();
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let rows = sqlx::query(
            "SELECT subscription_id, record_json FROM lash_trigger_subscriptions
             WHERE owner_scope = $1 AND tombstoned = FALSE FOR UPDATE",
        )
        .bind(&owner_scope)
        .fetch_all(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        let now = self.clock.timestamp_ms();
        for row in &rows {
            let subscription_id: String = row.get(0);
            let json: String = row.get(1);
            let mut record: TriggerSubscriptionRecord =
                serde_json::from_str(&json).map_err(process_decode_error)?;
            let next_revision = lash_core::facade_support::next_trigger_store_revision(&record)?;
            record.enabled = false;
            record.tombstoned = true;
            record.deleted_at_ms = Some(now);
            record.revision = next_revision;
            record.updated_at_ms = now;
            let sql_revision =
                plugin_sql_counter_value("trigger_subscription_revision", record.revision)?;
            sqlx::query(
                "UPDATE lash_trigger_subscriptions
                 SET enabled = FALSE, tombstoned = TRUE, revision = $2,
                     updated_at_ms = $3, record_json = $4
                 WHERE subscription_id = $1",
            )
            .bind(subscription_id)
            .bind(sql_revision)
            .bind(now as i64)
            .bind(serde_json::to_string(&record).map_err(process_decode_error)?)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(rows.len())
    }

    async fn ingest_occurrence(
        &self,
        request: TriggerOccurrenceRequest,
    ) -> Result<lash_core::TriggerIngressReceipt, PluginError> {
        lash_core::facade_support::validate_trigger_occurrence_request(&request)?;
        let occurrence_id = lash_core::facade_support::deterministic_occurrence_id(&request);
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(&request.idempotency_key)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        let existing = sqlx::query(
            "SELECT record_json FROM lash_trigger_occurrences
             WHERE idempotency_key = $1 FOR UPDATE",
        )
        .bind(&request.idempotency_key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        let (occurrence, is_new) = if let Some(row) = existing {
            let json: String = row.get(0);
            let occurrence: TriggerOccurrenceRecord =
                serde_json::from_str(&json).map_err(process_decode_error)?;
            if !lash_core::facade_support::trigger_occurrence_request_matches_record(
                &request,
                &occurrence,
            ) {
                return Err(PluginError::Session(format!(
                    "trigger occurrence idempotency conflict for `{}`",
                    request.idempotency_key
                )));
            }
            (occurrence, false)
        } else {
            let occurrence = TriggerOccurrenceRecord {
                occurrence_id,
                source_type: request.source_type,
                source_key: request.source_key,
                payload: request.payload,
                idempotency_key: request.idempotency_key,
                source: request.source,
                session_id: request.session_id,
                outcome: request.outcome,
                occurred_at_ms: self.clock.timestamp_ms(),
            };
            sqlx::query(
                "INSERT INTO lash_trigger_occurrences (
                    occurrence_id, idempotency_key, source_type, source_key,
                    occurred_at_ms, record_json
                 ) VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(&occurrence.occurrence_id)
            .bind(&occurrence.idempotency_key)
            .bind(&occurrence.source_type)
            .bind(&occurrence.source_key)
            .bind(occurrence.occurred_at_ms as i64)
            .bind(serde_json::to_string(&occurrence).map_err(process_decode_error)?)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
            (occurrence, true)
        };
        let reservations = match (
            is_new,
            occurrence.outcome == lash_core::TriggerOccurrenceOutcome::Fired,
        ) {
            (true, true) => {
                reserve_postgres_deliveries(&mut tx, &occurrence, self.clock.timestamp_ms()).await?
            }
            (false, true) => postgres_delivery_snapshots(&mut tx, &occurrence).await?,
            (_, false) => Vec::new(),
        };
        if is_new
            && occurrence.outcome == lash_core::TriggerOccurrenceOutcome::Fired
            && reservations.is_empty()
        {
            sqlx::query(
                "UPDATE lash_trigger_occurrences
                 SET reclaimable_at_ms = $2
                 WHERE occurrence_id = $1 AND reclaimable_at_ms IS NULL",
            )
            .bind(&occurrence.occurrence_id)
            .bind(i64::try_from(occurrence.occurred_at_ms).unwrap_or(i64::MAX))
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lash_core::TriggerIngressReceipt {
            occurrence,
            reservations,
        })
    }

    async fn list_occurrences(
        &self,
        filter: lash_core::TriggerOccurrenceFilter,
    ) -> Result<Vec<TriggerOccurrenceRecord>, PluginError> {
        let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
            "SELECT occurrence_id, record_json FROM lash_trigger_occurrences WHERE TRUE",
        );
        if let Some(source_type) = filter.source_type.as_ref() {
            query.push(" AND source_type = ").push_bind(source_type);
        }
        if let Some(source_key) = filter.source_key.as_ref() {
            query.push(" AND source_key = ").push_bind(source_key);
        }
        if let Some(start_ms) = filter.occurred_at_start_ms {
            query
                .push(" AND occurred_at_ms >= ")
                .push_bind(clamp_epoch_ms(start_ms));
        }
        if let Some(end_ms) = filter.occurred_at_end_ms {
            query
                .push(" AND occurred_at_ms < ")
                .push_bind(clamp_epoch_ms(end_ms));
        }
        query.push(" ORDER BY occurred_at_ms ASC, occurrence_id ASC");
        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        rows.into_iter()
            .map(|row| {
                let json: String = row.get(1);
                serde_json::from_str(&json).map_err(process_decode_error)
            })
            .collect()
    }

    async fn list_deliveries_by_occurrence_id(
        &self,
        occurrence_id: &str,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        list_deliveries_where(
            &self.pool,
            "d.occurrence_id = $1",
            Some(occurrence_id.to_string()),
        )
        .await
    }

    async fn list_deliveries_by_subscription_id(
        &self,
        subscription_id: &str,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        list_deliveries_where(
            &self.pool,
            "d.subscription_id = $1",
            Some(subscription_id.to_string()),
        )
        .await
    }

    async fn list_deliveries_by_process_id(
        &self,
        process_id: &str,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        list_deliveries_where(
            &self.pool,
            "d.process_id = $1",
            Some(process_id.to_string()),
        )
        .await
    }

    async fn list_deliveries(&self) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        list_deliveries_where(&self.pool, "TRUE", None).await
    }

    async fn list_delivery_process_ids(&self) -> Result<Vec<String>, PluginError> {
        sqlx::query_scalar(
            "SELECT DISTINCT process_id
             FROM lash_trigger_deliveries
             ORDER BY process_id ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(plugin_sqlx_error)
    }

    async fn list_delivery_retention_candidates(
        &self,
    ) -> Result<Vec<lash_core::TriggerDeliveryRetentionCandidate>, PluginError> {
        let rows = sqlx::query(
            "SELECT occurrence_id, subscription_id, process_id
             FROM lash_trigger_deliveries
             ORDER BY occurrence_id ASC, subscription_id ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        Ok(rows
            .into_iter()
            .map(|row| lash_core::TriggerDeliveryRetentionCandidate {
                occurrence_id: row.get(0),
                subscription_id: row.get(1),
                process_id: row.get(2),
            })
            .collect())
    }

    async fn list_session_owner_ids_for_retention(&self) -> Result<Vec<String>, PluginError> {
        let owner_scopes: Vec<String> = sqlx::query_scalar(
            "SELECT owner_scope
             FROM (
                 SELECT owner_scope FROM lash_trigger_subscriptions
                 UNION
                 SELECT 'session:' ||
                        (subscription_snapshot_json::jsonb #>> '{owner_scope,session_id}')
                 FROM lash_trigger_deliveries
                 WHERE subscription_snapshot_json::jsonb #>> '{owner_scope,type}' = 'session'
                 UNION
                 SELECT COALESCE(
                     result_json::jsonb #>> '{Ok,_owner_scope_namespace}',
                     result_json::jsonb #>> '{Err,_owner_scope_namespace}',
                     CASE
                         WHEN result_json::jsonb #>> '{Ok,receipt,owner_scope,type}' = 'session'
                         THEN 'session:' ||
                              (result_json::jsonb #>> '{Ok,receipt,owner_scope,session_id}')
                     END,
                     CASE
                         WHEN result_json::jsonb #>> '{Ok,receipts,0,owner_scope,type}' = 'session'
                         THEN 'session:' ||
                              (result_json::jsonb #>> '{Ok,receipts,0,owner_scope,session_id}')
                     END
                 ) AS owner_scope
                 FROM lash_trigger_mutation_receipts
             ) AS trigger_owner_scopes
             WHERE owner_scope LIKE 'session:%'
             ORDER BY owner_scope",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        let mut session_ids = std::collections::BTreeSet::new();
        for owner_scope in owner_scopes {
            if let Some(session_id) = owner_scope.strip_prefix("session:") {
                session_ids.insert(session_id.to_string());
            }
        }
        Ok(session_ids.into_iter().collect())
    }

    async fn reconcile_trigger_retention(
        &self,
        candidates: &[lash_core::TriggerDeliveryRetentionCandidate],
        deleted_session_ids: &[String],
    ) -> Result<lash_core::TriggerRetentionReconciliationReport, PluginError> {
        let occurrence_ids = candidates
            .iter()
            .map(|candidate| candidate.occurrence_id.clone())
            .collect::<Vec<_>>();
        let subscription_ids = candidates
            .iter()
            .map(|candidate| candidate.subscription_id.clone())
            .collect::<Vec<_>>();
        let process_ids = candidates
            .iter()
            .map(|candidate| candidate.process_id.clone())
            .collect::<Vec<_>>();
        let deleted_owner_scopes = deleted_session_ids
            .iter()
            .map(|session_id| lash_core::TriggerOwnerScope::session(session_id).namespace())
            .collect::<Vec<_>>();
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;

        let reclaimed_delivery_count = if candidates.is_empty() {
            0
        } else {
            sqlx::query(
                "DELETE FROM lash_trigger_deliveries AS delivery
                 USING UNNEST($1::TEXT[], $2::TEXT[], $3::TEXT[])
                       AS candidate(occurrence_id, subscription_id, process_id)
                 WHERE delivery.occurrence_id = candidate.occurrence_id
                   AND delivery.subscription_id = candidate.subscription_id
                   AND delivery.process_id = candidate.process_id",
            )
            .bind(occurrence_ids)
            .bind(subscription_ids)
            .bind(process_ids)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected() as usize
        };
        let reclaimed_occurrence_count = sqlx::query(
            "DELETE FROM lash_trigger_occurrences AS occurrence
             WHERE COALESCE(
                       occurrence.record_json::jsonb #>> '{outcome,kind}',
                       'fired'
                   ) = 'fired'
               AND NOT EXISTS (
                 SELECT 1 FROM lash_trigger_deliveries AS delivery
                 WHERE delivery.occurrence_id = occurrence.occurrence_id
             )",
        )
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?
        .rows_affected() as usize;

        let blocked_owner_scopes: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT
                    'session:' ||
                    (subscription_snapshot_json::jsonb #>> '{owner_scope,session_id}')
             FROM lash_trigger_deliveries
             WHERE subscription_snapshot_json::jsonb #>> '{owner_scope,type}' = 'session'",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        let blocked_owner_scopes = blocked_owner_scopes
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        let receipt_owner_scopes = deleted_owner_scopes
            .iter()
            .filter(|owner_scope| !blocked_owner_scopes.contains(*owner_scope))
            .cloned()
            .collect::<Vec<_>>();

        let reclaimed_subscription_count = if deleted_owner_scopes.is_empty() {
            0
        } else {
            sqlx::query(
                "DELETE FROM lash_trigger_subscriptions AS subscription
                 WHERE subscription.owner_scope = ANY($1::TEXT[])
                   AND NOT EXISTS (
                       SELECT 1 FROM lash_trigger_deliveries AS delivery
                       WHERE delivery.subscription_id = subscription.subscription_id
                   )",
            )
            .bind(&deleted_owner_scopes)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected() as usize
        };
        let reclaimed_mutation_receipt_count = if receipt_owner_scopes.is_empty() {
            0
        } else {
            sqlx::query(
                "DELETE FROM lash_trigger_mutation_receipts
                 WHERE COALESCE(
                     result_json::jsonb #>> '{Ok,_owner_scope_namespace}',
                     result_json::jsonb #>> '{Err,_owner_scope_namespace}',
                     CASE
                         WHEN result_json::jsonb #>> '{Ok,receipt,owner_scope,type}' = 'session'
                         THEN 'session:' ||
                              (result_json::jsonb #>> '{Ok,receipt,owner_scope,session_id}')
                     END,
                     CASE
                         WHEN result_json::jsonb #>> '{Ok,receipts,0,owner_scope,type}' = 'session'
                         THEN 'session:' ||
                              (result_json::jsonb #>> '{Ok,receipts,0,owner_scope,session_id}')
                     END
                 ) = ANY($1::TEXT[])",
            )
            .bind(&receipt_owner_scopes)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected() as usize
        };

        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lash_core::TriggerRetentionReconciliationReport {
            reclaimed_delivery_count,
            reclaimed_occurrence_count,
            reclaimed_subscription_count,
            reclaimed_mutation_receipt_count,
        })
    }

    async fn delete_delivery_retention_candidates(
        &self,
        candidates: &[lash_core::TriggerDeliveryRetentionCandidate],
    ) -> Result<usize, PluginError> {
        if candidates.is_empty() {
            return Ok(0);
        }
        let occurrence_ids = candidates
            .iter()
            .map(|candidate| candidate.occurrence_id.clone())
            .collect::<Vec<_>>();
        let subscription_ids = candidates
            .iter()
            .map(|candidate| candidate.subscription_id.clone())
            .collect::<Vec<_>>();
        let process_ids = candidates
            .iter()
            .map(|candidate| candidate.process_id.clone())
            .collect::<Vec<_>>();
        let armed_at_ms = i64::try_from(self.clock.timestamp_ms()).unwrap_or(i64::MAX);
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let deleted = sqlx::query(
            "DELETE FROM lash_trigger_deliveries AS delivery
                 USING UNNEST($1::TEXT[], $2::TEXT[], $3::TEXT[])
                       AS candidate(occurrence_id, subscription_id, process_id)
                 WHERE delivery.occurrence_id = candidate.occurrence_id
                   AND delivery.subscription_id = candidate.subscription_id
                   AND delivery.process_id = candidate.process_id",
        )
        .bind(occurrence_ids)
        .bind(subscription_ids)
        .bind(process_ids)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?
        .rows_affected() as usize;
        sqlx::query(
            "UPDATE lash_trigger_occurrences AS occurrence
             SET reclaimable_at_ms = $2
             WHERE occurrence.reclaimable_at_ms IS NULL
               AND COALESCE(
                       occurrence.record_json::jsonb #>> '{outcome,kind}',
                       'fired'
                   ) = 'fired'
               AND occurrence.occurrence_id = ANY($1::TEXT[])
               AND NOT EXISTS (
                   SELECT 1 FROM lash_trigger_deliveries AS delivery
                   WHERE delivery.occurrence_id = occurrence.occurrence_id
               )",
        )
        .bind(
            candidates
                .iter()
                .map(|candidate| candidate.occurrence_id.clone())
                .collect::<Vec<_>>(),
        )
        .bind(armed_at_ms)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(deleted)
    }

    async fn reclaim_trigger_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> lash_core::TriggerOccurrenceReclamationResult {
        let cutoff_epoch_ms = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        // One statement gives the scope proof and the indexed worklist from the
        // same snapshot. The aggregate deliberately visits the whole table so
        // `NothingToDo` remains witnessed emptiness; only eligible ids are
        // materialized, through the partial reclaimability index.
        let rows = sqlx::query(
            "WITH scope AS (
                 SELECT COUNT(*) AS inspected_count,
                        COUNT(*) FILTER (
                            WHERE reclaimable_at_ms IS NULL
                               OR COALESCE(
                                      record_json::jsonb #>> '{outcome,kind}',
                                      'fired'
                                  ) != 'fired'
                        )
                            AS live_fan_out_count,
                        COUNT(*) FILTER (
                            WHERE reclaimable_at_ms > $1
                              AND COALESCE(
                                      record_json::jsonb #>> '{outcome,kind}',
                                      'fired'
                                  ) = 'fired'
                        )
                            AS grace_deferred_count
                 FROM lash_trigger_occurrences
             ), candidates AS (
                 SELECT occurrence_id
                 FROM lash_trigger_occurrences
                 WHERE reclaimable_at_ms IS NOT NULL
                   AND reclaimable_at_ms <= $1
                   AND COALESCE(
                           record_json::jsonb #>> '{outcome,kind}',
                           'fired'
                       ) = 'fired'
             )
             SELECT scope.inspected_count,
                    scope.live_fan_out_count,
                    scope.grace_deferred_count,
                    candidates.occurrence_id
             FROM scope
             LEFT JOIN candidates ON TRUE
             ORDER BY candidates.occurrence_id ASC",
        )
        .bind(cutoff_epoch_ms)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| {
            lash_core::MaintenanceFailure::failed_before_any_work(Box::new(plugin_sqlx_error(
                error,
            )))
        })?;
        let first = &rows[0];
        let mut report = lash_core::TriggerOccurrenceReclamationReport {
            inspected_occurrence_count: first.get::<i64, _>(0) as usize,
            live_fan_out_count: first.get::<i64, _>(1) as usize,
            grace_deferred_count: first.get::<i64, _>(2) as usize,
            ..lash_core::TriggerOccurrenceReclamationReport::default()
        };
        let candidates = rows
            .into_iter()
            .filter_map(|row| row.get::<Option<String>, _>(3))
            .collect::<Vec<_>>();

        for occurrence_id in candidates {
            let deleted = sqlx::query(
                "DELETE FROM lash_trigger_occurrences AS occurrence
                 WHERE occurrence.occurrence_id = $1
                   AND occurrence.reclaimable_at_ms IS NOT NULL
                   AND occurrence.reclaimable_at_ms <= $2
                   AND COALESCE(
                           occurrence.record_json::jsonb #>> '{outcome,kind}',
                           'fired'
                       ) = 'fired'
                   AND NOT EXISTS (
                       SELECT 1 FROM lash_trigger_deliveries AS delivery
                       WHERE delivery.occurrence_id = occurrence.occurrence_id
                   )",
            )
            .bind(&occurrence_id)
            .bind(cutoff_epoch_ms)
            .execute(&self.pool)
            .await
            .map_err(|error| {
                lash_core::MaintenanceFailure::failed(
                    Box::new(plugin_sqlx_error(error)),
                    report.clone(),
                )
            })?
            .rows_affected() as usize;
            if deleted == 0 {
                report.reinspection_deferred_count += 1;
            } else {
                report.reclaimed_occurrence_count += deleted;
            }
        }
        Ok(report)
    }

    async fn prune_mutation_receipts(&self, cutoff_epoch_ms: u64) -> Result<usize, PluginError> {
        let cutoff_epoch_ms = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        Ok(sqlx::query(
            "WITH classified_receipts AS (
                     SELECT operation_id,
                            COALESCE(
                                result_json::jsonb #>> '{Ok,_owner_scope_namespace}',
                                result_json::jsonb #>> '{Err,_owner_scope_namespace}',
                                CASE result_json::jsonb #>> '{Ok,receipt,owner_scope,type}'
                                    WHEN 'session' THEN 'session:' ||
                                        (result_json::jsonb #>>
                                            '{Ok,receipt,owner_scope,session_id}')
                                    WHEN 'host' THEN 'host:' ||
                                        (result_json::jsonb #>>
                                            '{Ok,receipt,owner_scope,binding_id}')
                                    WHEN 'platform' THEN 'host'
                                END,
                                CASE result_json::jsonb #>> '{Ok,receipts,0,owner_scope,type}'
                                    WHEN 'session' THEN 'session:' ||
                                        (result_json::jsonb #>>
                                            '{Ok,receipts,0,owner_scope,session_id}')
                                    WHEN 'host' THEN 'host:' ||
                                        (result_json::jsonb #>>
                                            '{Ok,receipts,0,owner_scope,binding_id}')
                                    WHEN 'platform' THEN 'host'
                                END
                            ) AS owner_scope
                     FROM lash_trigger_mutation_receipts
                     WHERE created_at_ms < $1
                 )
                 DELETE FROM lash_trigger_mutation_receipts AS receipt
                 USING classified_receipts
                 WHERE receipt.operation_id = classified_receipts.operation_id
                   AND (
                       classified_receipts.owner_scope = 'host'
                       OR (
                           left(classified_receipts.owner_scope, 5) = 'host:'
                           AND length(classified_receipts.owner_scope) > 5
                       )
                   )",
        )
        .bind(cutoff_epoch_ms)
        .execute(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?
        .rows_affected() as usize)
    }
}

async fn reserve_postgres_deliveries(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    occurrence: &TriggerOccurrenceRecord,
    created_at_ms: u64,
) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "SELECT subscription_id, record_json FROM lash_trigger_subscriptions
         WHERE enabled = TRUE AND tombstoned = FALSE AND source_type = ",
    );
    query
        .push_bind(&occurrence.source_type)
        .push(" AND source_key = ")
        .push_bind(&occurrence.source_key);
    if let Some(session_id) = occurrence.session_id.as_deref() {
        query
            .push(" AND owner_scope = ")
            .push_bind(lash_core::TriggerOwnerScope::session(session_id).namespace());
    }
    query.push(" ORDER BY owner_scope ASC, subscription_key ASC FOR SHARE");
    let rows = query
        .build()
        .fetch_all(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
    let mut reservations = Vec::new();
    for row in rows {
        let subscription_id: String = row.get(0);
        let json: String = row.get(1);
        let subscription: TriggerSubscriptionRecord = match serde_json::from_str(&json) {
            Ok(subscription) => subscription,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    subscription_id,
                    "skipping malformed trigger subscription during occurrence ingress"
                );
                continue;
            }
        };
        let process_id = lash_core::facade_support::deterministic_delivery_process_id(
            &occurrence.occurrence_id,
            &subscription.subscription_id,
            &subscription.incarnation,
            subscription.revision,
        )?;
        let sql_revision =
            plugin_sql_counter_value("trigger_subscription_revision", subscription.revision)?;
        sqlx::query(
            "INSERT INTO lash_trigger_deliveries (
                occurrence_id, subscription_id, process_id, subscription_incarnation,
                subscription_revision, subscription_snapshot_json, created_at_ms
             ) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&occurrence.occurrence_id)
        .bind(&subscription.subscription_id)
        .bind(&process_id)
        .bind(&subscription.incarnation)
        .bind(sql_revision)
        .bind(serde_json::to_string(&subscription).map_err(process_decode_error)?)
        .bind(created_at_ms as i64)
        .execute(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
        reservations.push(TriggerDeliveryReservation {
            occurrence: occurrence.clone(),
            subscription,
            process_id,
            created_at_ms,
            reservation_status: lash_core::TriggerDeliveryReservationOutcome::Reserved,
        });
    }
    lash_core::facade_support::sort_trigger_delivery_reservations(&mut reservations);
    Ok(reservations)
}

async fn postgres_delivery_snapshots(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    occurrence: &TriggerOccurrenceRecord,
) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
    let rows = sqlx::query(
        "SELECT process_id, created_at_ms, subscription_snapshot_json
         FROM lash_trigger_deliveries WHERE occurrence_id = $1",
    )
    .bind(&occurrence.occurrence_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?;
    let mut reservations = rows
        .into_iter()
        .map(|row| {
            let json: String = row.get(2);
            Ok(TriggerDeliveryReservation {
                occurrence: occurrence.clone(),
                subscription: serde_json::from_str(&json).map_err(process_decode_error)?,
                process_id: row.get(0),
                created_at_ms: plugin_u64_from_sql("TriggerDelivery", "created_at_ms", row.get(1))?,
                reservation_status: lash_core::TriggerDeliveryReservationOutcome::AlreadyReserved,
            })
        })
        .collect::<Result<Vec<_>, PluginError>>()?;
    lash_core::facade_support::sort_trigger_delivery_reservations(&mut reservations);
    Ok(reservations)
}

async fn list_deliveries_where(
    pool: &sqlx::PgPool,
    where_clause: &'static str,
    value: Option<String>,
) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
    let sql = format!(
        "SELECT d.process_id, d.created_at_ms, o.record_json,
                d.subscription_snapshot_json
         FROM lash_trigger_deliveries d
         JOIN lash_trigger_occurrences o ON o.occurrence_id = d.occurrence_id
         WHERE {where_clause}
         ORDER BY d.created_at_ms ASC, d.occurrence_id ASC, d.subscription_id ASC"
    );
    let mut query = sqlx::query(&sql);
    if let Some(value) = value {
        query = query.bind(value);
    }
    let rows = query.fetch_all(pool).await.map_err(plugin_sqlx_error)?;
    rows.into_iter()
        .map(|row| {
            let occurrence_json: String = row.get(2);
            let subscription_json: String = row.get(3);
            Ok(TriggerDeliveryReservation {
                occurrence: serde_json::from_str(&occurrence_json).map_err(process_decode_error)?,
                subscription: serde_json::from_str(&subscription_json)
                    .map_err(process_decode_error)?,
                process_id: row.get(0),
                created_at_ms: plugin_u64_from_sql("TriggerDelivery", "created_at_ms", row.get(1))?,
                reservation_status: lash_core::TriggerDeliveryReservationOutcome::AlreadyReserved,
            })
        })
        .collect()
}
