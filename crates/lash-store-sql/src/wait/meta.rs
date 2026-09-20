//! `await_event_meta`: the single row holding the promise signing secret.
//!
//! The table has no shared statement. Its only read selects on the singleton
//! flag, which SQLite stores as `INTEGER 1` and PostgreSQL as `BOOLEAN TRUE`,
//! so the two texts genuinely fork and both are declared in their backend with
//! a manifest entry. Nothing else about the table forks, which is why the name
//! and the column list still live here.

/// The table's unprefixed name.
pub const TABLE: &str = "await_event_meta";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "singleton, signing_secret";

/// The only projection: the secret itself, read once when a store opens.
pub const SECRET_COLUMNS: &str = "signing_secret";
