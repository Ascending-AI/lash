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

        /// Every live subscription, in listing order. The general listing:
        /// it has no predicate to seek on, so it scans by design, and it is
        /// what an arbitrary admin filter falls to.
        list_all = "SELECT subscription_id, record_json
             FROM trigger_subscriptions
             WHERE lifecycle <> 'tombstoned'
             ORDER BY owner_scope ASC, subscription_key ASC";

        /// Every live subscription of owner scope `?1`: a session listing its
        /// own registrations, and the `List` command's whole answer.
        ///
        /// Seeks `(owner_scope, subscription_key)` on its leading column,
        /// which also serves the `ORDER BY`.
        list_by_owner = "SELECT subscription_id, record_json
             FROM trigger_subscriptions
             WHERE lifecycle <> 'tombstoned'
               AND owner_scope = ?1
             ORDER BY owner_scope ASC, subscription_key ASC";

        /// The live subscription of owner scope `?1` under key `?2`. Seeks
        /// `(owner_scope, subscription_key)` on both columns, which is that
        /// pair's uniqueness constraint.
        list_by_owner_and_key = "SELECT subscription_id, record_json
             FROM trigger_subscriptions
             WHERE lifecycle <> 'tombstoned'
               AND owner_scope = ?1
               AND subscription_key = ?2
             ORDER BY owner_scope ASC, subscription_key ASC";

        /// Every live subscription of source type `?1`, across owners. Seeks
        /// `(source_type, source_key, lifecycle)` on its leading column.
        list_by_source_type = "SELECT subscription_id, record_json
             FROM trigger_subscriptions
             WHERE lifecycle <> 'tombstoned'
               AND source_type = ?1
             ORDER BY owner_scope ASC, subscription_key ASC";

        /// Every live subscription of source `?1`/`?2`, across owners. Seeks
        /// `(source_type, source_key, lifecycle)` on both key columns.
        list_by_source = "SELECT subscription_id, record_json
             FROM trigger_subscriptions
             WHERE lifecycle <> 'tombstoned'
               AND source_type = ?1
               AND source_key = ?2
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

/// Which listing statement a subscription filter is served by.
///
/// The filter has five SQL-valued fields and the listing used to build a
/// `WHERE` clause out of whichever were set — a `format!` on SQLite, a
/// `QueryBuilder` on PostgreSQL. Writing that as one statement with optional
/// predicates costs the index: a comparison whose right-hand side mentions the
/// column is not sargable on either backend, so every listing would scan
/// whatever was bound. Instead each shape that an index can seek has its own
/// statement of plain equalities, and everything else falls to
/// [`SubscriptionStatements::list_all`].
///
/// A named statement pushes down only the part its index seeks on; the
/// remaining fields — and `name` and `target`, which have never been SQL
/// predicates — are decided by `TriggerSubscriptionFilter::matches` in the
/// caller, exactly as they were before. Every statement therefore returns a
/// superset of the answer and the answer itself is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListShape {
    /// No indexable field is set.
    All,
    /// Owner scope alone.
    ByOwner,
    /// Owner scope and subscription key.
    ByOwnerAndKey,
    /// Source type alone, with no owner scope.
    BySourceType,
    /// Source type and source key, with no owner scope.
    BySource,
}

impl ListShape {
    /// The shape of a filter that sets the fields these flags describe.
    ///
    /// Owner scope wins over source: it is the more selective of the two on
    /// every path that sets both, because a source type is shared across
    /// owners by construction. A source key with no source type is not a seek
    /// on `(source_type, source_key, lifecycle)`, so it is not a shape.
    #[must_use]
    pub const fn of(
        owner_scope: bool,
        subscription_key: bool,
        source_type: bool,
        source_key: bool,
    ) -> Self {
        match (owner_scope, subscription_key, source_type, source_key) {
            (true, true, _, _) => Self::ByOwnerAndKey,
            (true, false, _, _) => Self::ByOwner,
            (false, _, true, true) => Self::BySource,
            (false, _, true, false) => Self::BySourceType,
            (false, _, false, _) => Self::All,
        }
    }
}

impl SubscriptionStatements {
    /// The listing statement for `shape`.
    #[must_use]
    pub fn list_for(&self, shape: ListShape) -> &crate::Rendered {
        match shape {
            ListShape::All => &self.list_all,
            ListShape::ByOwner => &self.list_by_owner,
            ListShape::ByOwnerAndKey => &self.list_by_owner_and_key,
            ListShape::BySourceType => &self.list_by_source_type,
            ListShape::BySource => &self.list_by_source,
        }
    }
}
