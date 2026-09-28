//! `turn_cancel_affected_inputs`: the per-item dispositions a cancellation
//! recorded, as PostgreSQL stores them.
//!
//! One row per ingress item a cancel request touched — a host input, or a
//! process wake the turn held undelivered (`item_kind`, FIG-3543) — ordered by `ordinal` so the
//! settlement replays them in the order the turn observed. The table exists on
//! PostgreSQL only: SQLite keeps the same facts as one `record_json` document
//! on its cancel-request row. It is still a table with one
//! owner, one name and one column list per projection — a backend having no
//! counterpart is no reason to leave the
//! statements unnamed at their call sites (FIG-3387).

/// The table's unprefixed name.
pub const TABLE: &str = "turn_cancel_affected_inputs";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str =
    "session_id, turn_id, ordinal, input_id, disposition, input_json, item_kind, batch_id";

/// What the settlement reads back: the item, its payload, what the
/// cancellation decided to do with it, its kind and — for a wake — the batch
/// that carries it.
///
/// The key columns are the read's own parameters, so projecting them again
/// would return the caller its own arguments once per row.
pub const SETTLEMENT_COLUMNS: &str = "input_id, input_json, disposition, item_kind, batch_id";
