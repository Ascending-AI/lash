// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use sqlx::{Connection, PgConnection, PgPool};

// "LASH_PGT" encoded as a positive i64. Every test process that uses the
// configured shared database must hold this session-level lock for its entire
// database interaction.
const SHARED_DATABASE_LOCK_KEY: i64 = 0x4c41_5348_5f50_4754;

/// Reset the shared database to a clean slate for one test.
///
/// The configured database outlives every test process that touches it, so a
/// test whose scenario ids are deterministic — every conformance law — must
/// not see a previous run's journaled rows: a replayed `completed` row serves
/// its terminal without re-running the body the law is watching for.
///
/// Call this while holding [`SharedDatabaseLock`], before the test builds any
/// host over the database, so the truncate cannot race another test's worlds.
///
/// The truncate set derives from the live catalog rather than a
/// hand-maintained table list: a new `lash_*` table can no longer silently
/// bleed state between cases. `lash_schema_versions` is excluded — it holds
/// the component schema version gate, not per-case fixture rows — and
/// `lash_catalog_identity` holds the install's identity, likewise not fixture
/// state.
// Not every target that compiles this module calls it; the includers'
// `#[allow(dead_code)]` on `mod support` predates it.
#[allow(dead_code)]
pub async fn reset(pool: &PgPool) {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = 'public'
           AND tablename LIKE 'lash\\_%'
           AND tablename NOT IN ('lash_schema_versions', 'lash_catalog_identity')
         ORDER BY tablename",
    )
    .fetch_all(pool)
    .await
    .expect("list lash_* tables to reset");
    assert!(
        !tables.is_empty(),
        "expected the lash_* schema tables to exist before reset"
    );
    let truncate = format!("TRUNCATE {} RESTART IDENTITY CASCADE", tables.join(", "));
    sqlx::query(&truncate)
        .execute(pool)
        .await
        .expect("reset postgres tables");
    sqlx::query(
        "INSERT INTO lash_process_change_clock (singleton, current_seq)
         VALUES (TRUE, 0)
         ON CONFLICT (singleton) DO UPDATE SET current_seq = EXCLUDED.current_seq",
    )
    .execute(pool)
    .await
    .expect("reset postgres process change clock");
    sqlx::query(
        "INSERT INTO lash_turn_park_clock (singleton, current_seq)
         VALUES (TRUE, 0)
         ON CONFLICT (singleton) DO UPDATE SET current_seq = EXCLUDED.current_seq",
    )
    .execute(pool)
    .await
    .expect("reset postgres turn park clock");
    sqlx::query(
        "INSERT INTO lash_process_park_clock (singleton, current_seq)
         VALUES (TRUE, 0)
         ON CONFLICT (singleton) DO UPDATE SET current_seq = EXCLUDED.current_seq",
    )
    .execute(pool)
    .await
    .expect("reset postgres process park clock");
}

pub fn database_url() -> Option<String> {
    match std::env::var("LASH_POSTGRES_DATABASE_URL") {
        Ok(database_url) if !database_url.is_empty() => Some(database_url),
        Ok(_) => {
            if std::env::var("LASH_REQUIRE_POSTGRES").as_deref() == Ok("1") {
                panic!("LASH_POSTGRES_DATABASE_URL must be non-empty when LASH_REQUIRE_POSTGRES=1");
            }
            None
        }
        Err(error) => {
            if std::env::var("LASH_REQUIRE_POSTGRES").as_deref() == Ok("1") {
                panic!(
                    "LASH_POSTGRES_DATABASE_URL must be set when LASH_REQUIRE_POSTGRES=1: {error}"
                );
            }
            None
        }
    }
}

pub struct SharedDatabaseLock {
    _connection: PgConnection,
}

impl SharedDatabaseLock {
    pub async fn acquire(database_url: &str) -> Self {
        let mut connection = PgConnection::connect(database_url)
            .await
            .expect("connect Postgres test advisory lock");
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(SHARED_DATABASE_LOCK_KEY)
            .execute(&mut connection)
            .await
            .expect("acquire Postgres test advisory lock");
        Self {
            _connection: connection,
        }
    }
}
