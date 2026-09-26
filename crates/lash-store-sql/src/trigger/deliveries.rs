//! `trigger_deliveries`: one row per subscription an occurrence fired at.
//!
//! The row freezes the subscription as it stood when the firing reserved the
//! delivery (`subscription_snapshot_json`), so a later edit to the
//! subscription cannot retroactively change what was delivered.
//!
//! `process_id` is the delivery's binding: `NULL` while the reservation's
//! start has not completed, and the minted id of the process its start key
//! registered once it has (ADR 0107). A bound delivery is never started
//! again.

/// The table's unprefixed name.
pub const TABLE: &str = "trigger_deliveries";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str =
    "occurrence_id, subscription_id, process_id, subscription_incarnation,
                subscription_revision, subscription_snapshot_json, created_at_ms";

/// What a caller reading one occurrence's reservations needs: the process the
/// delivery started, when it was reserved, and the frozen subscription.
///
/// Narrow because the occurrence is already in the caller's hand — it is the
/// occurrence whose id keyed this read — so re-reading its id, and the
/// incarnation and revision the snapshot already carries, would be reading the
/// same facts twice.
pub const SNAPSHOT_COLUMNS: &str = "process_id, created_at_ms, subscription_snapshot_json";

/// The delivery's identity, and nothing else: what a retention pass compares
/// against the process registry to decide whether a delivery is still owed.
///
/// Narrow because the sweep loads every row in the table and never decodes
/// one: carrying `subscription_snapshot_json` here would read the whole frozen
/// subscription — an unbounded column — once per delivery, to answer a
/// question about three ids.
pub const RETENTION_CANDIDATE_COLUMNS: &str = "occurrence_id, subscription_id, process_id";

/// What a delivery listing reports: the reservation joined to the occurrence
/// that caused it.
///
/// The one projection of this table that spans two of them. It is narrow on
/// both sides — the delivery's own incarnation and revision columns are in the
/// snapshot, and the occurrence contributes only its record.
pub const RESERVATION_COLUMNS: &str = "d.process_id, d.created_at_ms, o.record_json,
                    d.subscription_snapshot_json";

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

/// PostgreSQL's spelling of [`SESSION_OWNER_SCOPE_SQLITE`].
pub const SESSION_OWNER_SCOPE_POSTGRES: &str = "'session:' ||
                    (subscription_snapshot_json::jsonb #>> '{owner_scope,session_id}')";

crate::statements! {
    /// `trigger_deliveries` statements both backends issue verbatim.
    pub struct DeliveryStatements @ "trigger_delivery" {
        /// Reserve the delivery of occurrence `?1` to subscription `?2`,
        /// freezing the subscription as `?5`. The reservation starts unbound.
        insert = "INSERT INTO trigger_deliveries (
                occurrence_id, subscription_id, process_id, subscription_incarnation,
                subscription_revision, subscription_snapshot_json, created_at_ms
             )
             VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6)";

        /// Bind delivery `?1`/`?2` to the process `?3` its start registered.
        /// Matches only an unbound row or a row already bound to `?3`, so a
        /// caller reads zero affected rows as a conflicting binding.
        bind_process = "UPDATE trigger_deliveries SET process_id = ?3
             WHERE occurrence_id = ?1 AND subscription_id = ?2
               AND (process_id IS NULL OR process_id = ?3)";

        /// The reservations occurrence `?1` already holds, for an ingress that
        /// found the occurrence durable and is reporting it again.
        select_snapshots_by_occurrence = "SELECT process_id, created_at_ms, subscription_snapshot_json
             FROM trigger_deliveries
             WHERE occurrence_id = ?1";

        /// Every process a bound delivery started.
        select_distinct_process_ids = "SELECT DISTINCT process_id
             FROM trigger_deliveries
             WHERE process_id IS NOT NULL
             ORDER BY process_id ASC";

        /// Every bound delivery's identity, for a retention pass to compare
        /// against the process registry. An unbound reservation is not a
        /// candidate: its start still owes the delivery a process.
        select_retention_candidates = "SELECT occurrence_id, subscription_id, process_id
             FROM trigger_deliveries
             WHERE process_id IS NOT NULL
             ORDER BY occurrence_id ASC, subscription_id ASC";

        /// Every reservation in the store.
        ///
        /// The four listings below were one `format!` per backend over a
        /// `where_clause` argument. They are four statements now, one per
        /// caller, and this one carries no predicate at all — it used to say
        /// `WHERE 1 = 1` on SQLite and `WHERE TRUE` on PostgreSQL, which is
        /// the same absence spelled two ways.
        list_all = "SELECT d.process_id, d.created_at_ms, o.record_json,
                    d.subscription_snapshot_json
             FROM trigger_deliveries d
             JOIN trigger_occurrences o ON o.occurrence_id = d.occurrence_id
             ORDER BY d.created_at_ms ASC, d.occurrence_id ASC, d.subscription_id ASC";

        /// Every reservation occurrence `?1` caused.
        list_by_occurrence_id = "SELECT d.process_id, d.created_at_ms, o.record_json,
                    d.subscription_snapshot_json
             FROM trigger_deliveries d
             JOIN trigger_occurrences o ON o.occurrence_id = d.occurrence_id
             WHERE d.occurrence_id = ?1
             ORDER BY d.created_at_ms ASC, d.occurrence_id ASC, d.subscription_id ASC";

        /// Every reservation subscription `?1` received.
        list_by_subscription_id = "SELECT d.process_id, d.created_at_ms, o.record_json,
                    d.subscription_snapshot_json
             FROM trigger_deliveries d
             JOIN trigger_occurrences o ON o.occurrence_id = d.occurrence_id
             WHERE d.subscription_id = ?1
             ORDER BY d.created_at_ms ASC, d.occurrence_id ASC, d.subscription_id ASC";

        /// The reservation that started process `?1`.
        list_by_process_id = "SELECT d.process_id, d.created_at_ms, o.record_json,
                    d.subscription_snapshot_json
             FROM trigger_deliveries d
             JOIN trigger_occurrences o ON o.occurrence_id = d.occurrence_id
             WHERE d.process_id = ?1
             ORDER BY d.created_at_ms ASC, d.occurrence_id ASC, d.subscription_id ASC";
    }
}
