//! SQLite-backed runtime trigger store, and the SQLite owner of the trigger
//! family.
//!
//! This is the durable peer of [`SqliteProcessRegistry`]: it stores trigger
//! subscriptions and append-only trigger occurrences at deployment scope,
//! outside any session database.
//!
//! The family lives in its own database file, which this store opens directly
//! and never attaches anywhere, so every statement is rendered once, through
//! the unqualified SQLite dialect — the text this store has always issued.
//! The effect family needs a schema qualifier because its tables are also
//! reached through an `ATTACH`ed name; this one does not.

use super::*;
use lash_sansio::{ProcessId, SessionId};
use lash_store_sql::trigger::{
    deliveries::DeliveryStatements,
    mutation_receipts::MutationReceiptStatements,
    occurrence_tombstones::OccurrenceTombstoneStatements,
    occurrences::{ListShape as OccurrenceListShape, OccurrenceStatements},
    subscriptions::{ListShape as SubscriptionListShape, SubscriptionStatements},
};
use std::sync::LazyLock;

#[path = "triggers/subscription_changes.rs"]
mod subscription_changes;
use subscription_changes::record_subscription_change;

lash_store_sql::statements! {
    /// `trigger_subscriptions` statements only SQLite issues.
    pub(crate) struct SubscriptionSqliteStatements @ "trigger_subscription" {
        /// The record of subscription `?1`, read before a mutation is
        /// evaluated against it.
        ///
        /// No `FOR UPDATE`: `BEGIN IMMEDIATE` already holds the database write
        /// lock, so the read and the write it decides cannot interleave.
        select_record_by_id = "SELECT record_json FROM trigger_subscriptions
             WHERE subscription_id = ?1";

        /// Every live record owned by scope `?1`, the input a prune evaluates.
        /// Forks for the same reason
        /// [`SubscriptionSqliteStatements::select_record_by_id`] does.
        select_records_for_prune = "SELECT record_json FROM trigger_subscriptions
             WHERE owner_scope = ?1 AND lifecycle <> 'tombstoned'";

        /// Every enabled subscription an occurrence of `?1`/`?2` fires at.
        ///
        /// Plain equalities, so the read seeks
        /// `(source_type, source_key, lifecycle)` on all three columns — this
        /// is the ingress path, and it runs once per firing. PostgreSQL adds
        /// `FOR SHARE` so a concurrent mutation cannot retire a subscription
        /// between this read and the delivery it reserves; SQLite holds the
        /// write lock for the whole ingress.
        select_enabled_for_source = "SELECT subscription_id, record_json
             FROM trigger_subscriptions
             WHERE lifecycle = 'enabled'
               AND source_type = ?1
               AND source_key = ?2
             ORDER BY owner_scope ASC, subscription_key ASC";

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
             ORDER BY owner_scope ASC, subscription_key ASC";

        /// Every subscription in the store, tombstoned ones included.
        ///
        /// SQLite alone reads the whole table to delete a session's
        /// subscriptions: it decides ownership from the decoded record's
        /// registrant session rather than from the `owner_scope` column, and
        /// skips a row whose JSON is malformed with a warning instead of
        /// failing the sweep. PostgreSQL selects by `owner_scope` under
        /// `FOR UPDATE` and has no counterpart. The two are deliberately left
        /// as they stand; closing the difference is a behaviour change, not a
        /// rendering one.
        select_all_for_session_sweep = "SELECT subscription_id, record_json
             FROM trigger_subscriptions";

        /// Read every subscription of the owner scopes in JSON array `?1`
        /// that no delivery still references, before recording deletion evidence.
        ///
        /// SQLite unnests the array with `json_each`; PostgreSQL binds a real
        /// `TEXT[]`.
        select_unreferenced_for_owners = "SELECT record_json FROM trigger_subscriptions AS subscription
             WHERE owner_scope IN (SELECT value FROM json_each(?1))
               AND NOT EXISTS (SELECT 1 FROM trigger_deliveries WHERE trigger_deliveries.subscription_id = subscription.subscription_id)";

        delete_unreferenced_for_owners = "DELETE FROM trigger_subscriptions
             WHERE owner_scope IN (SELECT value FROM json_each(?1))
               AND NOT EXISTS (
                   SELECT 1 FROM trigger_deliveries
                   WHERE trigger_deliveries.subscription_id =
                         trigger_subscriptions.subscription_id
               )";
    }
}

lash_store_sql::statements! {
    /// `trigger_occurrences` statements only SQLite issues.
    pub(crate) struct OccurrenceSqliteStatements @ "trigger_occurrence" {
        /// The occurrence already stored under idempotency key `?1`.
        ///
        /// PostgreSQL takes the row's write lock (`FOR UPDATE`) to hold the
        /// idempotency comparison across the insert that follows it; SQLite
        /// reads it under `BEGIN IMMEDIATE`.
        select_record_by_idempotency_key = "SELECT record_json
             FROM trigger_occurrences
             WHERE idempotency_key = ?1";

        /// Tombstone, at `?1`, every occurrence [`delete_orphan_fired`]
        /// deletes next in the same write transaction.
        ///
        /// SQLite writes the tombstone and deletes the row as two statements
        /// under its single writer; PostgreSQL does both in one statement.
        ///
        /// [`delete_orphan_fired`]: Self::delete_orphan_fired
        tombstone_orphan_fired = "INSERT INTO trigger_occurrence_tombstones (
                occurrence_id, reclaimed_at_ms
             )
             SELECT occurrence_id, ?1
             FROM trigger_occurrences
             WHERE outcome_kind = 'fired'
               AND NOT EXISTS (
                   SELECT 1 FROM trigger_deliveries
                   WHERE trigger_deliveries.occurrence_id =
                         trigger_occurrences.occurrence_id
               )";

        /// Only fired occurrences participate in delivery retention.
        delete_orphan_fired = "DELETE FROM trigger_occurrences
             WHERE outcome_kind = 'fired'
               AND NOT EXISTS (
                   SELECT 1 FROM trigger_deliveries
                   WHERE trigger_deliveries.occurrence_id =
                         trigger_occurrences.occurrence_id
               )";

        arm_reclaimable_for_candidates = "UPDATE trigger_occurrences
             SET reclaimable_at_ms = ?2
             WHERE reclaimable_at_ms IS NULL
               AND outcome_kind = 'fired'
               AND NOT EXISTS (
                   SELECT 1 FROM trigger_deliveries
                   WHERE trigger_deliveries.occurrence_id =
                         trigger_occurrences.occurrence_id
               )
               AND occurrence_id IN (
                   SELECT DISTINCT json_extract(candidate.value, '$.occurrence_id')
                   FROM json_each(?1) AS candidate
               )";

        /// The reclamation sweep's scope proof and its worklist, from one
        /// snapshot at cutoff `?1`.
        ///
        /// The aggregate visits the whole table so `NothingToDo` stays witnessed emptiness;
        /// only eligible ids are materialized.
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

        /// Tombstone occurrence `?1`, at `?3`, if [`delete_reclaimable_by_id`]
        /// deletes it next at cutoff `?2`.
        ///
        /// [`delete_reclaimable_by_id`]: Self::delete_reclaimable_by_id
        tombstone_reclaimable_by_id = "INSERT INTO trigger_occurrence_tombstones (
                occurrence_id, reclaimed_at_ms
             )
             SELECT occurrence_id, ?3
             FROM trigger_occurrences
             WHERE occurrence_id = ?1
               AND reclaimable_at_ms IS NOT NULL
               AND reclaimable_at_ms <= ?2
               AND outcome_kind = 'fired'
               AND NOT EXISTS (
                   SELECT 1 FROM trigger_deliveries
                   WHERE trigger_deliveries.occurrence_id =
                         trigger_occurrences.occurrence_id
               )";

        /// Reclaim occurrence `?1` if it is still eligible at cutoff `?2`.
        /// The whole eligibility test is re-proved here, because the worklist was read from an
        /// earlier snapshot.
        delete_reclaimable_by_id = "DELETE FROM trigger_occurrences
             WHERE occurrence_id = ?1
               AND reclaimable_at_ms IS NOT NULL
               AND reclaimable_at_ms <= ?2
               AND outcome_kind = 'fired'
               AND NOT EXISTS (
                   SELECT 1 FROM trigger_deliveries
                   WHERE trigger_deliveries.occurrence_id =
                         trigger_occurrences.occurrence_id
               )";

        /// Tombstone, at `?2`, every audit row [`prune_non_fired`] deletes
        /// next at cutoff `?1`.
        ///
        /// [`prune_non_fired`]: Self::prune_non_fired
        tombstone_non_fired = "INSERT INTO trigger_occurrence_tombstones (
                occurrence_id, reclaimed_at_ms
             )
             SELECT occurrence_id, ?2
             FROM trigger_occurrences
             WHERE occurred_at_ms < ?1
               AND outcome_kind != 'fired'";

        prune_non_fired = "DELETE FROM trigger_occurrences
             WHERE occurred_at_ms < ?1
               AND outcome_kind != 'fired'";
    }
}

lash_store_sql::statements! {
    /// `trigger_deliveries` statements only SQLite issues.
    pub(crate) struct DeliverySqliteStatements @ "trigger_delivery" {
        /// SQLite unnests the candidates with `json_each` and compares the
        /// three ids through `json_extract`; PostgreSQL joins three bound
        /// `TEXT[]`s with `UNNEST`.
        delete_retention_candidates = "DELETE FROM trigger_deliveries
             WHERE EXISTS (
                 SELECT 1
                 FROM json_each(?1) AS candidate
                 WHERE trigger_deliveries.occurrence_id =
                           json_extract(candidate.value, '$.occurrence_id')
                   AND trigger_deliveries.subscription_id =
                           json_extract(candidate.value, '$.subscription_id')
                   AND trigger_deliveries.process_id =
                           json_extract(candidate.value, '$.process_id')
             )";
    }
}

lash_store_sql::statements! {
    /// The trigger family's cross-table retention read, as SQLite issues it.
    pub(crate) struct RetentionSqliteStatements @ "trigger_retention" {
        /// Every session that owns a subscription, a delivery's frozen
        /// subscription, or a mutation receipt: the candidate set a session
        /// retention pass reconciles against the session catalog.
        ///
        /// The one statement of this family that reads all three tables, and
        /// it forks on the JSON read in the delivery arm.
        select_session_owner_ids = "SELECT owner_scope
             FROM (
                 SELECT owner_scope
                 FROM trigger_subscriptions
                 UNION
                 SELECT 'session:' || json_extract(
                            subscription_snapshot_json,
                            '$.owner_scope.session_id'
                        )
                 FROM trigger_deliveries
                 WHERE json_extract(
                           subscription_snapshot_json,
                           '$.owner_scope.type'
                       ) = 'session'
                 UNION
                 SELECT 'session:' || owner_id
                 FROM trigger_mutation_receipts
                 WHERE owner_kind = 'session'
             )
             WHERE owner_scope LIKE 'session:%'
             ORDER BY owner_scope";

        /// The session owners with a mutation receipt older than bound `?1`:
        /// the sweep's candidate set, filtered against the durable-core
        /// session catalog before it deletes (FIG-4108).
        select_receipt_session_owners = "SELECT DISTINCT owner_id
             FROM trigger_mutation_receipts
             WHERE owner_kind = 'session'
               AND created_at_ms < ?1";

        /// The host retention lever's receipt sweep (FIG-4108): receipts
        /// older than bound `?1` go when they are ownerless or their session
        /// owner is durably deleted — the caller enumerates and filters those
        /// owner ids into `?2` — unless a delivery still names the owner.
        reclaim_mutation_receipts = "DELETE FROM trigger_mutation_receipts
             WHERE created_at_ms < ?1
               AND (
                   owner_kind IN ('host', 'platform')
                   OR (
                       owner_kind = 'session'
                       AND owner_id IN (SELECT value FROM json_each(?2))
                       AND NOT EXISTS (
                           SELECT 1 FROM trigger_deliveries
                           WHERE json_extract(
                                     subscription_snapshot_json,
                                     '$.owner_scope.type'
                                 ) = 'session'
                             AND json_extract(
                                     subscription_snapshot_json,
                                     '$.owner_scope.session_id'
                                 ) = trigger_mutation_receipts.owner_id
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
    subscription: SubscriptionStatements,
    /// `trigger_subscriptions` statements only SQLite issues.
    subscription_sqlite: SubscriptionSqliteStatements,
    /// `trigger_occurrences` statements both backends issue verbatim.
    occurrence: OccurrenceStatements,
    /// `trigger_occurrences` statements only SQLite issues.
    occurrence_sqlite: OccurrenceSqliteStatements,
    /// `trigger_occurrence_tombstones` statements both backends issue
    /// verbatim.
    tombstone: OccurrenceTombstoneStatements,
    /// `trigger_deliveries` statements both backends issue verbatim.
    delivery: DeliveryStatements,
    /// `trigger_deliveries` statements only SQLite issues.
    delivery_sqlite: DeliverySqliteStatements,
    /// `trigger_mutation_receipts` statements both backends issue verbatim.
    receipt: MutationReceiptStatements,
    /// The family's cross-table retention statements, also issued by
    /// `retention.rs` on its own connection to this database.
    pub(crate) retention_sqlite: RetentionSqliteStatements,
}

static TRIGGER_SQL: LazyLock<TriggerSql> = LazyLock::new(|| {
    let dialect = lash_store_sql::Dialect::sqlite_unqualified();
    TriggerSql {
        subscription_change:
            lash_store_sql::trigger::subscription_changes::SubscriptionChangeStatements::render(
                dialect,
            ),
        subscription_change_clock: lash_store_sql::trigger::subscription_change_clock::SubscriptionChangeClockStatements::render(dialect),
        subscription: SubscriptionStatements::render(dialect),
        subscription_sqlite: SubscriptionSqliteStatements::render(dialect),
        occurrence: OccurrenceStatements::render(dialect),
        occurrence_sqlite: OccurrenceSqliteStatements::render(dialect),
        tombstone: OccurrenceTombstoneStatements::render(dialect),
        delivery: DeliveryStatements::render(dialect),
        delivery_sqlite: DeliverySqliteStatements::render(dialect),
        receipt: MutationReceiptStatements::render(dialect),
        retention_sqlite: RetentionSqliteStatements::render(dialect),
    }
});

/// The trigger-family statements, rendered at first use and never again.
///
/// One set, not one per schema: the trigger database is never attached to
/// another connection, so these tables are never addressed through a
/// qualifier. `retention.rs` issues `retention_sqlite` on the connection it
/// opens to the trigger database, where the same unqualified names hold.
pub(crate) fn trigger_sql() -> &'static TriggerSql {
    &TRIGGER_SQL
}

/// The rendered listing statement `filter`'s shape is served by, for the
/// conformance assertion that the owner filter is pushed into SQL rather than
/// applied in Rust.
#[cfg(any(test, feature = "testing"))]
pub(crate) fn subscription_list_sql(
    filter: &lash_core_execution::TriggerSubscriptionFilter,
) -> &'static str {
    trigger_sql()
        .subscription
        .list_for(subscription_list_shape(filter))
        .sql()
}

/// Planner witnesses for the named listings, and the dispatch that picks them.
#[cfg(test)]
#[path = "triggers/listing_plan_tests.rs"]
mod listing_plan_tests;

pub struct SqliteTriggerStore {
    pub(crate) conn: SqliteConnection,
    /// Held so a store opened on a memory backend keeps its database alive.
    _location: crate::location::DatabaseLocation,
    clock: Arc<dyn lash_core_execution::Clock>,
    fixed_incarnation: Option<String>,
}

impl SqliteTriggerStore {
    pub async fn open(path: &Path) -> tokio_rusqlite::Result<Self> {
        Self::open_with_clock(
            path,
            Arc::new(lash_core_execution::facade_support::SystemClock),
        )
        .await
    }

    pub async fn open_with_clock(
        path: &Path,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        crate::location::validate_file_database_path(path, "SqliteTriggerStore")?;
        Self::open_at(
            &crate::location::DatabaseLocation::standalone_file(path),
            clock,
        )
        .await
    }

    pub(crate) async fn open_at(
        location: &crate::location::DatabaseLocation,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        let conn = SqliteConnection::open(location.target()).await?;
        ensure_versioned_schema(&conn, SqliteDatabase::Triggers).await?;
        apply_pragmas(&conn).await?;
        Ok(Self {
            conn,
            _location: location.clone(),
            clock,
            fixed_incarnation: None,
        })
    }

    /// Pin otherwise-random trigger incarnation identity for durable fixture generation.
    pub fn with_incarnation_for_testing(mut self, incarnation: impl Into<String>) -> Self {
        self.fixed_incarnation = Some(incarnation.into());
        self
    }

    /// `sql` is a rendered statement, never a clause this function completes:
    /// the listing used to be one `format!` over a `where_clause` argument,
    /// and each caller now names the statement it means.
    async fn list_deliveries_with(
        &self,
        sql: &'static str,
        values: Vec<rusqlite::types::Value>,
    ) -> Result<
        Vec<lash_core_execution::TriggerDeliveryReservation>,
        lash_core_execution::PluginError,
    > {
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let mut stmt = conn.prepare_cached(sql).map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                            Ok((
                                row.get::<_, Option<String>>(0)?
                                    .map(|value| crate::sql_process_id(0, value))
                                    .transpose()?,
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
                        deliveries.push(
                            lash_core_execution::facade_support::decode_trigger_delivery(
                                &occurrence_json,
                                &subscription_json,
                                process_id,
                                created_at_ms,
                            )?,
                        );
                    }
                    Ok(deliveries)
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }
}

fn trigger_tx_outcome<T>(
    result: Result<T, lash_core_execution::PluginError>,
) -> TxOutcome<Result<T, lash_core_execution::PluginError>> {
    match result {
        Ok(value) => TxOutcome::Commit(Ok(value)),
        Err(err) => TxOutcome::Rollback(Err(err)),
    }
}

/// The listing statement shape `filter` is served by.
fn subscription_list_shape(
    filter: &lash_core_execution::TriggerSubscriptionFilter,
) -> SubscriptionListShape {
    SubscriptionListShape::of(
        filter.registrant_scope_id.is_some(),
        filter.subscription_key.is_some(),
        filter.source_type.is_some(),
        filter.source_key.is_some(),
    )
}

/// What the statement of `shape` binds, in its parameter order.
///
/// Exhaustive over the shape, so a new listing statement cannot be added
/// without deciding what it binds.
fn subscription_list_values(
    filter: &lash_core_execution::TriggerSubscriptionFilter,
    shape: SubscriptionListShape,
) -> Vec<rusqlite::types::Value> {
    let text =
        |value: &Option<String>| rusqlite::types::Value::Text(value.clone().unwrap_or_default());
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

/// What the occurrence listing of `shape` binds, in its parameter order.
///
/// The window is always bound: an unset start is `i64::MIN` and an unset end
/// `i64::MAX`, so the comparison stays a plain one against a value and the
/// index range survives. The closed `[start, end]` this produces is a superset
/// of the filter's half-open `[start, end)`, which
/// `TriggerOccurrenceFilter::matches` then narrows exactly.
fn occurrence_list_values(
    filter: &lash_core_execution::TriggerOccurrenceFilter,
    shape: OccurrenceListShape,
) -> Vec<rusqlite::types::Value> {
    let text =
        |value: &Option<String>| rusqlite::types::Value::Text(value.clone().unwrap_or_default());
    let mut values = match shape {
        OccurrenceListShape::All => Vec::new(),
        OccurrenceListShape::BySourceType => vec![text(&filter.source_type)],
        OccurrenceListShape::BySource => {
            vec![text(&filter.source_type), text(&filter.source_key)]
        }
    };
    values.push(rusqlite::types::Value::Integer(
        filter
            .occurred_at_start_ms
            .map_or(i64::MIN, crate::clamp_epoch_ms),
    ));
    values.push(rusqlite::types::Value::Integer(
        filter
            .occurred_at_end_ms
            .map_or(i64::MAX, crate::clamp_epoch_ms),
    ));
    values
}

#[async_trait::async_trait]
impl lash_core_execution::TriggerStore for SqliteTriggerStore {
    async fn execute_command(
        &self,
        operation_id: &str,
        command: lash_core_execution::TriggerCommand,
    ) -> Result<lash_core_execution::TriggerEffectResult, lash_core_execution::PluginError> {
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
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let sql = trigger_sql();
                Ok(trigger_tx_outcome((|| {
                    let receipt: Option<(String, String)> = tx
                        .query_row(
                            sql.receipt.select_by_operation_id.sql(),
                            params![preparation.receipt_id.as_str()],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()
                        .map_err(process_sqlite_error)?;
                    if let Some((stored_hash, json)) = receipt {
                        return lash_core_execution::facade_support::stored_trigger_receipt(
                            stored_hash,
                            &json,
                            &preparation,
                        );
                    }
                    let result = if let lash_core_execution::TriggerCommand::Prune {
                        owner_scope,
                        actor,
                        subscription_keys,
                    } = &*command
                    {
                        let mut stmt = tx
                            .prepare(sql.subscription_sqlite.select_records_for_prune.sql())
                            .map_err(process_sqlite_error)?;
                        let rows = stmt
                            .query_map(params![owner_scope.namespace()], |row| {
                                row.get::<_, String>(0)
                            })
                            .map_err(process_sqlite_error)?;
                        let mut records = Vec::new();
                        for row in rows {
                            records.push(
                                lash_core_execution::facade_support::decode_trigger_subscription_json(
                                    &row.map_err(process_sqlite_error)?,
                                )?,
                            );
                        }
                        drop(stmt);
                        lash_core_execution::facade_support::evaluate_trigger_prune(
                            records,
                            owner_scope.clone(),
                            actor.clone(),
                            subscription_keys.clone(),
                            now,
                        )
                    } else {
                        let current = tx
                            .query_row(
                                sql.subscription_sqlite.select_record_by_id.sql(),
                                params![preparation.subscription_id.as_str()],
                                |row| row.get::<_, String>(0),
                            )
                            .optional()
                            .map_err(process_sqlite_error)?
                            .map(|json| {
                                lash_core_execution::facade_support::decode_trigger_subscription_json(
                                    &json,
                                )
                            })
                            .transpose()?;
                        lash_core_execution::facade_support::evaluate_trigger_mutation_with_incarnation(
                            current,
                            *command,
                            now,
                            preparation.incarnation.clone(),
                        )?
                    };
                    for record in lash_core_execution::facade_support::trigger_mutation_records(
                        &result,
                    ) {
                        let sql_revision = plugin_sql_counter_value(
                            "trigger_subscription_revision",
                            record.revision,
                        )?;
                        crate::conn::cached_execute(tx,
                            sql.subscription.upsert.sql(),
                            params![
                                record.subscription_id.as_str(),
                                record.owner_scope.namespace(),
                                record.subscription_key.as_str(),
                                record.incarnation.as_str(),
                                sql_revision,
                                record.definition_fingerprint.as_str(),
                                record.source_type.as_str(),
                                record.source_key.as_str(),
                                record.lifecycle.as_column(),
                                record.lifecycle.deleted_at_ms().map(|ms| ms as i64),
                                record.created_at_ms as i64,
                                record.updated_at_ms as i64,
                                lash_core_execution::facade_support::encode_trigger_row(&record)?,
                            ],
                        )
                        .map_err(process_sqlite_error)?;
                        record_subscription_change(tx, record)?;
                    }
                    crate::conn::cached_execute(tx,
                        sql.receipt.insert.sql(),
                        params![
                            preparation.receipt_id.as_str(),
                            preparation.owner_scope.owner_kind_column(),
                            preparation.owner_scope.owner_id_column(),
                            preparation.request_fingerprint.as_str(),
                            lash_core_execution::facade_support::encode_trigger_row(&result)?,
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
        filter: lash_core_execution::TriggerSubscriptionFilter,
    ) -> Result<Vec<lash_core_execution::TriggerSubscriptionRecord>, lash_core_execution::PluginError>
    {
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let shape = subscription_list_shape(&filter);
                    let values = subscription_list_values(&filter, shape);
                    let mut stmt = conn
                        .prepare(trigger_sql().subscription.list_for(shape).sql())
                        .map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })
                        .map_err(process_sqlite_error)?;
                    let mut records = Vec::new();
                    for row in rows {
                        let (subscription_id, json) = row.map_err(process_sqlite_error)?;
                        let record = match lash_core_execution::facade_support::decode_trigger_subscription_json(&json)
                        {
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

    async fn subscriptions_changed_since(
        &self,
        cursor: lash_core_execution::TriggerSubscriptionChangeCursor,
        limit: usize,
    ) -> Result<
        (
            Vec<lash_core_execution::TriggerSubscriptionChange>,
            lash_core_execution::TriggerSubscriptionChangeCursor,
        ),
        lash_core_execution::PluginError,
    > {
        subscription_changes::changed_since(self, cursor, limit).await
    }

    async fn list_subscriptions_with_cursor(
        &self,
    ) -> Result<
        (
            Vec<lash_core_execution::TriggerSubscriptionRecord>,
            lash_core_execution::TriggerSubscriptionChangeCursor,
        ),
        lash_core_execution::PluginError,
    > {
        subscription_changes::snapshot(self).await
    }

    async fn compact_subscription_tombstones(
        &self,
        cutoff_epoch_ms: u64,
    ) -> Result<usize, lash_core_execution::PluginError> {
        subscription_changes::compact(self, cutoff_epoch_ms).await
    }

    async fn delete_session_subscriptions(
        &self,
        session_id: &SessionId,
    ) -> Result<usize, lash_core_execution::PluginError> {
        let session_id = session_id.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let sql = trigger_sql();
                Ok(trigger_tx_outcome((|| {
                    let mut stmt = tx
                        .prepare(sql.subscription_sqlite.select_all_for_session_sweep.sql())
                        .map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map([], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })
                        .map_err(process_sqlite_error)?;
                    let mut subscriptions = Vec::new();
                    for row in rows {
                        let (subscription_id, json) = row.map_err(process_sqlite_error)?;
                        let record = match lash_core_execution::facade_support::decode_trigger_subscription_json(&json)
                        {
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
                        if record.registrant_session_id()
                            == Some(&session_id)
                            && !record.is_tombstoned()
                        {
                            subscriptions.push((subscription_id, record));
                        }
                    }
                    drop(stmt);
                    let mut deleted = 0usize;
                    for (subscription_id, mut record) in subscriptions {
                        let next_revision =
                            lash_core_execution::facade_support::next_trigger_store_revision(
                                &record,
                            )?;
                        record.tombstone(now);
                        record.revision = next_revision;
                        record.updated_at_ms = now;
                        let sql_revision = plugin_sql_counter_value(
                            "trigger_subscription_revision",
                            record.revision,
                        )?;
                        deleted += tx
                            .execute(
                                sql.subscription.tombstone.sql(),
                                params![
                                    subscription_id.as_str(),
                                    sql_revision,
                                    now as i64,
                                    lash_core_execution::facade_support::encode_trigger_row(
                                        &record,
                                    )?,
                                ],
                            )
                            .map_err(process_sqlite_error)?;
                        record_subscription_change(tx, &record)?;
                    }
                    Ok(deleted)
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn ingest_occurrence(
        &self,
        request: lash_core_execution::TriggerOccurrenceRequest,
    ) -> Result<lash_core_execution::TriggerIngressReceipt, lash_core_execution::PluginError> {
        lash_core_execution::facade_support::validate_trigger_occurrence_request(&request)?;
        let occurrence_id =
            lash_core_execution::facade_support::deterministic_occurrence_id(&request);
        let occurred_at_ms = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let sql = trigger_sql();
                Ok(trigger_tx_outcome((|| {
                    let existing: Option<String> = tx
                        .query_row(
                            sql.occurrence_sqlite.select_record_by_idempotency_key.sql(),
                            params![request.idempotency_key.as_str()],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(process_sqlite_error)?;
                    let (record, is_new) = if let Some(existing_json) = existing {
                        let record =
                            lash_core_execution::facade_support::decode_trigger_occurrence_json(
                                &existing_json,
                            )?;
                        if !lash_core_execution::facade_support::trigger_occurrence_request_matches_record(
                            &request, &record,
                        ) {
                            return Err(lash_core_execution::durable_identity_conflict(format!(
                                "trigger occurrence idempotency conflict for `{}`",
                                request.idempotency_key
                            )));
                        }
                        (record, false)
                    } else {
                        // Retention reclaimed this identity: the ingest is a
                        // redelivery, and writes nothing back (FIG-4513).
                        let reclaimed: Option<i64> = tx
                            .query_row(
                                sql.tombstone.select_by_occurrence_id.sql(),
                                params![occurrence_id.as_str()],
                                |row| row.get(0),
                            )
                            .optional()
                            .map_err(process_sqlite_error)?;
                        if reclaimed.is_some() {
                            return Err(lash_core_execution::trigger_occurrence_reclaimed(
                                &occurrence_id,
                            ));
                        }
                        let record = request.into_record(occurrence_id.clone(), occurred_at_ms);
                        crate::conn::cached_execute(tx,
                            sql.occurrence.insert.sql(),
                            params![
                                record.occurrence_id.as_str(),
                                record.idempotency_key.as_str(),
                                record.source_type.as_str(),
                                record.source_key.as_str(),
                                record.occurred_at_ms as i64,
                                record.outcome.kind(),
                                lash_core_execution::facade_support::encode_trigger_row(&record)?,
                            ],
                        )
                        .map_err(process_sqlite_error)?;
                        (record, true)
                    };
                    let reservations = match (
                        is_new,
                        record.outcome == lash_core_execution::TriggerOccurrenceOutcome::Fired,
                    ) {
                        (true, true) => reserve_sqlite_deliveries(tx, &record, occurred_at_ms)?,
                        (false, true) => sqlite_delivery_snapshots(tx, &record)?,
                        (_, false) => Vec::new(),
                    };
                    if is_new
                        && record.outcome == lash_core_execution::TriggerOccurrenceOutcome::Fired
                        && reservations.is_empty()
                    {
                        crate::conn::cached_execute(tx,
                            sql.occurrence.arm_reclaimable.sql(),
                            params![record.occurrence_id.as_str(), record.occurred_at_ms as i64],
                        )
                        .map_err(process_sqlite_error)?;
                    }
                    Ok(lash_core_execution::TriggerIngressReceipt {
                        occurrence: record,
                        reservations,
                        realization: lash_core_execution::StoreRealization::from_wrote(is_new),
                    })
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn list_occurrences(
        &self,
        filter: lash_core_execution::TriggerOccurrenceFilter,
    ) -> Result<Vec<lash_core_execution::TriggerOccurrenceRecord>, lash_core_execution::PluginError>
    {
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let shape = OccurrenceListShape::of(
                        filter.source_type.is_some(),
                        filter.source_key.is_some(),
                    );
                    let values = occurrence_list_values(&filter, shape);
                    let mut stmt = conn
                        .prepare(trigger_sql().occurrence.list_for(shape).sql())
                        .map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })
                        .map_err(process_sqlite_error)?;
                    let mut records = Vec::new();
                    for row in rows {
                        let (_, json) = row.map_err(process_sqlite_error)?;
                        // The statement's window is the clamped closed one;
                        // the filter's own half-open bounds, over the raw
                        // `u64`s, decide each record.
                        let record =
                            lash_core_execution::facade_support::decode_trigger_occurrence_json(
                                &json,
                            )?;
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

    async fn list_deliveries_by_occurrence_id(
        &self,
        occurrence_id: &str,
    ) -> Result<
        Vec<lash_core_execution::TriggerDeliveryReservation>,
        lash_core_execution::PluginError,
    > {
        self.list_deliveries_with(
            trigger_sql().delivery.list_by_occurrence_id.sql(),
            vec![occurrence_id.to_string().into()],
        )
        .await
    }

    async fn list_deliveries_by_subscription_id(
        &self,
        subscription_id: &str,
    ) -> Result<
        Vec<lash_core_execution::TriggerDeliveryReservation>,
        lash_core_execution::PluginError,
    > {
        self.list_deliveries_with(
            trigger_sql().delivery.list_by_subscription_id.sql(),
            vec![subscription_id.to_string().into()],
        )
        .await
    }

    async fn list_deliveries_by_process_id(
        &self,
        process_id: &ProcessId,
    ) -> Result<
        Vec<lash_core_execution::TriggerDeliveryReservation>,
        lash_core_execution::PluginError,
    > {
        self.list_deliveries_with(
            trigger_sql().delivery.list_by_process_id.sql(),
            vec![process_id.to_string().into()],
        )
        .await
    }

    async fn list_deliveries(
        &self,
    ) -> Result<
        Vec<lash_core_execution::TriggerDeliveryReservation>,
        lash_core_execution::PluginError,
    > {
        self.list_deliveries_with(trigger_sql().delivery.list_all.sql(), Vec::new())
            .await
    }

    async fn bind_delivery_process(
        &self,
        occurrence_id: &str,
        subscription_id: &str,
        process_id: &ProcessId,
    ) -> Result<(), lash_core_execution::PluginError> {
        let occurrence_id = occurrence_id.to_string();
        let subscription_id = subscription_id.to_string();
        let process_id = process_id.clone();
        let bound_at_ms = self.clock.timestamp_ms();
        self.conn
            .write(move |conn| {
                Ok((|| {
                    let bound = conn
                        .execute(
                            trigger_sql().delivery.bind_process.sql(),
                            params![
                                occurrence_id.as_str(),
                                subscription_id.as_str(),
                                process_id.as_str(),
                                bound_at_ms as i64,
                            ],
                        )
                        .map_err(process_sqlite_error)?;
                    if bound == 1 {
                        Ok(())
                    } else {
                        Err(lash_core_execution::durable_identity_conflict(format!(
                            "trigger delivery `{occurrence_id}`/`{subscription_id}` is absent or already bound to another process than `{process_id}`"
                        )))
                    }
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn list_delivery_process_ids(
        &self,
    ) -> Result<Vec<ProcessId>, lash_core_execution::PluginError> {
        self.conn
            .call(|conn| {
                Ok((|| {
                    let mut stmt = conn
                        .prepare(trigger_sql().delivery.select_distinct_process_ids.sql())
                        .map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map([], |row| crate::row_process_id(row, 0))
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
    ) -> Result<
        Vec<lash_core_execution::TriggerDeliveryRetentionCandidate>,
        lash_core_execution::PluginError,
    > {
        self.conn
            .call(|conn| {
                Ok((|| {
                    let mut stmt = conn
                        .prepare(trigger_sql().delivery.select_retention_candidates.sql())
                        .map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map([], |row| {
                            Ok(lash_core_execution::TriggerDeliveryRetentionCandidate {
                                occurrence_id: row.get(0)?,
                                subscription_id: row.get(1)?,
                                process_id: crate::row_process_id(row, 2)?,
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

    async fn list_session_owner_ids_for_retention(
        &self,
    ) -> Result<Vec<SessionId>, lash_core_execution::PluginError> {
        self.conn
            .call(|conn| {
                Ok((|| {
                    let mut stmt = conn
                        .prepare(
                            trigger_sql()
                                .retention_sqlite
                                .select_session_owner_ids
                                .sql(),
                        )
                        .map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map([], |row| row.get::<_, String>(0))
                        .map_err(process_sqlite_error)?;
                    let mut session_ids = std::collections::BTreeSet::new();
                    for row in rows {
                        let owner_scope = row.map_err(process_sqlite_error)?;
                        if let Some(session_id) = owner_scope.strip_prefix("session:") {
                            session_ids.insert(
                                SessionId::parse(session_id)
                                    .map_err(lash_core_execution::PluginError::from)?,
                            );
                        }
                    }
                    Ok(session_ids.into_iter().collect())
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn reconcile_trigger_retention(
        &self,
        candidates: &[lash_core_execution::TriggerDeliveryRetentionCandidate],
        deleted_session_ids: &[SessionId],
    ) -> Result<
        lash_core_execution::TriggerRetentionReconciliationReport,
        lash_core_execution::PluginError,
    > {
        let candidates_json = serde_json::to_string(candidates).map_err(process_decode_error)?;
        let deleted_owner_scopes = deleted_session_ids
            .iter()
            .map(|session_id| {
                lash_core_execution::TriggerOwnerScope::session(session_id).namespace()
            })
            .collect::<Vec<_>>();
        let deleted_owner_scopes_json =
            serde_json::to_string(&deleted_owner_scopes).map_err(process_decode_error)?;
        let reclaimed_at_ms = i64::try_from(self.clock.timestamp_ms()).unwrap_or(i64::MAX);
        self.conn
            .write_flow(move |tx| {
                Ok(trigger_tx_outcome((|| {
                    let sql = trigger_sql();
                    let reclaimed_delivery_count = crate::conn::cached_execute(
                        tx,
                        sql.delivery_sqlite.delete_retention_candidates.sql(),
                        params![&candidates_json],
                    )
                    .map_err(process_sqlite_error)?;
                    crate::conn::cached_execute(
                        tx,
                        sql.occurrence_sqlite.tombstone_orphan_fired.sql(),
                        params![reclaimed_at_ms],
                    )
                    .map_err(process_sqlite_error)?;
                    let reclaimed_occurrence_count = crate::conn::cached_execute(
                        tx,
                        sql.occurrence_sqlite.delete_orphan_fired.sql(),
                        [],
                    )
                    .map_err(process_sqlite_error)?;

                    let records = {
                        let mut stmt = tx
                            .prepare(sql.subscription_sqlite.select_unreferenced_for_owners.sql())
                            .map_err(process_sqlite_error)?;
                        let rows = stmt
                            .query_map(params![&deleted_owner_scopes_json], |row| {
                                row.get::<_, String>(0)
                            })
                            .map_err(process_sqlite_error)?;
                        rows.collect::<Result<Vec<_>, _>>()
                            .map_err(process_sqlite_error)?
                    };
                    for json in records {
                        let mut record =
                            lash_core_execution::facade_support::decode_trigger_subscription_json(
                                &json,
                            )?;
                        if !record.is_tombstoned() {
                            record.revision =
                                lash_core_execution::facade_support::next_trigger_store_revision(
                                    &record,
                                )?;
                            record.tombstone(reclaimed_at_ms as u64);
                        }
                        record_subscription_change(tx, &record)?;
                    }

                    let reclaimed_subscription_count = crate::conn::cached_execute(
                        tx,
                        sql.subscription_sqlite.delete_unreferenced_for_owners.sql(),
                        params![&deleted_owner_scopes_json],
                    )
                    .map_err(process_sqlite_error)?;

                    Ok(lash_core_execution::TriggerRetentionReconciliationReport {
                        reclaimed_delivery_count,
                        reclaimed_occurrence_count,
                        reclaimed_subscription_count,
                    })
                })()))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn delete_delivery_retention_candidates(
        &self,
        candidates: &[lash_core_execution::TriggerDeliveryRetentionCandidate],
    ) -> Result<usize, lash_core_execution::PluginError> {
        if candidates.is_empty() {
            return Ok(0);
        }
        let candidates_json = serde_json::to_string(candidates).map_err(process_decode_error)?;
        let armed_at_ms = i64::try_from(self.clock.timestamp_ms()).unwrap_or(i64::MAX);
        self.conn
            .write(move |tx| {
                let sql = trigger_sql();
                let deleted = crate::conn::cached_execute(
                    tx,
                    sql.delivery_sqlite.delete_retention_candidates.sql(),
                    params![&candidates_json],
                )?;
                crate::conn::cached_execute(
                    tx,
                    sql.occurrence_sqlite.arm_reclaimable_for_candidates.sql(),
                    params![&candidates_json, armed_at_ms],
                )?;
                Ok(deleted)
            })
            .await
            .map_err(process_sqlite_error)
    }

    async fn reclaim_trigger_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> lash_core_execution::TriggerOccurrenceReclamationResult {
        let cutoff_epoch_ms = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        // The scope read stays a single autocommit statement; each candidate's
        // delete is its own gated write transaction, so a mid-loop failure
        // still commits every row already reclaimed (FIG-3975).
        let scoped = self
            .conn
            .call(move |conn| {
                let sql = trigger_sql();
                Ok((|| {
                    let mut stmt = conn
                        .prepare(sql.occurrence_sqlite.select_reclamation_scope.sql())
                        .map_err(|error| {
                            lash_core_execution::MaintenanceFailure::failed_before_any_work(
                                Box::new(process_sqlite_error(error)),
                            )
                        })?;
                    let rows = stmt
                        .query_map(params![cutoff_epoch_ms], |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, i64>(3)?,
                                row.get::<_, Option<String>>(4)?,
                            ))
                        })
                        .map_err(|error| {
                            lash_core_execution::MaintenanceFailure::failed_before_any_work(
                                Box::new(process_sqlite_error(error)),
                            )
                        })?;
                    let rows = rows.collect::<Result<Vec<_>, _>>().map_err(|error| {
                        lash_core_execution::MaintenanceFailure::failed_before_any_work(Box::new(
                            process_sqlite_error(error),
                        ))
                    })?;

                    let first = &rows[0];
                    let report = lash_core_execution::TriggerOccurrenceReclamationReport {
                        inspected_occurrence_count: first.0 as usize,
                        live_fan_out_count: first.1 as usize,
                        grace_deferred_count: first.2 as usize,
                        audit_retained_count: first.3 as usize,
                        ..lash_core_execution::TriggerOccurrenceReclamationReport::default()
                    };
                    let candidates = rows
                        .into_iter()
                        .filter_map(|(_, _, _, _, occurrence_id)| occurrence_id)
                        .collect::<Vec<_>>();
                    Ok((report, candidates))
                })())
            })
            .await
            .map_err(|error| {
                lash_core_execution::MaintenanceFailure::failed_before_any_work(Box::new(
                    process_sqlite_error(error),
                ))
            })?;
        let (mut report, candidates) = scoped?;
        let reclaimed_at_ms = i64::try_from(self.clock.timestamp_ms()).unwrap_or(i64::MAX);
        for occurrence_id in candidates {
            let deleted = self
                .conn
                .write(move |tx| {
                    let sql = trigger_sql();
                    crate::conn::cached_execute(
                        tx,
                        sql.occurrence_sqlite.tombstone_reclaimable_by_id.sql(),
                        params![occurrence_id, cutoff_epoch_ms, reclaimed_at_ms],
                    )?;
                    crate::conn::cached_execute(
                        tx,
                        sql.occurrence_sqlite.delete_reclaimable_by_id.sql(),
                        params![occurrence_id, cutoff_epoch_ms],
                    )
                })
                .await
                .map_err(|error| {
                    lash_core_execution::MaintenanceFailure::failed(
                        Box::new(process_sqlite_error(error)),
                        report.clone(),
                    )
                })?;
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
    ) -> Result<usize, lash_core_execution::StoreError> {
        let signed_cutoff = i64::try_from(written_before_epoch_ms);
        let beyond_sql_range = signed_cutoff.is_err();
        let written_before_ms = signed_cutoff.unwrap_or(i64::MAX);
        self.conn
            .write(move |tx| {
                crate::conn::cached_execute(
                    tx,
                    trigger_sql().tombstone.forget_written_before.sql(),
                    params![written_before_ms, beyond_sql_range],
                )
            })
            .await
            .map_err(crate::sqlite_error)
    }

    async fn prune_non_fired_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> Result<usize, lash_core_execution::PluginError> {
        let cutoff_epoch_ms = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        let reclaimed_at_ms = i64::try_from(self.clock.timestamp_ms()).unwrap_or(i64::MAX);
        self.conn
            .write(move |tx| {
                let sql = trigger_sql();
                crate::conn::cached_execute(
                    tx,
                    sql.occurrence_sqlite.tombstone_non_fired.sql(),
                    params![cutoff_epoch_ms, reclaimed_at_ms],
                )?;
                crate::conn::cached_execute(
                    tx,
                    sql.occurrence_sqlite.prune_non_fired.sql(),
                    params![cutoff_epoch_ms],
                )
            })
            .await
            .map_err(process_sqlite_error)
    }
}

fn reserve_sqlite_deliveries(
    tx: &rusqlite::Transaction<'_>,
    occurrence: &lash_core_execution::TriggerOccurrenceRecord,
    created_at_ms: u64,
) -> Result<Vec<lash_core_execution::TriggerDeliveryReservation>, lash_core_execution::PluginError>
{
    let sql = trigger_sql();
    let mut values: Vec<rusqlite::types::Value> = vec![
        occurrence.source_type.clone().into(),
        occurrence.source_key.clone().into(),
    ];
    let statement = match occurrence.session_id.as_ref() {
        Some(session_id) => {
            values.push(
                lash_core_execution::TriggerOwnerScope::session(session_id.clone())
                    .namespace()
                    .into(),
            );
            &sql.subscription_sqlite.select_enabled_for_source_and_owner
        }
        None => &sql.subscription_sqlite.select_enabled_for_source,
    };
    let mut stmt = tx
        .prepare_cached(statement.sql())
        .map_err(process_sqlite_error)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(process_sqlite_error)?;
    let mut subscriptions = Vec::new();
    for row in rows {
        let (subscription_id, json) = row.map_err(process_sqlite_error)?;
        match lash_core_execution::facade_support::decode_trigger_subscription_json(&json) {
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
        crate::conn::cached_execute(
            tx,
            sql.delivery.insert.sql(),
            params![
                occurrence.occurrence_id.as_str(),
                subscription.subscription_id.as_str(),
                subscription.incarnation.as_str(),
                sql_revision,
                lash_core_execution::facade_support::encode_trigger_row(&subscription)?,
                created_at_ms as i64,
                lash_core_execution::store::ObligationKey::TriggerDelivery {
                    occurrence_id: occurrence.occurrence_id.clone(),
                    subscription_id: subscription.subscription_id.clone(),
                }
                .id()
                .as_str(),
            ],
        )
        .map_err(process_sqlite_error)?;
        reservations.push(lash_core_execution::TriggerDeliveryReservation {
            occurrence: occurrence.clone(),
            subscription,
            process_id: None,
            created_at_ms,
        });
    }
    lash_core_execution::facade_support::sort_trigger_delivery_reservations(&mut reservations);
    Ok(reservations)
}

fn sqlite_delivery_snapshots(
    tx: &rusqlite::Transaction<'_>,
    occurrence: &lash_core_execution::TriggerOccurrenceRecord,
) -> Result<Vec<lash_core_execution::TriggerDeliveryReservation>, lash_core_execution::PluginError>
{
    let mut stmt = tx
        .prepare(trigger_sql().delivery.select_snapshots_by_occurrence.sql())
        .map_err(process_sqlite_error)?;
    let rows = stmt
        .query_map(params![occurrence.occurrence_id.as_str()], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?
                    .map(|value| crate::sql_process_id(0, value))
                    .transpose()?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(process_sqlite_error)?;
    let mut reservations = Vec::new();
    for row in rows {
        let (process_id, created_at_ms, snapshot_json) = row.map_err(process_sqlite_error)?;
        reservations.push(lash_core_execution::TriggerDeliveryReservation {
            occurrence: occurrence.clone(),
            subscription: lash_core_execution::facade_support::decode_trigger_subscription_json(
                &snapshot_json,
            )?,
            process_id,
            created_at_ms: plugin_u64_from_sql("TriggerDelivery", "created_at_ms", created_at_ms)?,
        });
    }
    lash_core_execution::facade_support::sort_trigger_delivery_reservations(&mut reservations);
    Ok(reservations)
}

/// The `revision` columns of `trigger_subscriptions` and `trigger_deliveries`
/// are written through [`plugin_sql_counter_value`] and read back by nothing in
/// the store — every read path decodes `record_json` instead. These tests are
/// the only observation of the value that actually lands in SQL.
#[cfg(test)]
#[path = "triggers/revision_column_tests.rs"]
mod revision_column_tests;

#[cfg(test)]
#[path = "triggers/outcome_laws.rs"]
mod outcome_laws;
