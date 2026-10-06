use std::sync::Arc;

use lash_core::StoreSet;
use lash_postgres_store::{PostgresStorage, PostgresStoreSet, testing::IsolatedDatabase};
use lash_restate_test::{RestateTestBackend, ServerConfig, backend_with_store_set};
use lash_sqlite_store::{SqliteStoreSet, SqliteStoreSetOptions};

use super::Edge;

#[derive(Clone, Copy)]
pub enum Backend {
    Sqlite,
    Postgres,
}

pub struct Fixture {
    pub double: RestateTestBackend<dyn StoreSet>,
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
    #[expect(
        clippy::expect_used,
        reason = "the evidence fixture runs the real reconcile pass"
    )]
    pub async fn reconcile(&self) {
        if let Some(shifts) = self
            .double
            .restate()
            .session_work_engine()
            .shifts_slot()
            .installed()
        {
            shifts
                .reconcile(
                    &lash_core::engine::ReconcileCursor::default(),
                    std::num::NonZeroUsize::MIN.saturating_add(63),
                )
                .await
                .expect("reconcile the evidence fixture");
        }
        tokio::task::yield_now().await;
    }
    #[expect(
        clippy::expect_used,
        reason = "acceptance fixture validates its store setup"
    )]
    pub async fn new(seed: u64, backend: Backend) -> Self {
        let config = ServerConfig::default();
        if matches!(backend, Backend::Postgres) {
            let url = lash_postgres_store::testing::required_database_url();
            let database = IsolatedDatabase::create(&url).await;
            let storage = PostgresStorage::connect(database.url())
                .await
                .expect("open provisioned PostgreSQL storage");
            let attachments = tempfile::tempdir().expect("attachment directory");
            let double = backend_with_store_set(
                seed,
                config,
                lash_restate_test::DeploymentHooks::default(),
                |clock| async {
                    Ok(Arc::new(PostgresStoreSet::with_clock(
                        &storage,
                        Arc::new(lash::persistence::FileAttachmentStore::new(
                            attachments.path(),
                        )),
                        lash_core::WakeDeliveryConfig::default(),
                        clock,
                    )) as Arc<dyn StoreSet>)
                },
            )
            .await
            .expect("PostgreSQL Restate double");
            Self {
                double,
                database: Database::Postgres {
                    storage: Box::new(storage),
                    _database: database,
                    _attachments: attachments,
                },
            }
        } else {
            let mut uri = None;
            let double = backend_with_store_set(
                seed,
                config,
                lash_restate_test::DeploymentHooks::default(),
                |clock| async {
                    let stores = Arc::new(
                        SqliteStoreSet::memory_with_options_and_clock(
                            SqliteStoreSetOptions {
                                process_id_mint: lash_core::ProcessIdMint::sequential_for_testing(),
                                ..SqliteStoreSetOptions::memory()
                            },
                            clock,
                        )
                        .await
                        .expect("SQLite stores"),
                    );
                    uri = Some(stores.database_uri().to_owned());
                    Ok(stores as Arc<dyn StoreSet>)
                },
            )
            .await
            .expect("SQLite Restate double");
            Self {
                double,
                database: Database::Sqlite(uri.expect("SQLite durable core URI")),
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
