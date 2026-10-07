//! PostgreSQL-backed runtime trigger store, and the PostgreSQL owner of the
//! trigger family.
//!
//! Every mutating atom runs in a server transaction that takes an advisory
//! lock on the subscription or the idempotency key first and reads the rows it
//! decides on `FOR UPDATE`, so the receipt check, the record read and the
//! write they guard cannot interleave under `READ COMMITTED`. SQLite reaches
//! the same guarantee through `BEGIN IMMEDIATE`, which is why most of this
//! module's forks are a lock suffix and nothing else.

use crate::*;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_store_sql::Dialect;
use lash_store_sql::trigger::deliveries::DeliveryStatements;
use lash_store_sql::trigger::mutation_receipts::MutationReceiptStatements;
use lash_store_sql::trigger::occurrence_tombstones::OccurrenceTombstoneStatements;
use lash_store_sql::trigger::occurrences::{
    ListShape as OccurrenceListShape, OccurrenceStatements,
};
use lash_store_sql::trigger::subscriptions::{
    ListShape as SubscriptionListShape, SubscriptionStatements,
};
use std::sync::LazyLock;

#[path = "trigger_store/start.rs"]
pub(crate) mod start;

lash_store_sql::statements! {
    /// `trigger_subscriptions` statements only PostgreSQL issues.
    pub(crate) struct SubscriptionPostgresStatements @ "trigger_subscription" {
        /// Serialize compaction with subscription change publication.
        lock_change_clock = "SELECT current_seq FROM trigger_subscription_change_clock WHERE singleton = TRUE FOR UPDATE";

        /// The record of subscription `$1`, under its write lock.
        ///
        /// `FOR UPDATE` is the fork: `READ COMMITTED` cannot hold this read
        /// across the mutation it decides, while SQLite already holds the
        /// database write lock.
        select_record_by_id = "SELECT record_json FROM trigger_subscriptions
             WHERE subscription_id = ?1 FOR UPDATE";

        /// Every live record owned by scope `?1`, under their write locks: the
        /// input a prune evaluates. Forks for the same reason
        /// [`SubscriptionPostgresStatements::select_record_by_id`] does.
        select_records_for_prune = "SELECT record_json FROM trigger_subscriptions
             WHERE owner_scope = ?1 AND lifecycle <> 'tombstoned' FOR UPDATE";

        /// Every enabled subscription an occurrence of `?1`/`?2` fires at,
        /// under a share lock.
        ///
        /// `FOR SHARE` is the fork: it stops a concurrent mutation retiring a
        /// subscription between this read and the delivery it reserves.
        /// SQLite holds the write lock for the whole ingress. The predicates
        /// are plain equalities on both backends, so the read seeks
        /// `(source_type, source_key, lifecycle)` on all three columns.
        select_enabled_for_source = "SELECT subscription_id, record_json
             FROM trigger_subscriptions
             WHERE lifecycle = 'enabled'
               AND source_type = ?1
               AND source_key = ?2
             ORDER BY owner_scope ASC, subscription_key ASC FOR SHARE";

        /// The same read narrowed to owner scope `?3`, which is what an
        /// occurrence that names a session fires at. Its own statement rather
        /// than an optional predicate, for the reason in
        /// [`lash_store_sql::trigger::subscriptions::ListShape`].
        select_enabled_for_source_and_owner = "SELECT subscription_id, record_json
             FROM trigger_subscriptions
             WHERE lifecycle = 'enabled'
               AND source_type = ?1
               AND source_key = ?2
               AND owner_scope = ?3
             ORDER BY owner_scope ASC, subscription_key ASC FOR SHARE";

        /// Every live subscription owned by scope `?1`, under their write
        /// locks: what a session delete tombstones.
        ///
        /// PostgreSQL alone has this operation. SQLite decides ownership from
        /// the decoded record's registrant session instead of from the
        /// `owner_scope` column, reading the whole table to do it, and skips a
        /// row whose JSON is malformed rather than failing the sweep. The two
        /// are deliberately left as they stand; closing the difference is a
        /// behaviour change, not a rendering one.
        select_session_owned_for_update = "SELECT subscription_id, record_json
             FROM trigger_subscriptions
             WHERE owner_scope = ?1 AND lifecycle <> 'tombstoned' FOR UPDATE";

        /// Lock every subscription of the owner scopes in `?1` that no
        /// delivery still references, before recording deletion evidence.
        ///
        /// PostgreSQL binds a real `TEXT[]`; SQLite unnests a JSON array with
        /// `json_each`.
        select_unreferenced_for_owners = "SELECT record_json FROM trigger_subscriptions AS subscription
             WHERE owner_scope = ANY(?1)
               AND NOT EXISTS (SELECT 1 FROM trigger_deliveries WHERE trigger_deliveries.subscription_id = subscription.subscription_id)
             FOR UPDATE";

        delete_unreferenced_by_ids = "DELETE FROM trigger_subscriptions AS subscription
             WHERE subscription.subscription_id = ANY(?1::TEXT[])
               AND NOT EXISTS (
                   SELECT 1 FROM trigger_deliveries AS delivery
                   WHERE delivery.subscription_id = subscription.subscription_id
               )";
    }
}

lash_store_sql::statements! {
    /// `trigger_occurrences` statements only PostgreSQL issues.
    pub(crate) struct OccurrencePostgresStatements @ "trigger_occurrence" {
        /// The occurrence already stored under idempotency key `?1`, under its
        /// write lock.
        ///
        /// `FOR UPDATE` is the fork: it holds the idempotency comparison
        /// across the insert that follows it. SQLite reads it under
        /// `BEGIN IMMEDIATE`.
        select_record_by_idempotency_key = "SELECT record_json
             FROM trigger_occurrences
             WHERE idempotency_key = ?1 FOR UPDATE";

        /// Delete every fired occurrence no delivery references, leaving each
        /// one's tombstone at `?1` (FIG-4513).
        ///
        /// Both backends read the typed outcome column. PostgreSQL deletes
        /// and tombstones in one statement,
        /// so no concurrent ingest sees the row gone and its tombstone absent;
        /// SQLite issues the two under its single writer.
        delete_orphan_fired = "WITH reclaimed AS (
                 DELETE FROM trigger_occurrences AS occurrence
                 WHERE occurrence.outcome_kind = 'fired'
                   AND NOT EXISTS (
                       SELECT 1 FROM trigger_deliveries AS delivery
                       WHERE delivery.occurrence_id = occurrence.occurrence_id
                   )
                 RETURNING occurrence.occurrence_id
             )
             INSERT INTO trigger_occurrence_tombstones (occurrence_id, reclaimed_at_ms)
             SELECT reclaimed.occurrence_id, ?1 FROM reclaimed";

        arm_reclaimable_for_candidates = "UPDATE trigger_occurrences AS occurrence
             SET reclaimable_at_ms = ?2
             WHERE occurrence.reclaimable_at_ms IS NULL
               AND occurrence.outcome_kind = 'fired'
               AND occurrence.occurrence_id = ANY(?1::TEXT[])
               AND NOT EXISTS (
                   SELECT 1 FROM trigger_deliveries AS delivery
                   WHERE delivery.occurrence_id = occurrence.occurrence_id
               )";

        /// The reclamation sweep's scope proof and its worklist, from one
        /// snapshot at cutoff `?1`.
        ///
        /// The aggregate visits the whole table so `NothingToDo` stays witnessed emptiness;
        /// only eligible ids are materialized, through the partial reclamation index.
        select_reclamation_scope = "WITH scope AS (
                 SELECT COUNT(*) AS inspected_count,
                        COUNT(*) FILTER (
                            WHERE reclaimable_at_ms IS NULL
                              AND outcome_kind = 'fired'
                        ) AS live_fan_out_count,
                        COUNT(*) FILTER (
                            WHERE outcome_kind != 'fired'
                        ) AS audit_retained_count,
                        COUNT(*) FILTER (
                            WHERE reclaimable_at_ms > ?1
                              AND outcome_kind = 'fired'
                        ) AS grace_deferred_count
                 FROM trigger_occurrences
             ), candidates AS (
                 SELECT occurrence_id
                 FROM trigger_occurrences
                 WHERE reclaimable_at_ms IS NOT NULL
                   AND reclaimable_at_ms <= ?1
                   AND outcome_kind = 'fired'
             )
             SELECT scope.inspected_count,
                    scope.live_fan_out_count,
                    scope.grace_deferred_count,
                    scope.audit_retained_count,
                    candidates.occurrence_id
             FROM scope
             LEFT JOIN candidates ON TRUE
             ORDER BY candidates.occurrence_id ASC";

        /// Reclaim occurrence `?1` if it is still eligible at cutoff `?2`,
        /// leaving its tombstone at `?3`.
        /// The whole eligibility test is re-proved here, because the worklist was read from an
        /// earlier snapshot.
        delete_reclaimable_by_id = "WITH reclaimed AS (
                 DELETE FROM trigger_occurrences AS occurrence
                 WHERE occurrence.occurrence_id = ?1
                   AND occurrence.reclaimable_at_ms IS NOT NULL
                   AND occurrence.reclaimable_at_ms <= ?2
                   AND occurrence.outcome_kind = 'fired'
                   AND NOT EXISTS (
                       SELECT 1 FROM trigger_deliveries AS delivery
                       WHERE delivery.occurrence_id = occurrence.occurrence_id
                   )
                 RETURNING occurrence.occurrence_id
             )
             INSERT INTO trigger_occurrence_tombstones (occurrence_id, reclaimed_at_ms)
             SELECT reclaimed.occurrence_id, ?3 FROM reclaimed";

        /// Delete every audit row recorded before cutoff `?1`, leaving each
        /// one's tombstone at `?2`.
        prune_non_fired = "WITH reclaimed AS (
                 DELETE FROM trigger_occurrences AS occurrence
                 WHERE occurrence.occurred_at_ms < ?1
                   AND occurrence.outcome_kind <> 'fired'
                 RETURNING occurrence.occurrence_id
             )
             INSERT INTO trigger_occurrence_tombstones (occurrence_id, reclaimed_at_ms)
             SELECT reclaimed.occurrence_id, ?2 FROM reclaimed";
    }
}

lash_store_sql::statements! {
    /// `trigger_deliveries` statements only PostgreSQL issues.
    pub(crate) struct DeliveryPostgresStatements @ "trigger_delivery" {
        /// Delete every delivery named by the three parallel arrays `?1`,
        /// `?2`, `?3`.
        ///
        /// PostgreSQL joins bound `TEXT[]`s with `UNNEST`; SQLite unnests one
        /// JSON array with `json_each` and reads the three ids back out of it.
        delete_retention_candidates = "DELETE FROM trigger_deliveries AS delivery
             USING UNNEST(?1::TEXT[], ?2::TEXT[], ?3::TEXT[])
                   AS candidate(occurrence_id, subscription_id, process_id)
             WHERE delivery.occurrence_id = candidate.occurrence_id
               AND delivery.subscription_id = candidate.subscription_id
               AND delivery.process_id = candidate.process_id";
    }
}

lash_store_sql::statements! {
    /// The trigger family's cross-table retention read, as PostgreSQL issues
    /// it.
    pub(crate) struct RetentionPostgresStatements @ "trigger_retention" {
        /// Every session that owns a subscription, a delivery's frozen
        /// subscription, or a mutation receipt: the candidate set a session
        /// retention pass reconciles against the session catalog.
        ///
        /// The one statement of this family that reads all three tables, and
        /// it forks on the JSON read in the delivery arm.
        select_session_owner_ids = "SELECT owner_scope
             FROM (
                 SELECT owner_scope FROM trigger_subscriptions
                 UNION
                 SELECT 'session:' ||
                        (subscription_snapshot_json::jsonb #>> '{owner_scope,session_id}')
                 FROM trigger_deliveries
                 WHERE subscription_snapshot_json::jsonb #>> '{owner_scope,type}' = 'session'
                 UNION
                 SELECT 'session:' || owner_id
                 FROM trigger_mutation_receipts
                 WHERE owner_kind = 'session'
             ) AS trigger_owner_scopes
             WHERE owner_scope LIKE 'session:%'
             ORDER BY owner_scope";

        /// The host retention lever's receipt sweep (FIG-4108): receipts
        /// older than bound `?1` go when they are ownerless or their session
        /// owner is durably deleted — `deleted_sessions` is in this database,
        /// so the proof is a join — unless a delivery still names the owner.
        reclaim_mutation_receipts = "DELETE FROM trigger_mutation_receipts
             WHERE created_at_ms < ?1
               AND (
                   owner_kind IN ('host', 'platform')
                   OR (
                       owner_kind = 'session'
                       AND EXISTS (
                           SELECT 1 FROM deleted_sessions AS deleted
                           WHERE deleted.session_id = trigger_mutation_receipts.owner_id
                       )
                       AND NOT EXISTS (
                           SELECT 1 FROM trigger_deliveries
                           WHERE subscription_snapshot_json::jsonb #>> '{owner_scope,type}'
                                 = 'session'
                             AND subscription_snapshot_json::jsonb #>> '{owner_scope,session_id}'
                                 = trigger_mutation_receipts.owner_id
                       )
                   )
               )";
    }
}

/// Every trigger-family statement, rendered once.
pub(crate) struct TriggerSql {
    /// `trigger_subscriptions` statements both backends issue verbatim.
    subscription_change:
        lash_store_sql::trigger::subscription_changes::SubscriptionChangeStatements,
    subscription_change_clock:
        lash_store_sql::trigger::subscription_change_clock::SubscriptionChangeClockStatements,
    pub(crate) subscription: SubscriptionStatements,
    /// `trigger_subscriptions` statements only PostgreSQL issues.
    pub(crate) subscription_postgres: SubscriptionPostgresStatements,
    /// `trigger_occurrences` statements both backends issue verbatim.
    pub(crate) occurrence: OccurrenceStatements,
    /// `trigger_occurrences` statements only PostgreSQL issues.
    occurrence_postgres: OccurrencePostgresStatements,
    /// `trigger_occurrence_tombstones` statements both backends issue
    /// verbatim.
    tombstone: OccurrenceTombstoneStatements,
    /// `trigger_deliveries` statements both backends issue verbatim.
    pub(crate) delivery: DeliveryStatements,
    /// `trigger_deliveries` statements only PostgreSQL issues.
    pub(crate) delivery_postgres: DeliveryPostgresStatements,
    /// `trigger_mutation_receipts` statements both backends issue verbatim.
    receipt: MutationReceiptStatements,
    /// The family's cross-table retention statements, also issued by
    /// `evidence_retention.rs` inside the sweep's guarded transaction.
    pub(crate) retention_postgres: RetentionPostgresStatements,
}

static TRIGGER_SQL: LazyLock<TriggerSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    TriggerSql {
        subscription_change:
            lash_store_sql::trigger::subscription_changes::SubscriptionChangeStatements::render(
                dialect,
            ),
        subscription_change_clock: lash_store_sql::trigger::subscription_change_clock::SubscriptionChangeClockStatements::render(dialect),
        subscription: SubscriptionStatements::render(dialect),
        subscription_postgres: SubscriptionPostgresStatements::render(dialect),
        occurrence: OccurrenceStatements::render(dialect),
        occurrence_postgres: OccurrencePostgresStatements::render(dialect),
        tombstone: OccurrenceTombstoneStatements::render(dialect),
        delivery: DeliveryStatements::render(dialect),
        delivery_postgres: DeliveryPostgresStatements::render(dialect),
        receipt: MutationReceiptStatements::render(dialect),
        retention_postgres: RetentionPostgresStatements::render(dialect),
    }
});

/// The trigger-family statements, rendered at first use and never again.
pub(crate) fn trigger_sql() -> &'static TriggerSql {
    &TRIGGER_SQL
}

/// The rendered listing statement `filter`'s shape is served by, for the
/// conformance assertion that the owner filter is pushed into SQL rather than
/// applied in Rust.
#[cfg(any(test, feature = "testing"))]
pub(crate) fn subscription_list_sql(filter: &TriggerSubscriptionFilter) -> &'static str {
    trigger_sql()
        .subscription
        .list_for(subscription_list_shape(filter))
        .sql()
}

/// The listing statement shape `filter` is served by.
pub(crate) fn subscription_list_shape(filter: &TriggerSubscriptionFilter) -> SubscriptionListShape {
    SubscriptionListShape::of(
        filter.registrant_scope_id.is_some(),
        filter.subscription_key.is_some(),
        filter.source_type.is_some(),
        filter.source_key.is_some(),
    )
}

/// What the subscription listing of `shape` binds, in its parameter order.
///
/// Exhaustive over the shape, so a new listing statement cannot be added
/// without deciding what it binds.
fn subscription_list_bindings(
    filter: &TriggerSubscriptionFilter,
    shape: SubscriptionListShape,
) -> Vec<String> {
    let text = |value: &Option<String>| value.clone().unwrap_or_default();
    match shape {
        SubscriptionListShape::All => Vec::new(),
        SubscriptionListShape::ByOwner => vec![text(&filter.registrant_scope_id)],
        SubscriptionListShape::ByOwnerAndKey => vec![
            text(&filter.registrant_scope_id),
            text(&filter.subscription_key),
        ],
        SubscriptionListShape::BySourceType => vec![text(&filter.source_type)],
        SubscriptionListShape::BySource => {
            vec![text(&filter.source_type), text(&filter.source_key)]
        }
    }
}

/// What the occurrence listing of `shape` binds: its source equalities, then
/// the window.
///
/// The window is always bound — an unset start as `i64::MIN`, an unset end as
/// `i64::MAX` — so the comparison stays a plain one against a value and the
/// index range survives. The closed `[start, end]` this produces is a superset
/// of the filter's half-open `[start, end)`, which
/// `TriggerOccurrenceFilter::matches` then narrows exactly.
pub(crate) fn occurrence_list_bindings(
    filter: &lash_core_execution::TriggerOccurrenceFilter,
    shape: OccurrenceListShape,
) -> (Vec<String>, i64, i64) {
    let text = |value: &Option<String>| value.clone().unwrap_or_default();
    let keys = match shape {
        OccurrenceListShape::All => Vec::new(),
        OccurrenceListShape::BySourceType => vec![text(&filter.source_type)],
        OccurrenceListShape::BySource => {
            vec![text(&filter.source_type), text(&filter.source_key)]
        }
    };
    (
        keys,
        filter.occurred_at_start_ms.map_or(i64::MIN, clamp_epoch_ms),
        filter.occurred_at_end_ms.map_or(i64::MAX, clamp_epoch_ms),
    )
}

#[async_trait::async_trait]
impl TriggerStore for PostgresTriggerStore {
    async fn execute_command(
        &self,
        operation_id: &str,
        command: lash_core_execution::TriggerCommand,
    ) -> Result<lash_core_execution::TriggerEffectResult, PluginError> {
        let prepared = match lash_core_execution::facade_support::prepare_trigger_command(
            command,
            operation_id,
            self.fixed_incarnation.clone(),
        ) {
            Ok(prepared) => prepared,
            Err(error) => return Ok(Err(error)),
        };
        use lash_core_execution::facade_support::PreparedTriggerCommand;
        let (command, preparation) = match prepared {
            PreparedTriggerCommand::List(filter) => {
                return self.list_subscriptions(filter).await.map(|records| {
                    Ok(lash_core_execution::TriggerCommandOutcome::List { records })
                });
            }
            PreparedTriggerCommand::Mutation {
                command,
                preparation,
            } => (command, preparation),
        };

        let sql = trigger_sql();
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        sqlx::query(
            crate::connection_sql::connection_sql()
                .lock_xact_by_text
                .sql(),
        )
        .bind(&preparation.subscription_id)
        .execute(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;

        let stored = sqlx::query(sql.receipt.select_by_operation_id.sql())
            .bind(&preparation.receipt_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?;
        if let Some(row) = stored {
            let stored_hash: String = row.get(0);
            let result_json: String = row.get(1);
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return lash_core_execution::facade_support::stored_trigger_receipt(
                stored_hash,
                &result_json,
                &preparation,
            );
        }

        let now = self.clock.timestamp_ms();
        let result = if let lash_core_execution::TriggerCommand::Prune {
            owner_scope,
            actor,
            subscription_keys,
        } = &*command
        {
            let rows = sqlx::query(sql.subscription_postgres.select_records_for_prune.sql())
                .bind(owner_scope.namespace())
                .fetch_all(&mut **tx)
                .await
                .map_err(plugin_sqlx_error)?;
            let records = rows
                .into_iter()
                .map(|row| {
                    let json: String = row.get(0);
                    lash_core_execution::facade_support::decode_trigger_subscription_json(&json)
                })
                .collect::<Result<Vec<_>, _>>()?;
            lash_core_execution::facade_support::evaluate_trigger_prune(
                records,
                owner_scope.clone(),
                actor.clone(),
                subscription_keys.clone(),
                now,
            )
        } else {
            let current_json: Option<String> =
                sqlx::query_scalar(sql.subscription_postgres.select_record_by_id.sql())
                    .bind(&preparation.subscription_id)
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(plugin_sqlx_error)?;
            let current = current_json
                .map(|json| {
                    lash_core_execution::facade_support::decode_trigger_subscription_json(&json)
                })
                .transpose()?;
            lash_core_execution::facade_support::evaluate_trigger_mutation_with_incarnation(
                current,
                *command,
                now,
                preparation.incarnation.clone(),
            )?
        };
        for record in lash_core_execution::facade_support::trigger_mutation_records(&result) {
            let sql_revision =
                plugin_sql_counter_value("trigger_subscription_revision", record.revision)?;
            sqlx::query(sql.subscription.upsert.sql())
                .bind(&record.subscription_id)
                .bind(record.owner_scope.namespace())
                .bind(&record.subscription_key)
                .bind(&record.incarnation)
                .bind(sql_revision)
                .bind(&record.definition_fingerprint)
                .bind(&record.source_type)
                .bind(&record.source_key)
                .bind(record.lifecycle.as_column())
                .bind(record.lifecycle.deleted_at_ms().map(|ms| ms as i64))
                .bind(record.created_at_ms as i64)
                .bind(record.updated_at_ms as i64)
                .bind(lash_core_execution::facade_support::encode_trigger_row(
                    record,
                )?)
                .execute(&mut **tx)
                .await
                .map_err(plugin_sqlx_error)?;
            record_subscription_change(&mut tx, record).await?;
        }
        sqlx::query(sql.receipt.insert.sql())
            .bind(&preparation.receipt_id)
            .bind(preparation.owner_scope.owner_kind_column())
            .bind(preparation.owner_scope.owner_id_column())
            .bind(&preparation.request_fingerprint)
            .bind(lash_core_execution::facade_support::encode_trigger_row(
                &result,
            )?)
            .bind(now as i64)
            .execute(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(result)
    }

    async fn list_subscriptions(
        &self,
        filter: TriggerSubscriptionFilter,
    ) -> Result<Vec<TriggerSubscriptionRecord>, PluginError> {
        let shape = subscription_list_shape(&filter);
        let mut query = sqlx::query(trigger_sql().subscription.list_for(shape).sql());
        for binding in subscription_list_bindings(&filter, shape) {
            query = query.bind(binding);
        }
        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        let mut records = Vec::new();
        for row in rows {
            let subscription_id: String = row.get(0);
            let json: String = row.get(1);
            match lash_core_execution::facade_support::decode_trigger_subscription_json(&json) {
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

    async fn subscriptions_changed_since(
        &self,
        cursor: lash_core_execution::TriggerSubscriptionChangeCursor,
        limit: usize,
    ) -> Result<
        (
            Vec<lash_core_execution::TriggerSubscriptionChange>,
            lash_core_execution::TriggerSubscriptionChangeCursor,
        ),
        PluginError,
    > {
        let sequence = plugin_sql_counter_value(
            "trigger_subscription_change_cursor",
            cursor.store_sequence(),
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        sqlx::query(
            crate::connection_sql::connection_sql()
                .begin_repeatable_read_read_only
                .sql(),
        )
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        let sql = &trigger_sql().subscription_change;
        let row = sqlx::query(trigger_sql().subscription_change_clock.clock.sql())
            .fetch_one(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        let horizon: i64 = row.get(1);
        if sequence < horizon {
            return Err(PluginError::TriggerSubscriptionChangeCursorPruned {
                requested_cursor: cursor,
                tombstone_compaction_horizon:
                    lash_core_execution::TriggerSubscriptionChangeCursor::from_store_sequence(
                        horizon as u64,
                    ),
            });
        }
        let rows = sqlx::query(sql.page.sql())
            .bind(sequence)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        let mut next = cursor;
        let mut changes = Vec::new();
        for row in rows {
            let seq: i64 = row.get(0);
            let json: String = row.get(1);
            changes.push(serde_json::from_str(&json).map_err(|error| {
                PluginError::StoredDataCorrupt {
                    record_kind: "TriggerSubscriptionChange".into(),
                    message: error.to_string(),
                }
            })?);
            next = lash_core_execution::TriggerSubscriptionChangeCursor::from_store_sequence(
                seq as u64,
            );
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok((changes, next))
    }

    async fn list_subscriptions_with_cursor(
        &self,
    ) -> Result<
        (
            Vec<TriggerSubscriptionRecord>,
            lash_core_execution::TriggerSubscriptionChangeCursor,
        ),
        PluginError,
    > {
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        sqlx::query(
            crate::connection_sql::connection_sql()
                .begin_repeatable_read_read_only
                .sql(),
        )
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;

        let row = sqlx::query(trigger_sql().subscription_change_clock.clock.sql())
            .fetch_one(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        let seq: i64 = row.get(0);
        let rows = sqlx::query(trigger_sql().subscription.live_snapshot.sql())
            .fetch_all(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        let mut records = Vec::new();
        for row in rows {
            let json: String = row.get(0);
            records.push(
                lash_core_execution::facade_support::decode_trigger_subscription_json(&json)?,
            );
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok((
            records,
            lash_core_execution::TriggerSubscriptionChangeCursor::from_store_sequence(seq as u64),
        ))
    }

    async fn compact_subscription_tombstones(
        &self,
        cutoff_epoch_ms: u64,
    ) -> Result<usize, PluginError> {
        let cutoff = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        let sql = &trigger_sql().subscription_change;
        // Lock the publication clock before selecting and deleting evidence.
        sqlx::query(trigger_sql().subscription_postgres.lock_change_clock.sql())
            .fetch_one(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?;
        let max: Option<i64> = sqlx::query_scalar(sql.compactable.sql())
            .bind(cutoff)
            .fetch_one(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?;
        let count = sqlx::query(sql.compact.sql())
            .bind(cutoff)
            .execute(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected() as usize;
        if let Some(max) = max {
            sqlx::query(trigger_sql().subscription_change_clock.horizon.sql())
                .bind(max)
                .execute(&mut **tx)
                .await
                .map_err(plugin_sqlx_error)?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(count)
    }

    async fn delete_session_subscriptions(
        &self,
        session_id: &SessionId,
    ) -> Result<usize, PluginError> {
        let sql = trigger_sql();
        let owner_scope = lash_core_execution::TriggerOwnerScope::session(session_id).namespace();
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        let rows = sqlx::query(
            sql.subscription_postgres
                .select_session_owned_for_update
                .sql(),
        )
        .bind(&owner_scope)
        .fetch_all(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
        let now = self.clock.timestamp_ms();
        for row in &rows {
            let subscription_id: String = row.get(0);
            let json: String = row.get(1);
            let mut record: TriggerSubscriptionRecord =
                lash_core_execution::facade_support::decode_trigger_subscription_json(&json)?;
            let next_revision =
                lash_core_execution::facade_support::next_trigger_store_revision(&record)?;
            record.tombstone(now);
            record.revision = next_revision;
            record.updated_at_ms = now;
            let sql_revision =
                plugin_sql_counter_value("trigger_subscription_revision", record.revision)?;
            sqlx::query(sql.subscription.tombstone.sql())
                .bind(subscription_id)
                .bind(sql_revision)
                .bind(now as i64)
                .bind(lash_core_execution::facade_support::encode_trigger_row(
                    &record,
                )?)
                .execute(&mut **tx)
                .await
                .map_err(plugin_sqlx_error)?;
            record_subscription_change(&mut tx, &record).await?;
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(rows.len())
    }

    async fn plan_occurrence(
        &self,
        request: &TriggerOccurrenceRequest,
    ) -> Result<lash_core_execution::TriggerOccurrencePlan, PluginError> {
        lash_core_execution::facade_support::validate_trigger_occurrence_request(request)?;
        let sql = trigger_sql();
        let mut conn = self.pool.acquire().await.map_err(plugin_sqlx_error)?;
        let existing: Option<String> = sqlx::query_scalar(
            sql.occurrence_postgres
                .select_record_by_idempotency_key
                .sql(),
        )
        .bind(&request.idempotency_key)
        .fetch_optional(&mut *conn)
        .await
        .map_err(plugin_sqlx_error)?;
        if let Some(json) = existing {
            let occurrence: TriggerOccurrenceRecord =
                lash_core_execution::facade_support::decode_trigger_occurrence_json(&json)?;
            if !lash_core_execution::facade_support::trigger_occurrence_request_matches_record(
                request,
                &occurrence,
            ) {
                return Err(lash_core_execution::durable_identity_conflict(format!(
                    "trigger occurrence idempotency conflict for `{}`",
                    request.idempotency_key
                )));
            }
            let reservations = postgres_delivery_snapshots(&mut conn, &occurrence).await?;
            return Ok(lash_core_execution::TriggerOccurrencePlan::Held(
                lash_core_execution::TriggerIngressReceipt {
                    occurrence,
                    reservations,
                    realization: lash_core_execution::StoreRealization::from_wrote(false),
                },
            ));
        }
        let occurrence_id =
            lash_core_execution::facade_support::deterministic_occurrence_id(request);
        // Retention reclaimed this identity: the emission is a redelivery,
        // and nothing is written back (FIG-4513).
        if postgres_occurrence_reclaimed(&mut conn, &occurrence_id).await? {
            return Err(lash_core_execution::trigger_occurrence_reclaimed(
                &occurrence_id,
            ));
        }
        let occurrence = request
            .clone()
            .into_record(occurrence_id, self.clock.timestamp_ms());
        let subscriptions = postgres_matched_subscriptions(&mut conn, &occurrence).await?;
        Ok(lash_core_execution::TriggerOccurrencePlan::Fresh {
            occurrence,
            subscriptions,
        })
    }

    async fn list_occurrences(
        &self,
        filter: lash_core_execution::TriggerOccurrenceFilter,
    ) -> Result<Vec<TriggerOccurrenceRecord>, PluginError> {
        let shape =
            OccurrenceListShape::of(filter.source_type.is_some(), filter.source_key.is_some());
        let (keys, start_ms, end_ms) = occurrence_list_bindings(&filter, shape);
        let mut query = sqlx::query(trigger_sql().occurrence.list_for(shape).sql());
        for key in keys {
            query = query.bind(key);
        }
        let rows = query
            .bind(start_ms)
            .bind(end_ms)
            .fetch_all(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        let mut records = Vec::new();
        for row in rows {
            let json: String = row.get(1);
            // The statement's window is the clamped closed one; the filter's
            // own half-open bounds, over the raw `u64`s, decide each record.
            let record: TriggerOccurrenceRecord =
                lash_core_execution::facade_support::decode_trigger_occurrence_json(&json)?;
            if filter.matches(&record) {
                records.push(record);
            }
        }
        Ok(records)
    }

    async fn list_deliveries_by_occurrence_id(
        &self,
        occurrence_id: &str,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        list_deliveries_with(
            &self.pool,
            trigger_sql().delivery.list_by_occurrence_id.sql(),
            Some(occurrence_id.to_string()),
        )
        .await
    }

    async fn list_deliveries_by_subscription_id(
        &self,
        subscription_id: &str,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        list_deliveries_with(
            &self.pool,
            trigger_sql().delivery.list_by_subscription_id.sql(),
            Some(subscription_id.to_string()),
        )
        .await
    }

    async fn list_deliveries_by_process_id(
        &self,
        process_id: &ProcessId,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        list_deliveries_with(
            &self.pool,
            trigger_sql().delivery.list_by_process_id.sql(),
            Some(process_id.to_string()),
        )
        .await
    }

    async fn list_deliveries(&self) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        list_deliveries_with(&self.pool, trigger_sql().delivery.list_all.sql(), None).await
    }

    async fn list_delivery_process_ids(&self) -> Result<Vec<ProcessId>, PluginError> {
        sqlx::query_scalar(trigger_sql().delivery.select_distinct_process_ids.sql())
            .fetch_all(&self.pool)
            .await
            .map_err(plugin_sqlx_error)
            .and_then(|ids: Vec<String>| {
                ids.iter()
                    .map(|id| crate::stored_process_id(id))
                    .collect::<Result<Vec<_>, _>>()
            })
    }

    async fn list_delivery_retention_candidates(
        &self,
    ) -> Result<Vec<lash_core_execution::TriggerDeliveryRetentionCandidate>, PluginError> {
        let rows = sqlx::query(trigger_sql().delivery.select_retention_candidates.sql())
            .fetch_all(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        rows.into_iter()
            .map(|row| {
                Ok(lash_core_execution::TriggerDeliveryRetentionCandidate {
                    occurrence_id: row.get(0),
                    subscription_id: row.get(1),
                    process_id: crate::stored_process_id(&row.get::<String, _>(2))?,
                })
            })
            .collect()
    }

    async fn list_session_owner_ids_for_retention(&self) -> Result<Vec<SessionId>, PluginError> {
        let owner_scopes: Vec<String> = sqlx::query_scalar(
            trigger_sql()
                .retention_postgres
                .select_session_owner_ids
                .sql(),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        let mut session_ids = std::collections::BTreeSet::new();
        for owner_scope in owner_scopes {
            if let Some(session_id) = owner_scope.strip_prefix("session:") {
                session_ids.insert(SessionId::parse(session_id)?);
            }
        }
        Ok(session_ids.into_iter().collect())
    }

    async fn reconcile_trigger_retention(
        &self,
        candidates: &[lash_core_execution::TriggerDeliveryRetentionCandidate],
        deleted_session_ids: &[SessionId],
    ) -> Result<lash_core_execution::TriggerRetentionReconciliationReport, PluginError> {
        let sql = trigger_sql();
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
            .map(|session_id| {
                lash_core_execution::TriggerOwnerScope::session(session_id).namespace()
            })
            .collect::<Vec<_>>();
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;

        let reclaimed_delivery_count = if candidates.is_empty() {
            0
        } else {
            sqlx::query(sql.delivery_postgres.delete_retention_candidates.sql())
                .bind(occurrence_ids)
                .bind(subscription_ids)
                .bind(
                    process_ids
                        .iter()
                        .map(ProcessId::as_str)
                        .collect::<Vec<_>>(),
                )
                .execute(&mut **tx)
                .await
                .map_err(plugin_sqlx_error)?
                .rows_affected() as usize
        };
        let reclaimed_occurrence_count =
            sqlx::query(sql.occurrence_postgres.delete_orphan_fired.sql())
                .bind(i64::try_from(self.clock.timestamp_ms()).unwrap_or(i64::MAX))
                .execute(&mut **tx)
                .await
                .map_err(plugin_sqlx_error)?
                .rows_affected() as usize;

        let now = self.clock.timestamp_ms();
        let rows = sqlx::query(
            sql.subscription_postgres
                .select_unreferenced_for_owners
                .sql(),
        )
        .bind(&deleted_owner_scopes)
        .fetch_all(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
        let mut reclaimed_subscription_ids = Vec::new();
        for row in rows {
            let json: String = row.get(0);
            let mut record =
                lash_core_execution::facade_support::decode_trigger_subscription_json(&json)?;
            if !record.is_tombstoned() {
                record.revision =
                    lash_core_execution::facade_support::next_trigger_store_revision(&record)?;
                record.tombstone(now);
            }
            record_subscription_change(&mut tx, &record).await?;
            reclaimed_subscription_ids.push(record.subscription_id);
        }
        let reclaimed_subscription_count = if reclaimed_subscription_ids.is_empty() {
            0
        } else {
            sqlx::query(sql.subscription_postgres.delete_unreferenced_by_ids.sql())
                .bind(&reclaimed_subscription_ids)
                .execute(&mut **tx)
                .await
                .map_err(plugin_sqlx_error)?
                .rows_affected() as usize
        };
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lash_core_execution::TriggerRetentionReconciliationReport {
            reclaimed_delivery_count,
            reclaimed_occurrence_count,
            reclaimed_subscription_count,
        })
    }

    async fn delete_delivery_retention_candidates(
        &self,
        candidates: &[lash_core_execution::TriggerDeliveryRetentionCandidate],
    ) -> Result<usize, PluginError> {
        if candidates.is_empty() {
            return Ok(0);
        }
        let sql = trigger_sql();
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
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        let deleted = sqlx::query(sql.delivery_postgres.delete_retention_candidates.sql())
            .bind(occurrence_ids)
            .bind(subscription_ids)
            .bind(
                process_ids
                    .iter()
                    .map(ProcessId::as_str)
                    .collect::<Vec<_>>(),
            )
            .execute(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected() as usize;
        sqlx::query(sql.occurrence_postgres.arm_reclaimable_for_candidates.sql())
            .bind(
                candidates
                    .iter()
                    .map(|candidate| candidate.occurrence_id.clone())
                    .collect::<Vec<_>>(),
            )
            .bind(armed_at_ms)
            .execute(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(deleted)
    }

    async fn reclaim_trigger_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> lash_core_execution::TriggerOccurrenceReclamationResult {
        let sql = trigger_sql();
        let cutoff_epoch_ms = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        let rows = sqlx::query(sql.occurrence_postgres.select_reclamation_scope.sql())
            .bind(cutoff_epoch_ms)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| {
                lash_core_execution::MaintenanceFailure::failed_before_any_work(Box::new(
                    plugin_sqlx_error(error),
                ))
            })?;
        let first = &rows[0];
        let mut report = lash_core_execution::TriggerOccurrenceReclamationReport {
            inspected_occurrence_count: first.get::<i64, _>(0) as usize,
            live_fan_out_count: first.get::<i64, _>(1) as usize,
            grace_deferred_count: first.get::<i64, _>(2) as usize,
            audit_retained_count: first.get::<i64, _>(3) as usize,
            ..lash_core_execution::TriggerOccurrenceReclamationReport::default()
        };
        let candidates = rows
            .into_iter()
            .filter_map(|row| row.get::<Option<String>, _>(4))
            .collect::<Vec<_>>();

        let reclaimed_at_ms = i64::try_from(self.clock.timestamp_ms()).unwrap_or(i64::MAX);
        for occurrence_id in candidates {
            let occurrence_id = occurrence_id.as_str();
            let deleted = crate::guarded_tx::guarded(&self.pool, &self.fence, |tx| {
                Box::pin(async move {
                    sqlx::query(sql.occurrence_postgres.delete_reclaimable_by_id.sql())
                        .bind(occurrence_id)
                        .bind(cutoff_epoch_ms)
                        .bind(reclaimed_at_ms)
                        .execute(tx.as_mut())
                        .await
                        .map_err(crate::store_sqlx_error)
                })
            })
            .await
            .map_err(|error| {
                lash_core_execution::MaintenanceFailure::failed(
                    Box::new(crate::plugin_store_error(error)),
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

    async fn forget_trigger_tombstones(
        &self,
        written_before_epoch_ms: u64,
    ) -> Result<usize, StoreError> {
        let signed_cutoff = i64::try_from(written_before_epoch_ms);
        let beyond_sql_range = signed_cutoff.is_err();
        let written_before_ms = signed_cutoff.unwrap_or(i64::MAX);
        let forgotten = crate::guarded_tx::guarded(&self.pool, &self.fence, |tx| {
            Box::pin(async move {
                sqlx::query(trigger_sql().tombstone.forget_written_before.sql())
                    .bind(written_before_ms)
                    .bind(beyond_sql_range)
                    .execute(tx.as_mut())
                    .await
                    .map_err(crate::store_sqlx_error)
            })
        })
        .await?;
        Ok(forgotten.rows_affected() as usize)
    }

    async fn prune_non_fired_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> Result<usize, PluginError> {
        let cutoff_epoch_ms = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        let reclaimed_at_ms = i64::try_from(self.clock.timestamp_ms()).unwrap_or(i64::MAX);
        let pruned = crate::guarded_tx::guarded(&self.pool, &self.fence, |tx| {
            Box::pin(async move {
                sqlx::query(trigger_sql().occurrence_postgres.prune_non_fired.sql())
                    .bind(cutoff_epoch_ms)
                    .bind(reclaimed_at_ms)
                    .execute(tx.as_mut())
                    .await
                    .map_err(crate::store_sqlx_error)
            })
        })
        .await
        .map_err(crate::plugin_store_error)?;
        Ok(pruned.rows_affected() as usize)
    }
}

async fn record_subscription_change(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &TriggerSubscriptionRecord,
) -> Result<(), PluginError> {
    let sql = &trigger_sql().subscription_change;
    let json = serde_json::to_string(&lash_core_execution::TriggerSubscriptionChange::from(
        record,
    ))
    .map_err(|error| PluginError::StoredDataCorrupt {
        record_kind: "TriggerSubscriptionChange".into(),
        message: error.to_string(),
    })?;
    let previous: Option<String> = sqlx::query_scalar(sql.previous.sql())
        .bind(&record.subscription_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
    if previous.as_deref() == Some(json.as_str()) {
        return Ok(());
    }
    // This transactional clock also serializes publication in commit order.
    // A PostgreSQL sequence would let an earlier uncommitted change be missed.
    let seq: Option<i64> = sqlx::query_scalar(trigger_sql().subscription_change_clock.bump.sql())
        .fetch_optional(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
    let seq = seq.ok_or_else(|| PluginError::MonotonicCounterOverflow {
        counter: "trigger_subscription_change_sequence".into(),
        current: i64::MAX as u64,
    })?;
    sqlx::query(sql.upsert.sql())
        .bind(&record.subscription_id)
        .bind(seq)
        .bind(record.lifecycle.deleted_at_ms().map(|ms| ms as i64))
        .bind(json)
        .execute(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
    Ok(())
}

/// Whether retention reclaimed `occurrence_id`, read from its tombstone.
pub(crate) async fn postgres_occurrence_reclaimed(
    conn: &mut sqlx::PgConnection,
    occurrence_id: &str,
) -> Result<bool, PluginError> {
    Ok(
        sqlx::query(trigger_sql().tombstone.select_by_occurrence_id.sql())
            .bind(occurrence_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(plugin_sqlx_error)?
            .is_some(),
    )
}

/// The enabled subscriptions a fired `occurrence` matches: none for any
/// other outcome. A malformed subscription is skipped, the same way on the
/// plan's read and the start's commit.
pub(crate) async fn postgres_matched_subscriptions(
    conn: &mut sqlx::PgConnection,
    occurrence: &TriggerOccurrenceRecord,
) -> Result<Vec<TriggerSubscriptionRecord>, PluginError> {
    if occurrence.outcome != lash_core_execution::TriggerOccurrenceOutcome::Fired {
        return Ok(Vec::new());
    }
    let sql = trigger_sql();
    let owner_scope = occurrence
        .session_id
        .clone()
        .map(|session_id| lash_core_execution::TriggerOwnerScope::session(session_id).namespace());
    let statement = match &owner_scope {
        Some(_) => {
            &sql.subscription_postgres
                .select_enabled_for_source_and_owner
        }
        None => &sql.subscription_postgres.select_enabled_for_source,
    };
    let mut query = sqlx::query(statement.sql())
        .bind(&occurrence.source_type)
        .bind(&occurrence.source_key);
    if let Some(owner_scope) = owner_scope {
        query = query.bind(owner_scope);
    }
    let rows = query
        .fetch_all(&mut *conn)
        .await
        .map_err(plugin_sqlx_error)?;
    let mut subscriptions = Vec::new();
    for row in rows {
        let subscription_id: String = row.get(0);
        let json: String = row.get(1);
        match lash_core_execution::facade_support::decode_trigger_subscription_json(&json) {
            Ok(subscription) => subscriptions.push(subscription),
            Err(err) => tracing::warn!(
                error = %err,
                subscription_id,
                "skipping malformed trigger subscription during occurrence ingress"
            ),
        }
    }
    lash_core_execution::facade_support::sort_trigger_subscriptions(&mut subscriptions);
    Ok(subscriptions)
}

async fn postgres_delivery_snapshots(
    conn: &mut sqlx::PgConnection,
    occurrence: &TriggerOccurrenceRecord,
) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
    let rows = sqlx::query(trigger_sql().delivery.select_snapshots_by_occurrence.sql())
        .bind(&occurrence.occurrence_id)
        .fetch_all(&mut *conn)
        .await
        .map_err(plugin_sqlx_error)?;
    let mut reservations = rows
        .into_iter()
        .map(|row| {
            let json: String = row.get(2);
            Ok(TriggerDeliveryReservation {
                occurrence: occurrence.clone(),
                subscription:
                    lash_core_execution::facade_support::decode_trigger_subscription_json(&json)?,
                process_id: crate::stored_process_id(&row.get::<String, _>(0))?,
                created_at_ms: plugin_u64_from_sql("TriggerDelivery", "created_at_ms", row.get(1))?,
            })
        })
        .collect::<Result<Vec<_>, PluginError>>()?;
    lash_core_execution::facade_support::sort_trigger_delivery_reservations(&mut reservations);
    Ok(reservations)
}

/// `sql` is a rendered statement, never a clause this function completes: the
/// listing used to be one `format!` over a `where_clause` argument, and each
/// caller now names the statement it means.
async fn list_deliveries_with(
    pool: &sqlx::PgPool,
    sql: &'static str,
    value: Option<String>,
) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
    let mut query = sqlx::query(sql);
    if let Some(value) = value {
        query = query.bind(value);
    }
    let rows = query.fetch_all(pool).await.map_err(plugin_sqlx_error)?;
    rows.into_iter()
        .map(|row| {
            let occurrence_json: String = row.get(2);
            let subscription_json: String = row.get(3);
            lash_core_execution::facade_support::decode_trigger_delivery(
                &occurrence_json,
                &subscription_json,
                crate::stored_process_id(&row.get::<String, _>(0))?,
                row.get(1),
            )
        })
        .collect()
}
