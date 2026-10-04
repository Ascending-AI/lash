//! Store tiers and drain probes shared by the Restate test harness.

// Test harness code: ambient env access is sanctioned here.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

/// The store tier a harness's endpoint and a law's runtime run over.
#[derive(Clone, Copy, Debug)]
pub(super) enum HarnessStoreTier {
    SqliteMemory,
    SqliteFile,
    /// An isolated database under `LASH_POSTGRES_DATABASE_URL`.
    Postgres,
}

/// What keeps a file or PostgreSQL tier's substrate alive for the
/// harness's lifetime.
pub(super) struct HarnessTierResources {
    _directory: tempfile::TempDir,
    _database: Option<lash_postgres_store::testing::IsolatedDatabase>,
}

impl HarnessStoreTier {
    pub(super) async fn open(
        self,
    ) -> (
        Arc<dyn lash_core::StoreSet>,
        Option<lash_sqlite_store::SqliteStoreSet>,
        Option<HarnessTierResources>,
    ) {
        match self {
            Self::SqliteMemory => {
                let stores = lash_sqlite_store::SqliteStoreSet::memory()
                    .await
                    .expect("open the endpoint's SQLite memory store set");
                (Arc::new(stores.clone()), Some(stores), None)
            }
            Self::SqliteFile => {
                let directory = tempfile::tempdir().expect("the SQLite file tier's directory");
                let stores =
                    lash_sqlite_store::SqliteStoreSet::open(directory.path().join("sqlite"))
                        .await
                        .expect("open the endpoint's SQLite file store set");
                (
                    Arc::new(stores.clone()),
                    Some(stores),
                    Some(HarnessTierResources {
                        _directory: directory,
                        _database: None,
                    }),
                )
            }
            Self::Postgres => {
                let url = std::env::var("LASH_POSTGRES_DATABASE_URL")
                    .expect("the PostgreSQL tier needs LASH_POSTGRES_DATABASE_URL");
                let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
                let storage = lash_postgres_store::PostgresStorage::connect(database.url())
                    .await
                    .expect("connect the PostgreSQL tier");
                let directory = tempfile::tempdir().expect("the PostgreSQL tier's attachments");
                let stores = lash_postgres_store::PostgresStoreSet::new(
                    &storage,
                    Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                        directory.path(),
                    )),
                );
                (
                    Arc::new(stores),
                    None,
                    Some(HarnessTierResources {
                        _directory: directory,
                        _database: Some(database),
                    }),
                )
            }
        }
    }
}
