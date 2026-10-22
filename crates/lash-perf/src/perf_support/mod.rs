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

/// Register a benchmark runner and its code package under the same owner/key.
/// Package roots come from the runner's manifests, as in a production host.
///
/// # Errors
/// Returns a refusal to enumerate the runner's tool manifests.
pub fn tool_plugin(
    owner: &'static str,
    key: lash::plugins::ToolProviderKey,
    tools: std::sync::Arc<dyn lash_core::ToolProvider>,
) -> Result<lash::plugins::StaticPluginFactory, lash::plugins::PluginError> {
    let packages = tool_packages(owner, key, std::sync::Arc::clone(&tools))?;
    Ok(lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(owner),
        lash::plugins::PluginSpec::new()
            .with_tool_provider(key, tools)
            .with_package_provider(key, packages),
    ))
}

/// Advertise the operation packages of a benchmark's registered tool runner.
/// Read registrations use this alongside their explicit read adapter.
///
/// # Errors
/// Returns a refusal to enumerate the runner's tool manifests.
pub fn tool_packages(
    owner: &'static str,
    key: lash::plugins::ToolProviderKey,
    tools: std::sync::Arc<dyn lash_core::ToolProvider>,
) -> Result<std::sync::Arc<dyn lash::package::PackageProvider>, lash::plugins::PluginError> {
    let roots = tools
        .tool_manifests(&lash_core::OwnerConfigView::default())?
        .into_iter()
        .map(|manifest| manifest.operation.package.as_slice()[0].clone())
        .collect();
    Ok(std::sync::Arc::new(
        lash::package::ToolPackageProvider::new(owner, key, roots, tools),
    ))
}

/// Capture linked operations and machine requirements with the production
/// process-controls policy, translating providers to their registered twins.
pub fn process_policy(
    components: &[lash_core::ProcessConfigKey<()>],
) -> std::sync::Arc<dyn lash_core::ProcessEnvPolicy> {
    let mut components = components.to_vec();
    if !components
        .iter()
        .any(|key| key.component() == lash::vm::KERNEL_SERVICES_CONFIG.component())
    {
        components.push(lash::vm::KERNEL_SERVICES_CONFIG);
    }
    lash_plugin_process_controls::policy::restrict_or(
        lash_plugin_process_controls::policy::from_run(move |_| {
            components
                .iter()
                .map(|key| lash_core::ProcessComponentEntry::new(*key, &(), std::iter::empty()))
                .collect()
        }),
    )
}

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
