//! `trigger_occurrences`: one append-only row per trigger firing.

/// The table's unprefixed name.
pub const TABLE: &str = "trigger_occurrences";

/// Every column an ingress writes, in insert order.
///
/// `reclaimable_at_ms` is absent on purpose: a fresh occurrence is never
/// reclaimable, and the column is armed by a later `UPDATE` once the firing's
/// fan-out is known.
pub const INSERT_COLUMNS: &str = "occurrence_id, idempotency_key, source_type, source_key,
                occurred_at_ms, record_json";

/// The durable record, with the id beside it.
///
/// The one multi-column read of this table. `record_json` is the occurrence;
/// the typed columns exist for the listing's predicates and for the source
/// index, and the record already carries every one of them.
pub const RECORD_COLUMNS: &str = "occurrence_id, record_json";

/// SQLite's spelling of the reclamation sweep's scope aggregate.
///
/// Not a narrow projection: it is four counts over the whole table, which is
/// what makes `NothingToDo` witnessed emptiness rather than an assumption.
/// It is named here because it is a projection of more than one column over
/// this table and the gate refuses an unnamed one — and because naming it is
/// what keeps the two backends' spellings side by side. The fork is the JSON
/// extraction alone, which is why there are two constants and not one; see
/// [`RECLAMATION_SCOPE_COUNTS_POSTGRES`].
pub const RECLAMATION_SCOPE_COUNTS_SQLITE: &str = "COUNT(*) AS inspected_count,
                        COUNT(*) FILTER (
                            WHERE reclaimable_at_ms IS NULL
                              AND COALESCE(json_extract(record_json, '$.outcome.kind'), 'fired') = 'fired'
                        ) AS live_fan_out_count,
                        COUNT(*) FILTER (
                            WHERE COALESCE(json_extract(record_json, '$.outcome.kind'), 'fired') != 'fired'
                        ) AS audit_retained_count,
                        COUNT(*) FILTER (
                            WHERE reclaimable_at_ms > ?1
                              AND COALESCE(json_extract(record_json, '$.outcome.kind'), 'fired') = 'fired'
                        ) AS grace_deferred_count";

/// PostgreSQL's spelling of [`RECLAMATION_SCOPE_COUNTS_SQLITE`]: the same four
/// counts, reading the outcome through `jsonb #>>` instead of `json_extract`.
pub const RECLAMATION_SCOPE_COUNTS_POSTGRES: &str = "COUNT(*) AS inspected_count,
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
                        ) AS grace_deferred_count";

crate::statements! {
    /// `trigger_occurrences` statements both backends issue verbatim.
    pub struct OccurrenceStatements @ "trigger_occurrence" {
        /// Append the occurrence `?1` under idempotency key `?2`.
        insert = "INSERT INTO trigger_occurrences (
                occurrence_id, idempotency_key, source_type, source_key,
                occurred_at_ms, record_json
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)";

        /// Every occurrence a filter admits: `?1` source type, `?2` source
        /// key, `?3` inclusive start and `?4` exclusive end of the occurrence
        /// window, each skipped when bound NULL.
        ///
        /// Four optional predicates that both backends used to build — SQLite
        /// with `push_str`, PostgreSQL with a `QueryBuilder` — over the same
        /// predicate set and the same order. Unlike the subscription listing
        /// there is no Rust filter behind this one, so the statement is the
        /// whole contract: bind NULL and the predicate is not applied.
        list_filtered = "SELECT occurrence_id, record_json
             FROM trigger_occurrences
             WHERE (?1 IS NULL OR source_type = ?1)
               AND (?2 IS NULL OR source_key = ?2)
               AND (?3 IS NULL OR occurred_at_ms >= ?3)
               AND (?4 IS NULL OR occurred_at_ms < ?4)
             ORDER BY occurred_at_ms ASC, occurrence_id ASC";

        /// Arm occurrence `?1` for reclamation at `?2`, keeping the first
        /// stamp. The `IS NULL` guard is what makes the grace period start
        /// once, at the instant the firing's last delivery went away.
        arm_reclaimable = "UPDATE trigger_occurrences
             SET reclaimable_at_ms = ?2
             WHERE occurrence_id = ?1 AND reclaimable_at_ms IS NULL";
    }
}
