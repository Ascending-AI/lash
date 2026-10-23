use super::*;

/// The SQL store set the workbench runs its durable backend over: SQLite
/// under the data directory, or PostgreSQL when a database URL is configured.
pub(crate) struct WorkbenchStores {
    pub(crate) stores: Arc<dyn lash::StoreSet>,
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
        let stores = lash::sqlite::SqliteStoreSet::open(
            data_dir.join("lash-sessions.db"),
            lash::sqlite::SqliteSynchronous::Normal,
        )
        .await
        .context("open the SQLite store set")?;
        Ok(Self {
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
        let endpoints = lash::postgres::PostgresEndpoints::from_url(database_url)
            .context("open Postgres workbench storage")?;
        let storage = lash::postgres::PostgresStorage::connect(
            &endpoints,
            &lash::postgres::PostgresHostConfig::default(),
            Default::default(),
        )
        .await
        .context("open Postgres workbench storage")?;
        let stores = lash::postgres::PostgresStoreSet::new(
            &storage,
            lash::sqlite::SqliteStoreSet::open(
                (data_dir.join("attachments")).join("attachments.db"),
                lash::sqlite::SqliteSynchronous::Normal,
            )
            .await
            .context("open SQLite attachment storage")?
            .attachment_store(),
        );
        Ok(Self {
            stores: Arc::new(stores),
            backend: "postgres",
        })
    }
}
