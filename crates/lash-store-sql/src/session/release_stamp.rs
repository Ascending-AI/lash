//! `release_stamp`: the one row recording which lash release wrote this
//! deployment.
//!
//! Every statement over this table forks. SQLite spells the singleton flag as
//! the integer `1` and PostgreSQL as `TRUE`, and the write instant comes from
//! the opening host on SQLite and from the server clock on PostgreSQL, so
//! nothing here is byte-identical after rendering and the module owns only the
//! table's name and its column lists.

/// The table's unprefixed name.
pub const TABLE: &str = "release_stamp";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "singleton, release_version, schema_versions, written_at_epoch_ms";
