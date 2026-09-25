//! `await_event_meta`: the single row holding the promise signing secret.
//!
//! The table has no shared statement. Its only read selects on the singleton
//! flag, which SQLite stores as `INTEGER 1`, so the statement is declared in
//! the SQLite store with a manifest entry (PostgreSQL journals no promises,
//! ADR 0104). The name and the column list still live here.

/// The table's unprefixed name.
pub const TABLE: &str = "await_event_meta";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "singleton, signing_secret";

/// The only projection: the secret itself, read once when a store opens.
pub const SECRET_COLUMNS: &str = "signing_secret";
