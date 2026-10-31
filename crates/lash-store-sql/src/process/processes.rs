//! `processes`: one row per registered process.
//!
//! `start_json` is immutable creation metadata; `execution_json` is the current
//! input/definition pair, replaced only on succession; `lifecycle_json` is the
//! mutable event fold. Reads concatenate their disjoint object members to
//! assemble the existing rich `ProcessRecord`, without storing it again. All three
//! codecs emit nonempty JSON objects. Text concatenation preserves payload
//! strings, including escaped NULs which PostgreSQL JSONB cannot represent.
//!
//! `status`, `last_event_sequence` and `cancel_requested_at_ms` are generated
//! from `lifecycle_json` in both dialects. Every changed batch saves that object
//! once, with its indexed timestamp and change sequence, in the event transaction.
//!
//! `status` carries domain vocabulary (`lash_execution_contracts::process::ProcessStatus`). A
//! statement here names a lifecycle partition as a `{{term(column)}}` token
//! and never spells it.

/// The table's unprefixed name.
pub const TABLE: &str = "processes";

/// Every column a statement writes, in insert order. The only statements
/// that name all of them are the two backends' registration inserts.
pub const INSERT_COLUMNS: &str = "process_id, start_key, originator_id,
                identity_kind, identity_label, created_at_ms, updated_at_ms,
                change_seq, lifetime_scope_kind, lifetime_scope_id,
                lifetime, lifecycle_json, consumer_hold_key,
                consumer_hold_scope_kind, consumer_hold_scope_id, consumer_hold_cancels, start_json, execution_json";

/// What the change feed reports for a live row.
///
/// The feed unions live rows with tombstones, so both arms carry the same
/// three result columns: the sequence the caller resumes from, the kind that
/// says which arm produced the row, and the payload. The literal `'upsert'` is
/// the arm's name, not a lifecycle label.
pub const CHANGE_FEED_UPSERT_COLUMNS: &str = "change_seq, 'upsert' AS kind, (substr(start_json, 1, length(start_json) - 1) || ',' || substr(execution_json, 2, length(execution_json) - 2) || ',' || substr(lifecycle_json, 2)) AS payload";

crate::statements! {
    /// `processes` statements both backends issue verbatim.
    pub struct ProcessStatements @ "process" {
        /// Inclusive bound for a fleet scan; process ids are not time ordered.
        select_max_process_id = "SELECT MAX(process_id) FROM processes";

        /// The stored record for `?1`.
        select_record_json_by_id = "SELECT (substr(start_json, 1, length(start_json) - 1) || ',' || substr(execution_json, 2, length(execution_json) - 2) || ',' || substr(lifecycle_json, 2)) AS record_json FROM processes WHERE process_id = ?1";

        /// The retained process registered under start key `?1`, if any: the
        /// idempotency half of a registration (ADR 0107).
        select_record_json_by_start_key = "SELECT (substr(start_json, 1, length(start_json) - 1) || ',' || substr(execution_json, 2, length(execution_json) - 2) || ',' || substr(lifecycle_json, 2)) AS record_json FROM processes WHERE start_key = ?1";

        exists_by_id = "SELECT EXISTS(SELECT 1 FROM processes WHERE process_id = ?1)";

        /// Release the consumer hold `?2` holds on `?1` (ADR 0116 §3.6): a
        /// no-op when the row carries another hold or none.
        release_consumer_hold = "UPDATE processes
             SET consumer_hold_key = NULL, consumer_hold_scope_kind = NULL,
                 consumer_hold_scope_id = NULL, consumer_hold_cancels = NULL
             WHERE process_id = ?1 AND consumer_hold_key = ?2";

        /// The processes held under the consumer hold `?1` whose call owes
        /// them a cancel now that it is abandoned (ADR 0116 §3.4).
        select_owed_cancels = "SELECT process_id FROM processes
             WHERE consumer_hold_key = ?1 AND consumer_hold_cancels IS TRUE
             ORDER BY process_id";

        /// Release every consumer hold owned by the scope `(?1, ?2)`: the
        /// scope's close ends every wait its calls still hold.
        release_consumer_holds_owned_by = "UPDATE processes
             SET consumer_hold_key = NULL, consumer_hold_scope_kind = NULL,
                 consumer_hold_scope_id = NULL, consumer_hold_cancels = NULL
             WHERE consumer_hold_scope_kind = ?1 AND consumer_hold_scope_id = ?2";

        /// The identity columns are absent because none of them is mutable.
        update_mutable_columns = "UPDATE processes
             SET updated_at_ms = ?2, change_seq = ?3, lifecycle_json = ?4
             WHERE process_id = ?1";

        /// A successor replaces its execution pair with the same one batch save.
        update_execution_columns = "UPDATE processes
             SET updated_at_ms = ?2, change_seq = ?3, lifecycle_json = ?4, execution_json = ?5
             WHERE process_id = ?1";

    }
}
