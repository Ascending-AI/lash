//! The database a scenario runs over: SQLite in memory, a SQLite file or an
//! isolated PostgreSQL database, each with the store set and the durable
//! store a node runs on.

include!("dialect_store.rs");

/// The PostgreSQL server a PostgreSQL leg runs on, or `None` when the run
/// was handed none and the leg is skipped.
pub fn postgres_url() -> Option<String> {
    // Test code: the PostgreSQL leg reads its server from the environment
    // the target's runner hands it.
    #[allow(clippy::disallowed_methods)]
    std::env::var("LASH_POSTGRES_DATABASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
}
