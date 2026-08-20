//! SQLite-backed runtime trigger store.
//!
//! This is the durable peer of [`SqliteProcessRegistry`]: it stores trigger
//! subscriptions and append-only trigger occurrences at deployment scope,
//! outside any session database.

use super::*;

pub struct SqliteTriggerStore {
    conn: SqliteConnection,
    clock: Arc<dyn lash_core::Clock>,
    fixed_incarnation: Option<String>,
}

impl SqliteTriggerStore {
    pub async fn open(path: &Path) -> tokio_rusqlite::Result<Self> {
        Self::open_with_clock(path, Arc::new(lash_core::facade_support::SystemClock)).await
    }

    pub async fn open_with_clock(
        path: &Path,
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        let conn = SqliteConnection::open(path).await?;
        ensure_trigger_schema(&conn).await?;
        apply_pragmas(&conn, StoreBacking::File).await?;
        Ok(Self {
            conn,
            clock,
            fixed_incarnation: None,
        })
    }

    pub async fn memory() -> tokio_rusqlite::Result<Self> {
        Self::memory_with_clock(Arc::new(lash_core::facade_support::SystemClock)).await
    }

    pub async fn memory_with_clock(
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        let conn = SqliteConnection::open_in_memory().await?;
        ensure_trigger_schema(&conn).await?;
        apply_pragmas(&conn, StoreBacking::Memory).await?;
        Ok(Self {
            conn,
            clock,
            fixed_incarnation: None,
        })
    }

    /// Pin otherwise-random trigger incarnation identity for durable fixture generation.
    #[doc(hidden)]
    pub fn with_incarnation_for_testing(mut self, incarnation: impl Into<String>) -> Self {
        self.fixed_incarnation = Some(incarnation.into());
        self
    }

    fn encode_json<T: serde::Serialize>(value: &T) -> Result<String, lash_core::PluginError> {
        serde_json::to_string(value).map_err(|err| {
            lash_core::PluginError::Session(format!("failed to encode trigger row: {err}"))
        })
    }

    fn decode_subscription(
        json: String,
    ) -> Result<lash_core::TriggerSubscriptionRecord, lash_core::PluginError> {
        serde_json::from_str(&json).map_err(|err| {
            lash_core::PluginError::Session(format!(
                "failed to decode trigger subscription row: {err}"
            ))
        })
    }

    fn decode_occurrence(
        json: String,
    ) -> Result<lash_core::TriggerOccurrenceRecord, lash_core::PluginError> {
        serde_json::from_str(&json).map_err(|err| {
            lash_core::PluginError::Session(format!(
                "failed to decode trigger occurrence row: {err}"
            ))
        })
    }

    fn decode_delivery(
        occurrence_json: String,
        subscription_json: String,
        process_id: String,
        created_at_ms: i64,
        reservation_status: lash_core::TriggerDeliveryReservationOutcome,
    ) -> Result<lash_core::TriggerDeliveryReservation, lash_core::PluginError> {
        Ok(lash_core::TriggerDeliveryReservation {
            occurrence: Self::decode_occurrence(occurrence_json)?,
            subscription: Self::decode_subscription(subscription_json)?,
            process_id,
            created_at_ms: plugin_u64_from_sql("TriggerDelivery", "created_at_ms", created_at_ms)?,
            reservation_status,
        })
    }

    async fn list_deliveries_where(
        &self,
        where_clause: &'static str,
        values: Vec<rusqlite::types::Value>,
    ) -> Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError> {
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let sql = format!(
                        "SELECT d.process_id, d.created_at_ms, o.record_json,
                                d.subscription_snapshot_json
                         FROM trigger_deliveries d
                         JOIN trigger_occurrences o ON o.occurrence_id = d.occurrence_id
                         WHERE {where_clause}
                         ORDER BY d.created_at_ms ASC, d.occurrence_id ASC, d.subscription_id ASC"
                    );
                    let mut stmt = conn.prepare(&sql).map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                            ))
                        })
                        .map_err(process_sqlite_error)?;
                    let mut deliveries = Vec::new();
                    for row in rows {
                        let (process_id, created_at_ms, occurrence_json, subscription_json) =
                            row.map_err(process_sqlite_error)?;
                        deliveries.push(Self::decode_delivery(
                            occurrence_json,
                            subscription_json,
                            process_id,
                            created_at_ms,
                            lash_core::TriggerDeliveryReservationOutcome::AlreadyReserved,
                        )?);
                    }
                    Ok(deliveries)
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }
}

fn trigger_tx_outcome<T>(
    result: Result<T, lash_core::PluginError>,
) -> TxOutcome<Result<T, lash_core::PluginError>> {
    match result {
        Ok(value) => TxOutcome::Commit(Ok(value)),
        Err(err) => TxOutcome::Rollback(Err(err)),
    }
}

#[async_trait::async_trait]
impl lash_core::TriggerStore for SqliteTriggerStore {
    async fn execute_command(
        &self,
        operation_id: &str,
        command: lash_core::TriggerCommand,
    ) -> Result<lash_core::TriggerEffectResult, lash_core::PluginError> {
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
        let public_operation_id = operation_id.to_string();
        let operation_id = lash_core::facade_support::trigger_operation_receipt_id(
            command.owner_scope(),
            operation_id,
        );
        let request_fingerprint = lash_core::facade_support::trigger_command_fingerprint(&command);
        let fixed_incarnation = self.fixed_incarnation.clone();
        let owner_scope = command.owner_scope().clone();
        let subscription_key = command.subscription_key().unwrap_or_default().to_string();
        let subscription_id = lash_core::facade_support::deterministic_subscription_id(
            &owner_scope,
            &subscription_key,
        );
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                Ok(trigger_tx_outcome((|| {
                    let receipt: Option<(String, String)> = tx
                        .query_row(
                            "SELECT request_fingerprint, result_json
                             FROM trigger_mutation_receipts WHERE operation_id = ?1",
                            params![operation_id.as_str()],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()
                        .map_err(process_sqlite_error)?;
                    if let Some((stored_hash, json)) = receipt {
                        if stored_hash != request_fingerprint {
                            return Ok(Err(lash_core::TriggerOperationError::Conflict {
                                subscription_key,
                                existing_revision: None,
                                existing_definition_fingerprint: Some(stored_hash),
                                requested_definition_fingerprint: Some(request_fingerprint),
                                reason: format!(
                                    "operation id `{public_operation_id}` was reused with different content"
                                ),
                            }));
                        }
                        return serde_json::from_str(&json).map_err(|err| {
                            lash_core::PluginError::Session(format!(
                                "failed to decode trigger mutation receipt: {err}"
                            ))
                        });
                    }
                    let result = if let lash_core::TriggerCommand::Prune {
                        owner_scope,
                        actor,
                        subscription_keys,
                    } = &command
                    {
                        let mut stmt = tx
                            .prepare(
                                "SELECT record_json FROM trigger_subscriptions
                                 WHERE owner_scope = ?1 AND tombstoned = 0",
                            )
                            .map_err(process_sqlite_error)?;
                        let rows = stmt
                            .query_map(params![owner_scope.namespace()], |row| {
                                row.get::<_, String>(0)
                            })
                            .map_err(process_sqlite_error)?;
                        let mut records = Vec::new();
                        for row in rows {
                            records.push(Self::decode_subscription(
                                row.map_err(process_sqlite_error)?,
                            )?);
                        }
                        drop(stmt);
                        lash_core::facade_support::evaluate_trigger_prune(
                            records,
                            owner_scope.clone(),
                            actor.clone(),
                            subscription_keys.clone(),
                            now,
                        )
                    } else {
                        let current = tx
                            .query_row(
                                "SELECT record_json FROM trigger_subscriptions
                                 WHERE subscription_id = ?1",
                                params![subscription_id.as_str()],
                                |row| row.get::<_, String>(0),
                            )
                            .optional()
                            .map_err(process_sqlite_error)?
                            .map(Self::decode_subscription)
                            .transpose()?;
                        if let Some(incarnation) = fixed_incarnation {
                            lash_core::facade_support::evaluate_trigger_mutation_with_incarnation(
                                current,
                                command,
                                now,
                                incarnation,
                            )?
                        } else {
                            lash_core::facade_support::evaluate_trigger_mutation(
                                current, command, now,
                            )?
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
                        let sql_revision = plugin_sql_counter_value(
                            "trigger_subscription_revision",
                            record.revision,
                        )?;
                        tx.execute(
                            "INSERT INTO trigger_subscriptions (
                            subscription_id, owner_scope, subscription_key, incarnation, revision,
                            definition_fingerprint, source_type, source_key, enabled, tombstoned,
                            created_at_ms, updated_at_ms, record_json
                         )
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                         ON CONFLICT(subscription_id) DO UPDATE SET
                            owner_scope = excluded.owner_scope,
                            subscription_key = excluded.subscription_key,
                            incarnation = excluded.incarnation,
                            revision = excluded.revision,
                            definition_fingerprint = excluded.definition_fingerprint,
                            source_type = excluded.source_type,
                            source_key = excluded.source_key,
                            enabled = excluded.enabled,
                            tombstoned = excluded.tombstoned,
                            updated_at_ms = excluded.updated_at_ms,
                            record_json = excluded.record_json",
                            params![
                                record.subscription_id.as_str(),
                                record.owner_scope.namespace(),
                                record.subscription_key.as_str(),
                                record.incarnation.as_str(),
                                sql_revision,
                                record.definition_fingerprint.as_str(),
                                record.source_type.as_str(),
                                record.source_key.as_str(),
                                i64::from(record.enabled),
                                i64::from(record.tombstoned),
                                record.created_at_ms as i64,
                                record.updated_at_ms as i64,
                                Self::encode_json(&record)?,
                            ],
                        )
                        .map_err(process_sqlite_error)?;
                    }
                    tx.execute(
                        "INSERT INTO trigger_mutation_receipts (
                            operation_id, request_fingerprint, result_json, created_at_ms
                         ) VALUES (?1, ?2, ?3, ?4)",
                        params![
                            operation_id.as_str(),
                            request_fingerprint.as_str(),
                            Self::encode_json(&result)?,
                            now as i64,
                        ],
                    )
                    .map_err(process_sqlite_error)?;
                    Ok(result)
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn list_subscriptions(
        &self,
        filter: lash_core::TriggerSubscriptionFilter,
    ) -> Result<Vec<lash_core::TriggerSubscriptionRecord>, lash_core::PluginError> {
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let mut sql =
                        "SELECT subscription_id, record_json FROM trigger_subscriptions WHERE 1 = 1"
                            .to_string();
                    let mut values = Vec::<rusqlite::types::Value>::new();
                    if let Some(registrant_scope_id) = filter.effective_registrant_scope_id() {
                        sql.push_str(" AND owner_scope = ?");
                        values.push(registrant_scope_id.into());
                    }
                    if let Some(subscription_key) = filter.subscription_key.as_ref() {
                        sql.push_str(" AND subscription_key = ?");
                        values.push(subscription_key.clone().into());
                    }
                    if let Some(source_type) = filter.source_type.as_ref() {
                        sql.push_str(" AND source_type = ?");
                        values.push(source_type.clone().into());
                    }
                    if let Some(source_key) = filter.source_key.as_ref() {
                        sql.push_str(" AND source_key = ?");
                        values.push(source_key.clone().into());
                    }
                    if let Some(enabled) = filter.enabled {
                        sql.push_str(" AND enabled = ?");
                        values.push(i64::from(enabled).into());
                    }
                    sql.push_str(
                        " AND tombstoned = 0 ORDER BY owner_scope ASC, subscription_key ASC",
                    );
                    let mut stmt = conn.prepare(&sql).map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })
                        .map_err(process_sqlite_error)?;
                    let mut records = Vec::new();
                    for row in rows {
                        let (subscription_id, json) = row.map_err(process_sqlite_error)?;
                        let record = match Self::decode_subscription(json) {
                            Ok(record) => record,
                            Err(err) => {
                                tracing::warn!(
                                    error = %err,
                                    subscription_id,
                                    "skipping malformed trigger subscription during listing"
                                );
                                continue;
                            }
                        };
                        if filter.matches(&record) {
                            records.push(record);
                        }
                    }
                    Ok(records)
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn delete_session_subscriptions(
        &self,
        session_id: &str,
    ) -> Result<usize, lash_core::PluginError> {
        let session_id = session_id.to_string();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                Ok(trigger_tx_outcome((|| {
                    let mut stmt = tx
                        .prepare("SELECT subscription_id, record_json FROM trigger_subscriptions")
                        .map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map([], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })
                        .map_err(process_sqlite_error)?;
                    let mut subscriptions = Vec::new();
                    for row in rows {
                        let (subscription_id, json) = row.map_err(process_sqlite_error)?;
                        let record = match Self::decode_subscription(json) {
                            Ok(record) => record,
                            Err(err) => {
                                tracing::warn!(
                                    error = %err,
                                    subscription_id,
                                    "skipping malformed trigger subscription during session delete"
                                );
                                continue;
                            }
                        };
                        if record.registrant_session_id() == Some(session_id.as_str())
                            && !record.tombstoned
                        {
                            subscriptions.push((subscription_id, record));
                        }
                    }
                    drop(stmt);
                    let mut deleted = 0usize;
                    for (subscription_id, mut record) in subscriptions {
                        let next_revision =
                            lash_core::facade_support::next_trigger_store_revision(&record)?;
                        record.enabled = false;
                        record.tombstoned = true;
                        record.deleted_at_ms = Some(now);
                        record.revision = next_revision;
                        record.updated_at_ms = now;
                        let sql_revision = plugin_sql_counter_value(
                            "trigger_subscription_revision",
                            record.revision,
                        )?;
                        deleted += tx
                            .execute(
                                "UPDATE trigger_subscriptions
                                 SET enabled = 0, tombstoned = 1, revision = ?2,
                                     updated_at_ms = ?3, record_json = ?4
                                 WHERE subscription_id = ?1",
                                params![
                                    subscription_id.as_str(),
                                    sql_revision,
                                    now as i64,
                                    Self::encode_json(&record)?,
                                ],
                            )
                            .map_err(process_sqlite_error)?;
                    }
                    Ok(deleted)
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn ingest_occurrence(
        &self,
        request: lash_core::TriggerOccurrenceRequest,
    ) -> Result<lash_core::TriggerIngressReceipt, lash_core::PluginError> {
        lash_core::facade_support::validate_trigger_occurrence_request(&request)?;
        let occurrence_id = lash_core::facade_support::deterministic_occurrence_id(&request);
        let occurred_at_ms = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                Ok(trigger_tx_outcome((|| {
                    let existing: Option<String> = tx
                        .query_row(
                            "SELECT record_json
                             FROM trigger_occurrences
                             WHERE idempotency_key = ?1",
                            params![request.idempotency_key.as_str()],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(process_sqlite_error)?;
                    let (record, is_new) = if let Some(existing_json) = existing {
                        let record = Self::decode_occurrence(existing_json)?;
                        if !lash_core::facade_support::trigger_occurrence_request_matches_record(
                            &request, &record,
                        ) {
                            return Err(lash_core::PluginError::Session(format!(
                                "trigger occurrence idempotency conflict for `{}`",
                                request.idempotency_key
                            )));
                        }
                        (record, false)
                    } else {
                        let record = lash_core::TriggerOccurrenceRecord {
                            occurrence_id: occurrence_id.clone(),
                            source_type: request.source_type,
                            source_key: request.source_key,
                            payload: request.payload,
                            idempotency_key: request.idempotency_key,
                            source: request.source,
                            session_id: request.session_id,
                            occurred_at_ms,
                        };
                        tx.execute(
                            "INSERT INTO trigger_occurrences (
                                occurrence_id, idempotency_key, source_type,
                                source_key, occurred_at_ms, record_json
                             )
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            params![
                                record.occurrence_id.as_str(),
                                record.idempotency_key.as_str(),
                                record.source_type.as_str(),
                                record.source_key.as_str(),
                                record.occurred_at_ms as i64,
                                Self::encode_json(&record)?,
                            ],
                        )
                        .map_err(process_sqlite_error)?;
                        (record, true)
                    };
                    let reservations = if is_new {
                        reserve_sqlite_deliveries(tx, &record, occurred_at_ms)?
                    } else {
                        sqlite_delivery_snapshots(
                            tx,
                            &record,
                            lash_core::TriggerDeliveryReservationOutcome::AlreadyReserved,
                        )?
                    };
                    if is_new && reservations.is_empty() {
                        tx.execute(
                            "UPDATE trigger_occurrences
                             SET reclaimable_at_ms = ?2
                             WHERE occurrence_id = ?1 AND reclaimable_at_ms IS NULL",
                            params![record.occurrence_id.as_str(), record.occurred_at_ms as i64],
                        )
                        .map_err(process_sqlite_error)?;
                    }
                    Ok(lash_core::TriggerIngressReceipt {
                        occurrence: record,
                        reservations,
                    })
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn list_occurrences(
        &self,
        filter: lash_core::TriggerOccurrenceFilter,
    ) -> Result<Vec<lash_core::TriggerOccurrenceRecord>, lash_core::PluginError> {
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let mut sql =
                        "SELECT occurrence_id, record_json FROM trigger_occurrences WHERE 1 = 1"
                            .to_string();
                    let mut values = Vec::<rusqlite::types::Value>::new();
                    if let Some(source_type) = filter.source_type.as_ref() {
                        sql.push_str(" AND source_type = ?");
                        values.push(source_type.clone().into());
                    }
                    if let Some(source_key) = filter.source_key.as_ref() {
                        sql.push_str(" AND source_key = ?");
                        values.push(source_key.clone().into());
                    }
                    if let Some(start_ms) = filter.occurred_at_start_ms {
                        sql.push_str(" AND occurred_at_ms >= ?");
                        values.push(crate::clamp_epoch_ms(start_ms).into());
                    }
                    if let Some(end_ms) = filter.occurred_at_end_ms {
                        sql.push_str(" AND occurred_at_ms < ?");
                        values.push(crate::clamp_epoch_ms(end_ms).into());
                    }
                    sql.push_str(" ORDER BY occurred_at_ms ASC, occurrence_id ASC");
                    let mut stmt = conn.prepare(&sql).map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })
                        .map_err(process_sqlite_error)?;
                    let mut records = Vec::new();
                    for row in rows {
                        let (occurrence_id, json) = row.map_err(process_sqlite_error)?;
                        match Self::decode_occurrence(json) {
                            Ok(record) => records.push(record),
                            Err(err) => tracing::warn!(
                                error = %err,
                                occurrence_id,
                                "skipping malformed trigger occurrence during listing"
                            ),
                        }
                    }
                    Ok(records)
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn list_deliveries_by_occurrence_id(
        &self,
        occurrence_id: &str,
    ) -> Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError> {
        self.list_deliveries_where(
            "d.occurrence_id = ?1",
            vec![occurrence_id.to_string().into()],
        )
        .await
    }

    async fn list_deliveries_by_subscription_id(
        &self,
        subscription_id: &str,
    ) -> Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError> {
        self.list_deliveries_where(
            "d.subscription_id = ?1",
            vec![subscription_id.to_string().into()],
        )
        .await
    }

    async fn list_deliveries_by_process_id(
        &self,
        process_id: &str,
    ) -> Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError> {
        self.list_deliveries_where("d.process_id = ?1", vec![process_id.to_string().into()])
            .await
    }

    async fn list_deliveries(
        &self,
    ) -> Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError> {
        self.list_deliveries_where("1 = 1", Vec::new()).await
    }

    async fn list_delivery_process_ids(&self) -> Result<Vec<String>, lash_core::PluginError> {
        self.conn
            .call(|conn| {
                Ok((|| {
                    let mut stmt = conn
                        .prepare(
                            "SELECT DISTINCT process_id
                             FROM trigger_deliveries
                             ORDER BY process_id ASC",
                        )
                        .map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map([], |row| row.get::<_, String>(0))
                        .map_err(process_sqlite_error)?;
                    rows.collect::<Result<Vec<_>, _>>()
                        .map_err(process_sqlite_error)
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn list_delivery_retention_candidates(
        &self,
    ) -> Result<Vec<lash_core::TriggerDeliveryRetentionCandidate>, lash_core::PluginError> {
        self.conn
            .call(|conn| {
                Ok((|| {
                    let mut stmt = conn
                        .prepare(
                            "SELECT occurrence_id, subscription_id, process_id
                             FROM trigger_deliveries
                             ORDER BY occurrence_id ASC, subscription_id ASC",
                        )
                        .map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map([], |row| {
                            Ok(lash_core::TriggerDeliveryRetentionCandidate {
                                occurrence_id: row.get(0)?,
                                subscription_id: row.get(1)?,
                                process_id: row.get(2)?,
                            })
                        })
                        .map_err(process_sqlite_error)?;
                    rows.collect::<Result<Vec<_>, _>>()
                        .map_err(process_sqlite_error)
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn delete_delivery_retention_candidates(
        &self,
        candidates: &[lash_core::TriggerDeliveryRetentionCandidate],
    ) -> Result<usize, lash_core::PluginError> {
        if candidates.is_empty() {
            return Ok(0);
        }
        let candidates_json = serde_json::to_string(candidates).map_err(process_decode_error)?;
        let armed_at_ms = i64::try_from(self.clock.timestamp_ms()).unwrap_or(i64::MAX);
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                let deleted = tx.execute(
                    "DELETE FROM trigger_deliveries
                     WHERE EXISTS (
                         SELECT 1
                         FROM json_each(?1) AS candidate
                         WHERE trigger_deliveries.occurrence_id =
                                   json_extract(candidate.value, '$.occurrence_id')
                           AND trigger_deliveries.subscription_id =
                                   json_extract(candidate.value, '$.subscription_id')
                           AND trigger_deliveries.process_id =
                                   json_extract(candidate.value, '$.process_id')
                     )",
                    params![&candidates_json],
                )?;
                tx.execute(
                    "UPDATE trigger_occurrences
                     SET reclaimable_at_ms = ?2
                     WHERE reclaimable_at_ms IS NULL
                       AND NOT EXISTS (
                           SELECT 1 FROM trigger_deliveries
                           WHERE trigger_deliveries.occurrence_id =
                                 trigger_occurrences.occurrence_id
                       )
                       AND occurrence_id IN (
                           SELECT DISTINCT json_extract(candidate.value, '$.occurrence_id')
                           FROM json_each(?1) AS candidate
                       )",
                    params![&candidates_json, armed_at_ms],
                )?;
                tx.commit()?;
                Ok(deleted)
            })
            .await
            .map_err(process_sqlite_error)
    }

    async fn reclaim_trigger_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> lash_core::TriggerOccurrenceReclamationResult {
        let cutoff_epoch_ms = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        let partial = Arc::new(Mutex::new(
            lash_core::TriggerOccurrenceReclamationReport::default(),
        ));
        let partial_for_call = Arc::clone(&partial);
        self.conn
            .call(move |conn| {
                Ok((|| {
                    // One statement gives the scope proof and the indexed
                    // worklist from the same snapshot. The aggregate visits the
                    // whole table so `NothingToDo` stays witnessed emptiness;
                    // only eligible ids are materialized.
                    let rows = {
                        let mut stmt = conn
                            .prepare(
                                "WITH scope AS (
                                     SELECT COUNT(*) AS inspected_count,
                                            COUNT(*) FILTER (
                                                WHERE reclaimable_at_ms IS NULL
                                            ) AS live_fan_out_count,
                                            COUNT(*) FILTER (
                                                WHERE reclaimable_at_ms > ?1
                                            ) AS grace_deferred_count
                                     FROM trigger_occurrences
                                 ), candidates AS (
                                     SELECT occurrence_id
                                     FROM trigger_occurrences
                                     WHERE reclaimable_at_ms IS NOT NULL
                                       AND reclaimable_at_ms <= ?1
                                 )
                                 SELECT scope.inspected_count,
                                        scope.live_fan_out_count,
                                        scope.grace_deferred_count,
                                        candidates.occurrence_id
                                 FROM scope
                                 LEFT JOIN candidates ON TRUE
                                 ORDER BY candidates.occurrence_id ASC",
                            )
                            .map_err(|error| {
                                lash_core::MaintenanceFailure::failed_before_any_work(Box::new(
                                    process_sqlite_error(error),
                                ))
                            })?;
                        let rows = stmt
                            .query_map(params![cutoff_epoch_ms], |row| {
                                Ok((
                                    row.get::<_, i64>(0)?,
                                    row.get::<_, i64>(1)?,
                                    row.get::<_, i64>(2)?,
                                    row.get::<_, Option<String>>(3)?,
                                ))
                            })
                            .map_err(|error| {
                                lash_core::MaintenanceFailure::failed_before_any_work(Box::new(
                                    process_sqlite_error(error),
                                ))
                            })?;
                        rows.collect::<Result<Vec<_>, _>>().map_err(|error| {
                            lash_core::MaintenanceFailure::failed_before_any_work(Box::new(
                                process_sqlite_error(error),
                            ))
                        })?
                    };

                    let first = &rows[0];
                    let mut report = lash_core::TriggerOccurrenceReclamationReport {
                        inspected_occurrence_count: first.0 as usize,
                        live_fan_out_count: first.1 as usize,
                        grace_deferred_count: first.2 as usize,
                        ..lash_core::TriggerOccurrenceReclamationReport::default()
                    };
                    *partial_for_call
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = report.clone();
                    let candidates = rows
                        .into_iter()
                        .filter_map(|(_, _, _, occurrence_id)| occurrence_id)
                        .collect::<Vec<_>>();

                    for occurrence_id in candidates {
                        let deleted = conn
                            .execute(
                                "DELETE FROM trigger_occurrences
                                 WHERE occurrence_id = ?1
                                   AND reclaimable_at_ms IS NOT NULL
                                   AND reclaimable_at_ms <= ?2
                                   AND NOT EXISTS (
                                       SELECT 1 FROM trigger_deliveries
                                       WHERE trigger_deliveries.occurrence_id =
                                             trigger_occurrences.occurrence_id
                                   )",
                                params![occurrence_id, cutoff_epoch_ms],
                            )
                            .map_err(|error| {
                                lash_core::MaintenanceFailure::failed(
                                    Box::new(process_sqlite_error(error)),
                                    report.clone(),
                                )
                            })?;
                        if deleted == 0 {
                            report.reinspection_deferred_count += 1;
                        } else {
                            report.reclaimed_occurrence_count += deleted;
                        }
                        *partial_for_call
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = report.clone();
                    }
                    Ok(report)
                })())
            })
            .await
            .map_err(|error| {
                lash_core::MaintenanceFailure::failed(
                    Box::new(process_sqlite_error(error)),
                    partial
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone(),
                )
            })?
    }

    async fn prune_mutation_receipts(
        &self,
        cutoff_epoch_ms: u64,
    ) -> Result<usize, lash_core::PluginError> {
        let cutoff_epoch_ms = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        self.conn
            .call(move |conn| {
                conn.execute(
                    "DELETE FROM trigger_mutation_receipts WHERE created_at_ms < ?1",
                    params![cutoff_epoch_ms],
                )
            })
            .await
            .map_err(process_sqlite_error)
    }
}

fn reserve_sqlite_deliveries(
    tx: &rusqlite::Transaction<'_>,
    occurrence: &lash_core::TriggerOccurrenceRecord,
    created_at_ms: u64,
) -> Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError> {
    let mut sql = "SELECT subscription_id, record_json FROM trigger_subscriptions
         WHERE enabled = 1 AND tombstoned = 0 AND source_type = ?1 AND source_key = ?2"
        .to_string();
    let mut values: Vec<rusqlite::types::Value> = vec![
        occurrence.source_type.clone().into(),
        occurrence.source_key.clone().into(),
    ];
    if let Some(session_id) = occurrence.session_id.as_deref() {
        sql.push_str(" AND owner_scope = ?3");
        values.push(
            lash_core::TriggerOwnerScope::session(session_id)
                .namespace()
                .into(),
        );
    }
    sql.push_str(" ORDER BY owner_scope ASC, subscription_key ASC");
    let mut stmt = tx.prepare(&sql).map_err(process_sqlite_error)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(process_sqlite_error)?;
    let mut subscriptions = Vec::new();
    for row in rows {
        let (subscription_id, json) = row.map_err(process_sqlite_error)?;
        match SqliteTriggerStore::decode_subscription(json) {
            Ok(subscription) => subscriptions.push(subscription),
            Err(err) => tracing::warn!(
                error = %err,
                subscription_id,
                "skipping malformed trigger subscription during occurrence ingress"
            ),
        }
    }
    drop(stmt);

    let mut reservations = Vec::with_capacity(subscriptions.len());
    for subscription in subscriptions {
        let sql_revision =
            plugin_sql_counter_value("trigger_subscription_revision", subscription.revision)?;
        let process_id = lash_core::facade_support::deterministic_delivery_process_id(
            &occurrence.occurrence_id,
            &subscription.subscription_id,
            &subscription.incarnation,
            subscription.revision,
        )?;
        tx.execute(
            "INSERT INTO trigger_deliveries (
                occurrence_id, subscription_id, process_id, subscription_incarnation,
                subscription_revision, subscription_snapshot_json, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                occurrence.occurrence_id.as_str(),
                subscription.subscription_id.as_str(),
                process_id.as_str(),
                subscription.incarnation.as_str(),
                sql_revision,
                SqliteTriggerStore::encode_json(&subscription)?,
                created_at_ms as i64,
            ],
        )
        .map_err(process_sqlite_error)?;
        reservations.push(lash_core::TriggerDeliveryReservation {
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

fn sqlite_delivery_snapshots(
    tx: &rusqlite::Transaction<'_>,
    occurrence: &lash_core::TriggerOccurrenceRecord,
    reservation_status: lash_core::TriggerDeliveryReservationOutcome,
) -> Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError> {
    let mut stmt = tx
        .prepare(
            "SELECT process_id, created_at_ms, subscription_snapshot_json
             FROM trigger_deliveries
             WHERE occurrence_id = ?1",
        )
        .map_err(process_sqlite_error)?;
    let rows = stmt
        .query_map(params![occurrence.occurrence_id.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(process_sqlite_error)?;
    let mut reservations = Vec::new();
    for row in rows {
        let (process_id, created_at_ms, snapshot_json) = row.map_err(process_sqlite_error)?;
        reservations.push(lash_core::TriggerDeliveryReservation {
            occurrence: occurrence.clone(),
            subscription: SqliteTriggerStore::decode_subscription(snapshot_json)?,
            process_id,
            created_at_ms: plugin_u64_from_sql("TriggerDelivery", "created_at_ms", created_at_ms)?,
            reservation_status: reservation_status.clone(),
        });
    }
    lash_core::facade_support::sort_trigger_delivery_reservations(&mut reservations);
    Ok(reservations)
}
