//! PostgreSQL-backed runtime trigger store, and the PostgreSQL owner of the
//! trigger family's four tables.
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
use lash_store_sql::trigger::occurrences::{
    ListShape as OccurrenceListShape, OccurrenceStatements,
};
use lash_store_sql::trigger::subscriptions::{
    ListShape as SubscriptionListShape, SubscriptionStatements,
};
use std::sync::LazyLock;

lash_store_sql::statements! {
    /// `trigger_subscriptions` statements only PostgreSQL issues.
    pub(crate) struct SubscriptionPostgresStatements @ "trigger_subscription" {
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

        /// Delete every subscription of the owner scopes in `?1` that no
        /// delivery still references.
        ///
        /// PostgreSQL binds a real `TEXT[]`; SQLite unnests a JSON array with
        /// `json_each`.
        delete_unreferenced_for_owners = "DELETE FROM trigger_subscriptions AS subscription
             WHERE subscription.owner_scope = ANY(?1::TEXT[])
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

        /// Delete every fired occurrence no delivery references.
        ///
        /// PostgreSQL reads the outcome with `jsonb #>>`, SQLite with
        /// `json_extract`.
        delete_orphan_fired = "DELETE FROM trigger_occurrences AS occurrence
             WHERE COALESCE(occurrence.record_json::jsonb #>> '{outcome,kind}', 'fired') = 'fired'
               AND NOT EXISTS (
                   SELECT 1 FROM trigger_deliveries AS delivery
                   WHERE delivery.occurrence_id = occurrence.occurrence_id
               )";

        /// Arm at `?2` every occurrence named in `?1` whose last delivery this
        /// pass removed. Forks on the bound array and on the outcome read.
        arm_reclaimable_for_candidates = "UPDATE trigger_occurrences AS occurrence
             SET reclaimable_at_ms = ?2
             WHERE occurrence.reclaimable_at_ms IS NULL
               AND COALESCE(occurrence.record_json::jsonb #>> '{outcome,kind}', 'fired') = 'fired'
               AND occurrence.occurrence_id = ANY(?1::TEXT[])
               AND NOT EXISTS (
                   SELECT 1 FROM trigger_deliveries AS delivery
                   WHERE delivery.occurrence_id = occurrence.occurrence_id
               )";

        /// The reclamation sweep's scope proof and its worklist, from one
        /// snapshot at cutoff `?1`.
        ///
        /// The aggregate visits the whole table so `NothingToDo` stays
        /// witnessed emptiness; only eligible ids are materialized, through
        /// the partial reclaimability index. Forks on the outcome read alone —
        /// see
        /// [`lash_store_sql::trigger::occurrences::RECLAMATION_SCOPE_COUNTS_POSTGRES`].
        select_reclamation_scope = "WITH scope AS (
                 SELECT COUNT(*) AS inspected_count,
                        COUNT(*) FILTER (
                            WHERE reclaimable_at_ms IS NULL
                              AND COALESCE(record_json::jsonb #>> '{outcome,kind}', 'fired') = 'fired'
                        ) AS live_fan_out_count,
                        COUNT(*) FILTER (
                            WHERE COALESCE(record_json::jsonb #>> '{outcome,kind}', 'fired') != 'fired'
                        ) AS audit_retained_count,
                        COUNT(*) FILTER (
                            WHERE reclaimable_at_ms > ?1
                              AND COALESCE(record_json::jsonb #>> '{outcome,kind}', 'fired') = 'fired'
                        ) AS grace_deferred_count
                 FROM trigger_occurrences
             ), candidates AS (
                 SELECT occurrence_id
                 FROM trigger_occurrences
                 WHERE reclaimable_at_ms IS NOT NULL
                   AND reclaimable_at_ms <= ?1
                   AND COALESCE(record_json::jsonb #>> '{outcome,kind}', 'fired') = 'fired'
             )
             SELECT scope.inspected_count,
                    scope.live_fan_out_count,
                    scope.grace_deferred_count,
                    scope.audit_retained_count,
                    candidates.occurrence_id
             FROM scope
             LEFT JOIN candidates ON TRUE
             ORDER BY candidates.occurrence_id ASC";

        /// Reclaim occurrence `?1` if it is still eligible at cutoff `?2`. The
        /// whole eligibility test is re-proved here, because the worklist was
        /// read from an earlier snapshot. Forks on the outcome read.
        delete_reclaimable_by_id = "DELETE FROM trigger_occurrences AS occurrence
             WHERE occurrence.occurrence_id = ?1
               AND occurrence.reclaimable_at_ms IS NOT NULL
               AND occurrence.reclaimable_at_ms <= ?2
               AND COALESCE(occurrence.record_json::jsonb #>> '{outcome,kind}', 'fired') = 'fired'
               AND NOT EXISTS (
                   SELECT 1 FROM trigger_deliveries AS delivery
                   WHERE delivery.occurrence_id = occurrence.occurrence_id
               )";

        /// Drop non-fired (audit) occurrences older than `?1`. Forks on the
        /// outcome read.
        prune_non_fired = "DELETE FROM trigger_occurrences
             WHERE occurred_at_ms < ?1
               AND COALESCE(record_json::jsonb #>> '{outcome,kind}', 'fired') <> 'fired'";
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

        /// Every session whose deliveries are still outstanding: the scopes a
        /// retention pass must not reclaim receipts for. Forks on the JSON
        /// read.
        select_session_owner_scopes = "SELECT DISTINCT
                    'session:' ||
                    (subscription_snapshot_json::jsonb #>> '{owner_scope,session_id}')
             FROM trigger_deliveries
             WHERE subscription_snapshot_json::jsonb #>> '{owner_scope,type}' = 'session'";
    }
}

lash_store_sql::statements! {
    /// `trigger_mutation_receipts` statements only PostgreSQL issues.
    pub(crate) struct MutationReceiptPostgresStatements @ "trigger_mutation_receipt" {
        /// Drop the session-owned receipts of the owner ids in `?1`. Forks on
        /// the bound `TEXT[]` against SQLite's `json_each`.
        delete_for_session_owners = "DELETE FROM trigger_mutation_receipts
             WHERE owner_kind = 'session'
               AND owner_id = ANY(?1::TEXT[])";
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
    }
}

/// Every trigger-family statement, rendered once.
pub(crate) struct TriggerSql {
    /// `trigger_subscriptions` statements both backends issue verbatim.
    pub(crate) subscription: SubscriptionStatements,
    /// `trigger_subscriptions` statements only PostgreSQL issues.
    pub(crate) subscription_postgres: SubscriptionPostgresStatements,
    /// `trigger_occurrences` statements both backends issue verbatim.
    pub(crate) occurrence: OccurrenceStatements,
    /// `trigger_occurrences` statements only PostgreSQL issues.
    occurrence_postgres: OccurrencePostgresStatements,
    /// `trigger_deliveries` statements both backends issue verbatim.
    delivery: DeliveryStatements,
    /// `trigger_deliveries` statements only PostgreSQL issues.
    delivery_postgres: DeliveryPostgresStatements,
    /// `trigger_mutation_receipts` statements both backends issue verbatim.
    receipt: MutationReceiptStatements,
    /// `trigger_mutation_receipts` statements only PostgreSQL issues.
    receipt_postgres: MutationReceiptPostgresStatements,
    /// The family's cross-table retention read.
    retention_postgres: RetentionPostgresStatements,
}

static TRIGGER_SQL: LazyLock<TriggerSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    TriggerSql {
        subscription: SubscriptionStatements::render(dialect),
        subscription_postgres: SubscriptionPostgresStatements::render(dialect),
        occurrence: OccurrenceStatements::render(dialect),
        occurrence_postgres: OccurrencePostgresStatements::render(dialect),
        delivery: DeliveryStatements::render(dialect),
        delivery_postgres: DeliveryPostgresStatements::render(dialect),
        receipt: MutationReceiptStatements::render(dialect),
        receipt_postgres: MutationReceiptPostgresStatements::render(dialect),
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
    filter: &lash_core::TriggerOccurrenceFilter,
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
        command: lash_core::TriggerCommand,
    ) -> Result<lash_core::TriggerEffectResult, PluginError> {
        let owner_valid = match command.owner_scope() {
            lash_core::TriggerOwnerScope::Session { session_id } => {
                crate::namespace::is_valid_opaque_key(session_id)
            }
            lash_core::TriggerOwnerScope::Host { binding_id } => {
                crate::namespace::is_valid_opaque_key(binding_id.trim())
            }
            lash_core::TriggerOwnerScope::Platform => true,
        };
        if !crate::namespace::is_valid_opaque_key(operation_id.trim()) || !owner_valid {
            return Ok(Err(lash_core::TriggerOperationError::Invalid {
                message: "invalid trigger operation or owner identifier".into(),
            }));
        }
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

        let sql = trigger_sql();
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
        sqlx::query(
            crate::connection_sql::connection_sql()
                .lock_xact_by_text
                .sql(),
        )
        .bind(&subscription_id)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;

        let stored = sqlx::query(sql.receipt.select_by_operation_id.sql())
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
            let rows = sqlx::query(sql.subscription_postgres.select_records_for_prune.sql())
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
            let current_json: Option<String> =
                sqlx::query_scalar(sql.subscription_postgres.select_record_by_id.sql())
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
                .bind(serde_json::to_string(record).map_err(process_decode_error)?)
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?;
        }
        sqlx::query(sql.receipt.insert.sql())
            .bind(&receipt_id)
            .bind(receipt_owner_scope.owner_kind_column())
            .bind(receipt_owner_scope.owner_id_column())
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

    async fn delete_session_subscriptions(
        &self,
        session_id: &SessionId,
    ) -> Result<usize, PluginError> {
        let sql = trigger_sql();
        let owner_scope = lash_core::TriggerOwnerScope::session(session_id).namespace();
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let rows = sqlx::query(
            sql.subscription_postgres
                .select_session_owned_for_update
                .sql(),
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
            record.tombstone(now);
            record.revision = next_revision;
            record.updated_at_ms = now;
            let sql_revision =
                plugin_sql_counter_value("trigger_subscription_revision", record.revision)?;
            sqlx::query(sql.subscription.tombstone.sql())
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
        let sql = trigger_sql();
        let occurrence_id = lash_core::facade_support::deterministic_occurrence_id(&request);
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        sqlx::query(
            crate::connection_sql::connection_sql()
                .lock_xact_by_text
                .sql(),
        )
        .bind(&request.idempotency_key)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        let existing = sqlx::query(
            sql.occurrence_postgres
                .select_record_by_idempotency_key
                .sql(),
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
                return Err(lash_core::durable_identity_conflict(format!(
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
            sqlx::query(sql.occurrence.insert.sql())
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
            sqlx::query(sql.occurrence.arm_reclaimable.sql())
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
            realization: lash_core::StoreRealization::from_wrote(is_new),
        })
    }

    async fn list_occurrences(
        &self,
        filter: lash_core::TriggerOccurrenceFilter,
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
                serde_json::from_str(&json).map_err(process_decode_error)?;
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
            .map(|ids: Vec<String>| ids.into_iter().map(ProcessId::from).collect())
            .map_err(plugin_sqlx_error)
    }

    async fn list_delivery_retention_candidates(
        &self,
    ) -> Result<Vec<lash_core::TriggerDeliveryRetentionCandidate>, PluginError> {
        let rows = sqlx::query(trigger_sql().delivery.select_retention_candidates.sql())
            .fetch_all(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        Ok(rows
            .into_iter()
            .map(|row| lash_core::TriggerDeliveryRetentionCandidate {
                occurrence_id: row.get(0),
                subscription_id: row.get(1),
                process_id: ProcessId::from(row.get::<String, _>(2)),
            })
            .collect())
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
                session_ids.insert(SessionId::from(session_id));
            }
        }
        Ok(session_ids.into_iter().collect())
    }

    async fn reconcile_trigger_retention(
        &self,
        candidates: &[lash_core::TriggerDeliveryRetentionCandidate],
        deleted_session_ids: &[SessionId],
    ) -> Result<lash_core::TriggerRetentionReconciliationReport, PluginError> {
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
            .map(|session_id| lash_core::TriggerOwnerScope::session(session_id).namespace())
            .collect::<Vec<_>>();
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;

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
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?
                .rows_affected() as usize
        };
        let reclaimed_occurrence_count =
            sqlx::query(sql.occurrence_postgres.delete_orphan_fired.sql())
                .execute(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?
                .rows_affected() as usize;

        let blocked_owner_scopes: Vec<String> =
            sqlx::query_scalar(sql.delivery_postgres.select_session_owner_scopes.sql())
                .fetch_all(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?;
        let blocked_owner_scopes = blocked_owner_scopes
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        let receipt_owner_ids = deleted_owner_scopes
            .iter()
            .filter(|owner_scope| !blocked_owner_scopes.contains(*owner_scope))
            .map(|owner_scope| owner_scope["session:".len()..].to_string())
            .collect::<Vec<_>>();

        let reclaimed_subscription_count = if deleted_owner_scopes.is_empty() {
            0
        } else {
            sqlx::query(
                sql.subscription_postgres
                    .delete_unreferenced_for_owners
                    .sql(),
            )
            .bind(&deleted_owner_scopes)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?
            .rows_affected() as usize
        };
        let reclaimed_mutation_receipt_count = if receipt_owner_ids.is_empty() {
            0
        } else {
            sqlx::query(sql.receipt_postgres.delete_for_session_owners.sql())
                .bind(&receipt_owner_ids)
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
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let deleted = sqlx::query(sql.delivery_postgres.delete_retention_candidates.sql())
            .bind(occurrence_ids)
            .bind(subscription_ids)
            .bind(
                process_ids
                    .iter()
                    .map(ProcessId::as_str)
                    .collect::<Vec<_>>(),
            )
            .execute(&mut *tx)
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
        let sql = trigger_sql();
        let cutoff_epoch_ms = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        let rows = sqlx::query(sql.occurrence_postgres.select_reclamation_scope.sql())
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
            audit_retained_count: first.get::<i64, _>(3) as usize,
            ..lash_core::TriggerOccurrenceReclamationReport::default()
        };
        let candidates = rows
            .into_iter()
            .filter_map(|row| row.get::<Option<String>, _>(4))
            .collect::<Vec<_>>();

        for occurrence_id in candidates {
            let deleted = sqlx::query(sql.occurrence_postgres.delete_reclaimable_by_id.sql())
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
        Ok(
            sqlx::query(trigger_sql().receipt.prune_host_and_platform.sql())
                .bind(cutoff_epoch_ms)
                .execute(&self.pool)
                .await
                .map_err(plugin_sqlx_error)?
                .rows_affected() as usize,
        )
    }

    async fn prune_non_fired_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> Result<usize, PluginError> {
        let cutoff_epoch_ms = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        Ok(
            sqlx::query(trigger_sql().occurrence_postgres.prune_non_fired.sql())
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
    created_at_ms: u64,
) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
    let sql = trigger_sql();
    let owner_scope = occurrence
        .session_id
        .as_deref()
        .map(|session_id| lash_core::TriggerOwnerScope::session(session_id).namespace());
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
        sqlx::query(sql.delivery.insert.sql())
            .bind(&occurrence.occurrence_id)
            .bind(&subscription.subscription_id)
            .bind(process_id.as_str())
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
    let rows = sqlx::query(trigger_sql().delivery.select_snapshots_by_occurrence.sql())
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
                process_id: ProcessId::from(row.get::<String, _>(0)),
                created_at_ms: plugin_u64_from_sql("TriggerDelivery", "created_at_ms", row.get(1))?,
                reservation_status: lash_core::TriggerDeliveryReservationOutcome::AlreadyReserved,
            })
        })
        .collect::<Result<Vec<_>, PluginError>>()?;
    lash_core::facade_support::sort_trigger_delivery_reservations(&mut reservations);
    Ok(reservations)
}

/// Run one of the four named delivery listings.
///
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
            Ok(TriggerDeliveryReservation {
                occurrence: serde_json::from_str(&occurrence_json).map_err(process_decode_error)?,
                subscription: serde_json::from_str(&subscription_json)
                    .map_err(process_decode_error)?,
                process_id: ProcessId::from(row.get::<String, _>(0)),
                created_at_ms: plugin_u64_from_sql("TriggerDelivery", "created_at_ms", row.get(1))?,
                reservation_status: lash_core::TriggerDeliveryReservationOutcome::AlreadyReserved,
            })
        })
        .collect()
}
