//! `trigger_subscriptions`: one row per durable trigger subscription.
//!
//! `record_json` is the subscription; the typed columns beside it exist so a
//! predicate never has to open the JSON. That is why every projection here is
//! either `record_json` alone or [`RECORD_COLUMNS`].

/// The table's unprefixed name.
pub const TABLE: &str = "trigger_subscriptions";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str =
    "subscription_id, owner_scope, subscription_key, incarnation, revision,
                definition_fingerprint, source_type, source_key, lifecycle, deleted_at_ms,
                created_at_ms, updated_at_ms, record_json";

/// The durable record, with the id a warning names when it does not decode.
///
/// Narrow deliberately, and the only multi-column read of this table: a
/// listing decodes `record_json` and carries `subscription_id` for one reason
/// only — a row whose JSON is malformed is skipped with a `tracing::warn!` that
/// has to name which row it skipped. Every typed column beside them is a
/// predicate input that the record already carries, so reading them back would
/// be reading the same fact twice.
pub const RECORD_COLUMNS: &str = "subscription_id, record_json";

crate::statements! {
    /// `trigger_subscriptions` statements both backends issue verbatim.
    pub struct SubscriptionStatements @ "trigger_subscription" {
        /// Record the subscription `?1`, replacing the row already under its
        /// id.
        ///
        /// `subscription_id` is deterministic in the owner scope and the
        /// subscription key, so this is the same subscription in every case
        /// the conflict fires — a revision, a lifecycle move or a revival,
        /// each of which the caller has already evaluated against the row it
        /// read in this transaction. `created_at_ms` is the one column the
        /// conflict path leaves alone.
        upsert = "INSERT INTO trigger_subscriptions (
                subscription_id, owner_scope, subscription_key, incarnation, revision,
                definition_fingerprint, source_type, source_key, lifecycle, deleted_at_ms,
                created_at_ms, updated_at_ms, record_json
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT (subscription_id) DO UPDATE SET
                owner_scope = EXCLUDED.owner_scope,
                subscription_key = EXCLUDED.subscription_key,
                incarnation = EXCLUDED.incarnation,
                revision = EXCLUDED.revision,
                definition_fingerprint = EXCLUDED.definition_fingerprint,
                source_type = EXCLUDED.source_type,
                source_key = EXCLUDED.source_key,
                lifecycle = EXCLUDED.lifecycle,
                deleted_at_ms = EXCLUDED.deleted_at_ms,
                updated_at_ms = EXCLUDED.updated_at_ms,
                record_json = EXCLUDED.record_json";

        /// Every live subscription a filter admits: `?1` owner scope, `?2`
        /// subscription key, `?3` source type, `?4` source key, `?5`
        /// lifecycle, each skipped when bound NULL.
        ///
        /// The five optional predicates were a `format!`ed `WHERE` clause on
        /// SQLite and a `QueryBuilder` on PostgreSQL, which is how one filter
        /// became thirty-two statements nobody could see. They are one
        /// statement here, with the same predicate set: an unbound predicate
        /// is bound NULL and `COALESCE` compares the column with itself, which
        /// is exactly "no predicate" because every one of these columns is
        /// `NOT NULL`. `TriggerSubscriptionFilter::matches` still decides each
        /// record afterwards, as it did before — it covers `name` and
        /// `target`, which have never been SQL predicates.
        list_filtered = "SELECT subscription_id, record_json
             FROM trigger_subscriptions
             WHERE lifecycle <> 'tombstoned'
               AND owner_scope = COALESCE(?1, owner_scope)
               AND subscription_key = COALESCE(?2, subscription_key)
               AND source_type = COALESCE(?3, source_type)
               AND source_key = COALESCE(?4, source_key)
               AND lifecycle = COALESCE(?5, lifecycle)
             ORDER BY owner_scope ASC, subscription_key ASC";

        /// Tombstone subscription `?1` at `?3`, with revision `?2` and the
        /// record `?4` its caller advanced. The deletion time lands in the
        /// column and in the record together; the table's CHECK refuses the
        /// pair coming apart.
        tombstone = "UPDATE trigger_subscriptions
             SET lifecycle = 'tombstoned', deleted_at_ms = ?3, revision = ?2,
                 updated_at_ms = ?3, record_json = ?4
             WHERE subscription_id = ?1";
    }
}
