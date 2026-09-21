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

        /// Every occurrence in the window `?1`..`?2`, in listing order. The
        /// general listing: no source predicate to seek on, and no index over
        /// `occurred_at_ms` alone, so it scans by design.
        list_all = "SELECT occurrence_id, record_json
             FROM trigger_occurrences
             WHERE occurred_at_ms >= ?1
               AND occurred_at_ms <= ?2
             ORDER BY occurred_at_ms ASC, occurrence_id ASC";

        /// Every occurrence of source type `?1` in the window `?2`..`?3`.
        /// Seeks `(source_type, source_key, occurred_at_ms)` on its leading
        /// column.
        list_by_source_type = "SELECT occurrence_id, record_json
             FROM trigger_occurrences
             WHERE source_type = ?1
               AND occurred_at_ms >= ?2
               AND occurred_at_ms <= ?3
             ORDER BY occurred_at_ms ASC, occurrence_id ASC";

        /// Every occurrence of source `?1`/`?2` in the window `?3`..`?4`.
        /// Seeks `(source_type, source_key, occurred_at_ms)` on both key
        /// columns and ranges on the third.
        list_by_source = "SELECT occurrence_id, record_json
             FROM trigger_occurrences
             WHERE source_type = ?1
               AND source_key = ?2
               AND occurred_at_ms >= ?3
               AND occurred_at_ms <= ?4
             ORDER BY occurred_at_ms ASC, occurrence_id ASC";

        /// The `IS NULL` guard is what makes the grace period start once, at the instant the
        /// firing's last delivery went away.
        arm_reclaimable = "UPDATE trigger_occurrences
             SET reclaimable_at_ms = ?2
             WHERE occurrence_id = ?1 AND reclaimable_at_ms IS NULL";
    }
}

/// Which listing statement an occurrence filter is served by.
///
/// The same reasoning as [`super::subscriptions::ListShape`]: a listing whose
/// predicates are optional at the SQL level cannot seek, so each shape an
/// index serves gets a statement of plain equalities.
///
/// The window is not a shape. Both bounds are always bound — an unset start as
/// `i64::MIN`, an unset end as `i64::MAX` — so the comparison is always a
/// plain, sargable one against a value, and the third column of
/// `(source_type, source_key, occurred_at_ms)` is still a range. That makes
/// the SQL window a closed `[start, end]` over clamped bounds, which is a
/// superset of the filter's half-open `[start, end)` over the raw `u64` ones;
/// `TriggerOccurrenceFilter::matches` decides each record afterwards, so the
/// answer is the filter's own, to the bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListShape {
    /// Neither source field is set.
    All,
    /// Source type alone.
    BySourceType,
    /// Source type and source key.
    BySource,
}

impl ListShape {
    /// The shape of a filter that sets the fields these flags describe.
    ///
    /// A source key with no source type is not a seek on
    /// `(source_type, source_key, occurred_at_ms)`, so it is not a shape.
    #[must_use]
    pub const fn of(source_type: bool, source_key: bool) -> Self {
        match (source_type, source_key) {
            (true, true) => Self::BySource,
            (true, false) => Self::BySourceType,
            (false, _) => Self::All,
        }
    }
}

impl OccurrenceStatements {
    /// The listing statement for `shape`.
    #[must_use]
    pub fn list_for(&self, shape: ListShape) -> &crate::Rendered {
        match shape {
            ListShape::All => &self.list_all,
            ListShape::BySourceType => &self.list_by_source_type,
            ListShape::BySource => &self.list_by_source,
        }
    }
}
