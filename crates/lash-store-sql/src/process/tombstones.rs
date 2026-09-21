//! `process_tombstones`: the durable record that a process was pruned.
//!
//! A tombstone outlives the row it replaces, which is what lets a caller be
//! told "no longer retained" rather than "unknown".

/// The table's unprefixed name.
pub const TABLE: &str = "process_tombstones";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str =
    "process_id, incarnation, terminal_label, pruned_at_ms, pruned_change_seq";

/// What a caller asking after a pruned process is told.
///
/// The two facts a `ProcessTombstoneStamp` carries: how the process ended and
/// when its row was reclaimed. The change sequence is absent because it
/// belongs to the feed, not to the answer.
pub const TERMINAL_STAMP_COLUMNS: &str = "terminal_label, pruned_at_ms";

/// What the SQLite change feed reports for a pruned row.
///
/// The deleted arm of the feed's `UNION ALL`: the same three result columns as
/// [`super::processes::CHANGE_FEED_UPSERT_COLUMNS`], with the tombstone's
/// facts assembled into the payload. It is its own constant because SQLite
/// spells the assembly `json_object` and PostgreSQL spells it
/// `json_build_object` over a cast.
pub const SQLITE_CHANGE_FEED_DELETED_COLUMNS: &str = "pruned_change_seq, 'deleted' AS kind,
                    json_object(
                        'process_id', process_id,
                        'incarnation', incarnation,
                        'terminal_label', terminal_label,
                        'pruned_at_ms', pruned_at_ms,
                        'pruned_change_seq', pruned_change_seq
                    ) AS payload";

/// What the PostgreSQL change feed reports for a pruned row. See
/// [`SQLITE_CHANGE_FEED_DELETED_COLUMNS`].
pub const PG_CHANGE_FEED_DELETED_COLUMNS: &str = "pruned_change_seq,
                    'deleted' AS kind,
                    json_build_object(
                        'process_id', process_id,
                        'incarnation', incarnation,
                        'terminal_label', terminal_label,
                        'pruned_at_ms', pruned_at_ms,
                        'pruned_change_seq', pruned_change_seq
                    )::TEXT AS payload";

crate::statements! {
    /// `process_tombstones` statements both backends issue verbatim.
    pub struct TombstoneStatements @ "process_tombstone" {
        /// How process `?1` most recently ended, and when it was reclaimed.
        select_latest_terminal = "SELECT terminal_label, pruned_at_ms
                 FROM process_tombstones WHERE process_id = ?1
                 ORDER BY incarnation DESC LIMIT 1";

        /// The same, for the exact incarnation `?1` / `?2` a caller named.
        select_terminal_for_incarnation = "SELECT terminal_label, pruned_at_ms
                 FROM process_tombstones
                 WHERE process_id = ?1 AND incarnation = ?2";

        /// The newest incarnation of `?1` that has been pruned: what tells a
        /// caller holding an older reference that it was superseded rather
        /// than reclaimed.
        select_latest_incarnation = "SELECT incarnation FROM process_tombstones
                 WHERE process_id = ?1 ORDER BY incarnation DESC LIMIT 1";
    }
}
