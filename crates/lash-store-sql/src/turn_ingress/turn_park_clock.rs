//! `turn_park_clock`: the turn park feed's single-row sequence (FIG-3659).
//!
//! One row, two counters, and no shared statement at all: every operation over
//! this table forks. `current_seq` is the sequence every feed event is
//! allocated from, which makes `turn_park_events.seq` order equal commit
//! order; `compaction_horizon` is the host-raised cursor below which a feed
//! read is refused typed. The singleton flag is `INTEGER 1` on SQLite and
//! `BOOLEAN TRUE` on PostgreSQL, and PostgreSQL bumps and reads the sequence
//! in one `RETURNING` round trip where SQLite issues the update and the read
//! separately under its write lock. The table still has an owner module,
//! because the layout's rule is one module per table rather than one module
//! per shared statement.

/// The table's unprefixed name.
pub const TABLE: &str = "turn_park_clock";

/// Every column, in insert order. Written once, by the schema's seed row.
pub const INSERT_COLUMNS: &str = "singleton, current_seq, compaction_horizon";
