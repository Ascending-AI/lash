//! VM snapshots: the neutral statements of the `snapshots` domain (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L7 (FIG-5177): its statements, and its table's DDL in each
//! dialect's `durable` module, are that lane's. This file and the matching
//! file in each dialect are the only places its SQL may appear
//! (`scripts/check-durable-sql.py`).

/// The table's unprefixed name. I0 (FIG-5194) creates it; its statements are its
/// owner's.
pub const TABLE: &str = "exec_snapshots";
