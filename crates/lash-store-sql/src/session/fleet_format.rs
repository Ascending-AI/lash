//! `fleet_format`: the one row recording the durable-format generation every
//! writer in the fleet emits (ADR 0106 §1 `F`).
//!
//! Every statement over this table forks. SQLite spells the singleton flag as
//! the integer `1` and PostgreSQL as `TRUE`, and SQLite *upserts* the row —
//! a single-process deployment finalizes on open — where PostgreSQL only ever
//! provisions it, because the shared store's row belongs to the fleet rather
//! than to whichever build opens first. The module owns only the table's name
//! and its column lists.

/// The table's unprefixed name.
pub const TABLE: &str = "fleet_format";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "singleton, format_version";

/// The row as a reader consults it.
///
/// The full row minus `singleton`: the flag is the primary key of a one-row
/// table, so it is always the same value and carries no information a reader
/// could use.
pub const FORMAT_COLUMNS: &str = "format_version";
