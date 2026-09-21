//! `checkpoint_blob_refs`: one row per edge from a checkpoint manifest to a
//! component blob it references.
//!
//! Every statement over this table forks. The batch insert is `json_each` on
//! SQLite and `unnest` on PostgreSQL, the reclaim reads bind one ref on SQLite
//! and an array on PostgreSQL, and the two anti-joins name the durable head
//! table, which the two backends spell differently (ADR 0098). So this module
//! owns the table's name and its column lists, and both backends own their own
//! statements.

/// The table's unprefixed name.
pub const TABLE: &str = "checkpoint_blob_refs";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "checkpoint_ref, blob_ref";
