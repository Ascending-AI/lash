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
        let receipt_id = lash_core::facade_support::trigger_operation_receipt_id(
            command.owner_scope(),
            operation_id,
        )?;
        let subscription_key = command.subscription_key().unwrap_or_default().to_string();
        let subscription_id = lash_core::facade_support::deterministic_subscription_id(
            command.owner_scope(),
            &subscription_key,
        )?;
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

        let now = current_epoch_ms();
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
            lash_core::facade_support::evaluate_trigger_mutation(current, command, now)?
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
            .bind(record.revision as i64)
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
        .bind(serde_json::to_string(&result).map_err(process_decode_error)?)
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
        let now = current_epoch_ms();
        for row in &rows {
            let subscription_id: String = row.get(0);
            let json: String = row.get(1);
            let mut record: TriggerSubscriptionRecord =
                serde_json::from_str(&json).map_err(process_decode_error)?;
            record.enabled = false;
            record.tombstoned = true;
            record.deleted_at_ms = Some(now);
            record.revision = record.revision.saturating_add(1);
            record.updated_at_ms = now;
            sqlx::query(
                "UPDATE lash_trigger_subscriptions
                 SET enabled = FALSE, tombstoned = TRUE, revision = $2,
                     updated_at_ms = $3, record_json = $4
                 WHERE subscription_id = $1",
            )
            .bind(subscription_id)
            .bind(record.revision as i64)
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
    ) -> Result<lash_core::TriggerIngressResult, PluginError> {
        lash_core::facade_support::validate_trigger_occurrence_request(&request)?;
        let request_hash = lash_core::facade_support::trigger_occurrence_request_hash(&request)?;
        let occurrence_id = lash_core::facade_support::deterministic_occurrence_id(&request)?;
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(&request.idempotency_key)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        let existing = sqlx::query(
            "SELECT request_hash, record_json FROM lash_trigger_occurrences
             WHERE idempotency_key = $1 FOR UPDATE",
        )
        .bind(&request.idempotency_key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        let (occurrence, is_new) = if let Some(row) = existing {
            let existing_hash: String = row.get(0);
            if existing_hash != request_hash {
                return Err(PluginError::Session(format!(
                    "trigger occurrence idempotency conflict for `{}`",
                    request.idempotency_key
                )));
            }
            let json: String = row.get(1);
            (
                serde_json::from_str(&json).map_err(process_decode_error)?,
                false,
            )
        } else {
            let occurrence = TriggerOccurrenceRecord {
                occurrence_id,
                source_type: request.source_type,
                source_key: request.source_key,
                payload: request.payload,
                idempotency_key: request.idempotency_key,
                source: request.source,
                session_id: request.session_id,
                occurred_at_ms: current_epoch_ms(),
            };
            sqlx::query(
                "INSERT INTO lash_trigger_occurrences (
                    occurrence_id, idempotency_key, request_hash, source_type, source_key,
                    occurred_at_ms, record_json
                 ) VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(&occurrence.occurrence_id)
            .bind(&occurrence.idempotency_key)
            .bind(&request_hash)
            .bind(&occurrence.source_type)
            .bind(&occurrence.source_key)
            .bind(occurrence.occurred_at_ms as i64)
            .bind(serde_json::to_string(&occurrence).map_err(process_decode_error)?)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
            (occurrence, true)
        };
        let reservations = if is_new {
            reserve_postgres_deliveries(&mut tx, &occurrence).await?
        } else {
            postgres_delivery_snapshots(&mut tx, &occurrence).await?
        };
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lash_core::TriggerIngressResult {
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
                .push_bind(start_ms as i64);
        }
        if let Some(end_ms) = filter.occurred_at_end_ms {
            query
                .push(" AND occurred_at_ms < ")
                .push_bind(end_ms as i64);
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
        Ok(sqlx::query(
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
        .execute(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?
        .rows_affected() as usize)
    }

    async fn prune_mutation_receipts(&self, cutoff_epoch_ms: u64) -> Result<usize, PluginError> {
        let cutoff_epoch_ms = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        Ok(
            sqlx::query("DELETE FROM lash_trigger_mutation_receipts WHERE created_at_ms < $1")
                .bind(cutoff_epoch_ms)
                .execute(&self.pool)
                .await
                .map_err(plugin_sqlx_error)?
                .rows_affected() as usize,
        )
    }
}

async fn reserve_postgres_deliveries(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    occurrence: &TriggerOccurrenceRecord,
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
    let created_at_ms = current_epoch_ms();
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
        .bind(subscription.revision as i64)
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
            reservation_status: lash_core::TriggerDeliveryReservationStatus::Reserved,
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
                created_at_ms: row.get::<i64, _>(1) as u64,
                reservation_status: lash_core::TriggerDeliveryReservationStatus::AlreadyReserved,
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
                created_at_ms: row.get::<i64, _>(1) as u64,
                reservation_status: lash_core::TriggerDeliveryReservationStatus::AlreadyReserved,
            })
        })
        .collect()
}
