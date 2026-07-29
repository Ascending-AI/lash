struct WorkbenchStores {
    session_store_factory: Arc<dyn lash::persistence::SessionStoreFactory>,
    process_registry: Arc<dyn lash::process::ProcessRegistry>,
    process_continuations: Arc<dyn lash::process::ProcessContinuationStore>,
    trigger_store: Arc<dyn lash::triggers::TriggerStore>,
    artifact_store: Arc<dyn lash::persistence::LashlangArtifactStore>,
    process_env_store: Arc<dyn lash::persistence::ProcessExecutionEnvStore>,
    backend: &'static str,
}

impl WorkbenchStores {
    async fn open(data_dir: &std::path::Path, database_url: Option<&str>) -> AnyhowResult<Self> {
        match database_url {
            Some(database_url) => Self::open_postgres(database_url).await,
            None => Self::open_sqlite(data_dir).await,
        }
    }

    async fn open_sqlite(data_dir: &std::path::Path) -> AnyhowResult<Self> {
        let process_registry_path = data_dir.join("processes.db");
        let session_store_root = data_dir.join("lash-sessions");
        let session_store_factory = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new_with_process_registry(
                &session_store_root,
                &process_registry_path,
            ),
        ) as Arc<dyn lash::persistence::SessionStoreFactory>;
        let process_store = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(
                &process_registry_path,
                session_store_root,
            )
                .await
                .context("open SQLite process registry")?,
        );
        let process_registry = process_store.clone() as Arc<dyn lash::process::ProcessRegistry>;
        let process_continuations =
            process_store as Arc<dyn lash::process::ProcessContinuationStore>;
        let trigger_store = Arc::new(
            lash_sqlite_store::SqliteTriggerStore::open(&data_dir.join("triggers.db"))
                .await
                .context("open SQLite trigger store")?,
        ) as Arc<dyn lash::triggers::TriggerStore>;
        let artifacts = Arc::new(
            lash_sqlite_store::Store::open(&data_dir.join("artifacts.db"))
                .await
                .context("open SQLite Lashlang store")?,
        );
        Ok(Self {
            session_store_factory,
            process_registry,
            process_continuations,
            trigger_store,
            artifact_store: artifacts.clone(),
            process_env_store: artifacts,
            backend: "sqlite",
        })
    }

    async fn open_postgres(database_url: &str) -> AnyhowResult<Self> {
        anyhow::ensure!(
            !database_url.trim().is_empty(),
            "AGENT_WORKBENCH_DATABASE_URL must not be empty"
        );
        let storage = lash_postgres_store::PostgresStorage::connect(database_url)
            .await
            .context("open Postgres workbench storage")?;
        let artifacts = Arc::new(storage.lashlang_artifact_store());
        let process_store = Arc::new(storage.process_registry());
        Ok(Self {
            session_store_factory: Arc::new(
                storage
                    .session_store_factory_with_shared_process_registry(),
            ),
            process_registry: process_store.clone(),
            process_continuations: process_store,
            trigger_store: Arc::new(storage.trigger_store()),
            artifact_store: artifacts.clone(),
            process_env_store: artifacts,
            backend: "postgres",
        })
    }
}
