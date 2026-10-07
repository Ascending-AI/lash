//! `trigger_deliveries`: one row per subscription an occurrence fired at.
//!
//! The row freezes the subscription as it stood when the firing reserved the
//! delivery (`subscription_snapshot_json`), so a later edit to the
//! subscription cannot retroactively change what was delivered.
//!
//! `status` is `started` with its minted `process_id`, or `refused` with
//! `refusal_json` retaining the typed code, diagnostic and value mismatch.
//! Both commit with the occurrence (ADR 0132 §12), so a held emission answers
//! the first disposition and never starts a refused delivery.

/// The table's unprefixed name.
pub const TABLE: &str = "trigger_deliveries";

/// Every column the reserving insert writes, in insert order.
pub const INSERT_COLUMNS: &str =
    "occurrence_id, subscription_id, process_id, subscription_incarnation,
                subscription_revision, subscription_snapshot_json, created_at_ms, status, refusal_json";

/// SQLite's spelling of the owning session of a frozen subscription snapshot.
///
/// Not a column list: an expression the retention sweep projects to enumerate
/// the sessions whose deliveries are still outstanding. It is named here for
/// the same reason
/// [`super::occurrences::RECLAMATION_SCOPE_COUNTS_SQLITE`] is — the comma
/// inside `json_extract`'s argument list makes it a multi-column projection as
/// far as the gate can tell, and naming it puts the two backends' spellings
/// where they can be compared.
pub const SESSION_OWNER_SCOPE_SQLITE: &str = "'session:' || json_extract(
                                    subscription_snapshot_json,
                                    '$.owner_scope.session_id'
                                )";

crate::statements! {
    /// `trigger_deliveries` statements both backends issue verbatim.
    pub struct DeliveryStatements @ "trigger_delivery" {
        /// Record the delivery of occurrence `?1` to subscription `?2`,
        /// freezing the subscription as `?5`, with disposition `?8`: the
        /// started process `?7` or terminal refusal `?9`.
        insert = "INSERT INTO trigger_deliveries (
                occurrence_id, subscription_id, process_id, subscription_incarnation,
                subscription_revision, subscription_snapshot_json, created_at_ms, status, refusal_json
             )
             VALUES (?1, ?2, ?7, ?3, ?4, ?5, ?6, ?8, ?9)";

        /// The deliveries occurrence `?1` already holds, for a plan that
        /// found the occurrence recorded and reports it again.
        select_snapshots_by_occurrence = "SELECT process_id, created_at_ms, subscription_snapshot_json, status, refusal_json
             FROM trigger_deliveries
             WHERE occurrence_id = ?1";

        /// Every process a bound delivery started.
        select_distinct_process_ids = "SELECT DISTINCT process_id
             FROM trigger_deliveries
             WHERE status = 'started'
             ORDER BY process_id ASC";

        /// Every delivery's identity, for a retention pass to compare
        /// against the process registry.
        select_retention_candidates = "SELECT occurrence_id, subscription_id, process_id
             FROM trigger_deliveries
             WHERE status = 'started'
             ORDER BY occurrence_id ASC, subscription_id ASC";

        /// Every reservation in the store.
        ///
        /// The four listings below were one `format!` per backend over a
        /// `where_clause` argument. They are four statements now, one per
        /// caller, and this one carries no predicate at all — it used to say
        /// `WHERE 1 = 1` on SQLite and `WHERE TRUE` on PostgreSQL, which is
        /// the same absence spelled two ways.
        list_all = "SELECT d.process_id, d.created_at_ms, o.record_json,
                    d.subscription_snapshot_json, d.status, d.refusal_json
             FROM trigger_deliveries d
             JOIN trigger_occurrences o ON o.occurrence_id = d.occurrence_id
             ORDER BY d.created_at_ms ASC, d.occurrence_id ASC, d.subscription_id ASC";

        /// Every reservation occurrence `?1` caused.
        list_by_occurrence_id = "SELECT d.process_id, d.created_at_ms, o.record_json,
                    d.subscription_snapshot_json, d.status, d.refusal_json
             FROM trigger_deliveries d
             JOIN trigger_occurrences o ON o.occurrence_id = d.occurrence_id
             WHERE d.occurrence_id = ?1
             ORDER BY d.created_at_ms ASC, d.occurrence_id ASC, d.subscription_id ASC";

        /// Every reservation subscription `?1` received.
        list_by_subscription_id = "SELECT d.process_id, d.created_at_ms, o.record_json,
                    d.subscription_snapshot_json, d.status, d.refusal_json
             FROM trigger_deliveries d
             JOIN trigger_occurrences o ON o.occurrence_id = d.occurrence_id
             WHERE d.subscription_id = ?1
             ORDER BY d.created_at_ms ASC, d.occurrence_id ASC, d.subscription_id ASC";

        /// The reservation that started process `?1`.
        list_by_process_id = "SELECT d.process_id, d.created_at_ms, o.record_json,
                    d.subscription_snapshot_json, d.status, d.refusal_json
             FROM trigger_deliveries d
             JOIN trigger_occurrences o ON o.occurrence_id = d.occurrence_id
             WHERE d.process_id = ?1
             ORDER BY d.created_at_ms ASC, d.occurrence_id ASC, d.subscription_id ASC";
    }
}
