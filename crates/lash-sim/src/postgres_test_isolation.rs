//! Per-test Postgres isolation for this crate's Postgres-backed unit tests.
//!
//! These suites run under a process-per-test runner alongside the
//! `lash-postgres-store` conformance suites, which truncate every `lash_*`
//! table on the configured database. Sharing one database across them means
//! each suite can truncate another's rows mid-run — the failures rotate and
//! never reproduce serially. Rather than joining the shared advisory lock and
//! serializing, each suite here takes a database of its own.

use lash_postgres_store::testing::IsolatedDatabase;

/// # Panics
///
/// Panics if an explicitly selected PostgreSQL leg has no database URL.
pub(crate) async fn isolated_database() -> IsolatedDatabase {
    IsolatedDatabase::create(&lash_postgres_store::testing::required_database_url()).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL; select inside a pg16 gate"]
async fn postgres_isolation_requires_a_database_url() {
    let _database = isolated_database().await;
}

#[test]
fn postgres_variants_never_pass_without_a_database_url() {
    assert_requires_database_url(
        "postgres_test_isolation::postgres_isolation_requires_a_database_url",
    );
}

#[cfg(test)]
pub(crate) fn assert_requires_database_url(law: &str) {
    let executable = std::env::current_exe().expect("test executable");
    for url in [None, Some(""), Some(" \t ")] {
        let mut command = std::process::Command::new(&executable);
        command
            .args(["--exact", law, "--include-ignored", "--nocapture"])
            .env_remove("LASH_POSTGRES_DATABASE_URL");
        if let Some(url) = url {
            command.env("LASH_POSTGRES_DATABASE_URL", url);
        }
        let output = command.output().expect("run PostgreSQL variant");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stdout.contains("running 1 test"), "{stdout}\n{stderr}");
        assert!(
            !output.status.success() && stdout.contains("0 passed; 1 failed"),
            "{law} with URL {url:?} passed vacuously: {stdout}\n{stderr}"
        );
        assert!(
            stderr.contains("LASH_POSTGRES_DATABASE_URL"),
            "{stdout}\n{stderr}"
        );
    }
}
