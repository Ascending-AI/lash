//! Per-test Postgres isolation for this crate's Postgres-backed unit tests.
//!
//! These suites run under a process-per-test runner alongside the
//! `lash-postgres-store` conformance suites, which truncate every `lash_*`
//! table on the configured database. Sharing one database across them means
//! each suite can truncate another's rows mid-run — the failures rotate and
//! never reproduce serially. Rather than joining the shared advisory lock and
//! serializing, each suite here takes a database of its own.

use lash_postgres_store::testing::IsolatedDatabase;

/// Creates a database exclusive to the calling test, or `None` when Postgres is
/// not configured.
///
/// # Panics
///
/// Panics when `LASH_REQUIRE_POSTGRES=1` and no database URL is configured, so
/// a missing CI variable cannot silently skip the Postgres lane.
pub(crate) async fn isolated_database() -> Option<IsolatedDatabase> {
    let base_url = match std::env::var("LASH_POSTGRES_DATABASE_URL") {
        Ok(base_url) if !base_url.is_empty() => base_url,
        _ if std::env::var("LASH_REQUIRE_POSTGRES").as_deref() == Ok("1") => {
            panic!("LASH_POSTGRES_DATABASE_URL must be set when LASH_REQUIRE_POSTGRES=1")
        }
        _ => return None,
    };
    Some(IsolatedDatabase::create(&base_url).await)
}
