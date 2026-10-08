use std::sync::Arc;

use lash_core::StoreSet;
use lash_postgres_store::{PostgresStorage, PostgresStoreSet, testing::IsolatedDatabase};
use lash_sqlite_store::{SqliteStoreSet, SqliteStoreSetOptions};

use super::Edge;

#[derive(Clone, Copy)]
pub enum Backend {
    Sqlite,
    Postgres,
}

/// A law's store set, the backend every core of the law runs over, and the
/// database the law reads its edges from.
pub struct Fixture {
    pub backend: lash::Backend,
    database: Database,
}

enum Database {
    Sqlite(String),
    Postgres {
        storage: Box<PostgresStorage>,
        _database: IsolatedDatabase,
        _attachments: tempfile::TempDir,
    },
}

impl Fixture {
    /// Let the core's background work (its artifact cleanup relay) run
    /// before the next read.
    pub async fn settle(&self) {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    #[expect(
        clippy::expect_used,
        reason = "acceptance fixture validates its store setup"
    )]
    pub async fn new(backend: Backend) -> Self {
        if matches!(backend, Backend::Postgres) {
            let url = lash_postgres_store::testing::required_database_url();
            let database = IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::testing::connect(database.url())
                .await
                .expect("open provisioned PostgreSQL storage");
            let attachments = tempfile::tempdir().expect("attachment directory");
            let stores = Arc::new(PostgresStoreSet::new(
                &storage,
                lash_sqlite_store::SqliteStoreSet::open(
                    (attachments.path()).join("attachments.db"),
                    lash_sqlite_store::SqliteSynchronous::Normal,
                )
                .await
                .expect("SQLite attachment store")
                .attachment_store(),
            )) as Arc<dyn StoreSet>;
            Self {
                backend: lash_conformance::backend_over(stores),
                database: Database::Postgres {
                    storage: Box::new(storage),
                    _database: database,
                    _attachments: attachments,
                },
            }
        } else {
            let stores = Arc::new(
                SqliteStoreSet::memory_with_options_and_clock(
                    SqliteStoreSetOptions {
                        process_id_mint: lash_core::ProcessIdMint::sequential_for_testing(),
                        ..SqliteStoreSetOptions::memory()
                    },
                    Arc::new(lash_core::facade_support::SystemClock),
                )
                .await
                .expect("SQLite stores"),
            );
            let uri = stores.database_uri().to_owned();
            Self {
                backend: lash_conformance::backend_over(stores as Arc<dyn StoreSet>),
                database: Database::Sqlite(uri),
            }
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "acceptance fixture reads the durable edge table"
    )]
    pub async fn edges(&self) -> Vec<Edge> {
        const SQLITE_QUERY: &str = "SELECT artifact_ref, referrer_kind, referrer_id
            FROM artifact_referrer_edges WHERE namespace = 'lashlang_module'
            ORDER BY artifact_ref, referrer_kind, referrer_id";
        const POSTGRES_QUERY: &str = "SELECT artifact_ref, referrer_kind, referrer_id
            FROM lash_artifact_referrer_edges WHERE namespace = 'lashlang_module'
            ORDER BY artifact_ref, referrer_kind, referrer_id";
        match &self.database {
            Database::Sqlite(uri) => {
                let connection = rusqlite::Connection::open_with_flags(
                    uri,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_URI,
                )
                .expect("open SQLite durable core for edge inspection");
                let mut statement = connection.prepare(SQLITE_QUERY).expect("prepare edge read");
                statement
                    .query_map([], |row| {
                        Ok(Edge {
                            artifact_ref: row.get(0)?,
                            kind: row.get(1)?,
                            id: row.get(2)?,
                        })
                    })
                    .expect("read SQLite edges")
                    .map(|row| row.expect("decode SQLite edge"))
                    .collect()
            }
            Database::Postgres { storage, .. } => {
                sqlx::query_as::<_, (String, String, String)>(POSTGRES_QUERY)
                    .fetch_all(storage.pool())
                    .await
                    .expect("read PostgreSQL edges")
                    .into_iter()
                    .map(|(artifact_ref, kind, id)| Edge {
                        artifact_ref,
                        kind,
                        id,
                    })
                    .collect()
            }
        }
    }
}
