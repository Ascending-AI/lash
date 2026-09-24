use super::*;

/// The SQL store set the workbench runs its Restate backend over: SQLite
/// under the data directory, or PostgreSQL when a database URL is configured.
pub(crate) struct WorkbenchStores {
    pub(crate) stores: Arc<dyn lash::durability::StoreSet>,
    pub(crate) artifact_store: Arc<dyn lash::persistence::LashlangArtifactStore>,
    pub(crate) backend: &'static str,
}

impl WorkbenchStores {
    pub(crate) async fn open(
        data_dir: &std::path::Path,
        database_url: Option<&str>,
    ) -> AnyhowResult<Self> {
        match database_url {
            Some(database_url) => Self::open_postgres(data_dir, database_url).await,
            None => Self::open_sqlite(data_dir).await,
        }
    }

    pub(crate) async fn open_sqlite(data_dir: &std::path::Path) -> AnyhowResult<Self> {
        crate::prior_store_layout::refuse_prior_store_layout(
            data_dir,
            &["processes.db", "triggers.db", "artifacts.db", "attachments"],
        )?;
        let stores = lash_sqlite_store::SqliteStoreSet::open(data_dir.join("lash-sessions"))
            .await
            .context("open the SQLite store set")?;
        Ok(Self {
            artifact_store: stores.process_env_store(),
            stores: Arc::new(stores),
            backend: "sqlite",
        })
    }

    pub(crate) async fn open_postgres(
        data_dir: &std::path::Path,
        database_url: &str,
    ) -> AnyhowResult<Self> {
        anyhow::ensure!(
            !database_url.trim().is_empty(),
            "AGENT_WORKBENCH_DATABASE_URL must not be empty"
        );
        let storage = lash_postgres_store::PostgresStorage::connect(database_url)
            .await
            .context("open Postgres workbench storage")?;
        let stores = lash_postgres_store::PostgresStoreSet::new(
            &storage,
            Arc::new(lash::persistence::FileAttachmentStore::new(
                data_dir.join("attachments"),
            )),
        );
        Ok(Self {
            artifact_store: stores.process_env_store(),
            stores: Arc::new(stores),
            backend: "postgres",
        })
    }
}
