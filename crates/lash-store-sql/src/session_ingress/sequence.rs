//! The counter retained for the lifetime of one session.

/// The table's unprefixed name.
pub const TABLE: &str = "session_ingress_sequence";

/// The session key and the last allocated position written by the upsert.
pub const INSERT_COLUMNS: &str = "session_id, enqueue_seq";

/// Allocation returns only the new position to the admitting producer.
pub const ALLOCATION_COLUMNS: &str = "enqueue_seq";
