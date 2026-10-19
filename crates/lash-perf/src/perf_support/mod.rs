pub mod async_operations;
pub mod dhat;
pub mod git;
pub mod instructions;
pub mod memory;
pub mod metrics;
pub mod paths;
pub mod report;
pub mod scheduler;
pub mod stack;
pub mod tempdir;
pub mod time;
pub mod trace_file;

/// Refuse a PostgreSQL performance run that cannot retain acknowledged local
/// commits across power loss. Provisioning owns these settings; the harness
/// validates them before opening its measured window, without changing them.
///
/// # Errors
/// Returns an error when policy is unavailable or selects a weaker contract.
pub async fn require_durable_postgres<'e>(
    executor: impl sqlx::Executor<'e, Database = sqlx::Postgres>,
) -> anyhow::Result<()> {
    let (fsync, commit): (String, String) =
        sqlx::query_as("SELECT current_setting('fsync'), current_setting('synchronous_commit')")
            .fetch_one(executor)
            .await?;
    validate_postgres_durability(&fsync, &commit)
}

fn validate_postgres_durability(fsync: &str, commit: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        fsync == "on" && matches!(commit, "on" | "local" | "remote_write" | "remote_apply"),
        "performance runs require fsync=on and synchronous WAL commit; got fsync={fsync}, synchronous_commit={commit}"
    );
    Ok(())
}

/// Read the selected backend's actual policy before starting any timer.
///
/// # Errors
/// Returns the selected connection's query error; no settings are invented.
pub async fn postgres_settings(pool: &sqlx::PgPool) -> anyhow::Result<String> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT name, setting FROM pg_settings WHERE name IN ('fsync', 'synchronous_commit', 'full_page_writes', 'wal_level', 'server_version') ORDER BY name")
        .fetch_all(pool).await?;
    Ok(serde_json::to_string(
        &rows
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>(),
    )?)
}

#[cfg(test)]
mod durability_tests {
    /// PERFORMANCE-DURABILITY: counts and latency must measure the promised
    /// acknowledged-commit contract, including every local synchronous level.
    #[test]
    fn performance_requires_fsync_and_synchronous_wal_commit() {
        for level in ["on", "local", "remote_write", "remote_apply"] {
            assert!(super::validate_postgres_durability("on", level).is_ok());
            assert!(super::validate_postgres_durability("off", level).is_err());
        }
        for level in ["off", "unknown"] {
            assert!(super::validate_postgres_durability("on", level).is_err());
        }
    }
}
