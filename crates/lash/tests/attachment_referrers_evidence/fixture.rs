use std::sync::Arc;

use lash_core::StoreSet;
use lash_postgres_store::{PostgresStorage, PostgresStoreSet, testing::IsolatedDatabase};
use lash_restate_test::{RestateTestBackend, ServerConfig, backend_with_store_set};
use lash_sqlite_store::{SqliteDatabase, SqliteStoreSet, SqliteStoreSetOptions};

/// The dual-backend Restate double (FIG-4066): SQLite always, PostgreSQL when
/// `LASH_POSTGRES_DATABASE_URL` is set, and required under
/// `LASH_REQUIRE_POSTGRES=1`.
pub struct Fixture {
    pub double: RestateTestBackend<dyn StoreSet>,
    database: Database,
}

enum Database {
    Sqlite(String),
    Postgres {
        storage: PostgresStorage,
        _database: IsolatedDatabase,
        _attachments: tempfile::TempDir,
    },
}

impl Fixture {
    #[expect(
        clippy::expect_used,
        reason = "acceptance fixture validates its store setup"
    )]
    pub async fn new(seed: u64) -> Self {
        let config = ServerConfig::default();
        let configured_url = std::env::var("LASH_POSTGRES_DATABASE_URL")
            .ok()
            .filter(|url| !url.trim().is_empty());
        assert!(
            configured_url.is_some()
                || std::env::var("LASH_REQUIRE_POSTGRES").as_deref() != Ok("1"),
            "LASH_POSTGRES_DATABASE_URL must be set and non-empty when LASH_REQUIRE_POSTGRES=1"
        );
        if let Some(url) = configured_url {
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
                    storage,
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
                    uri = Some(stores.database_uri(SqliteDatabase::DurableCore).to_owned());
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

    /// Every session id the catalog has a row for, live or deleted.
    #[expect(
        clippy::expect_used,
        reason = "acceptance fixture reads the durable session tables"
    )]
    pub async fn catalog_session_ids(&self) -> Vec<String> {
        const SQLITE_QUERY: &str = "SELECT session_id FROM session_meta
            UNION SELECT session_id FROM deleted_sessions ORDER BY 1";
        const POSTGRES_QUERY: &str = "SELECT session_id FROM lash_session_meta
            UNION SELECT session_id FROM lash_deleted_sessions ORDER BY 1";
        match &self.database {
            Database::Sqlite(uri) => {
                let connection = rusqlite::Connection::open_with_flags(
                    uri,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_URI,
                )
                .expect("open SQLite durable core for session inspection");
                let mut statement = connection
                    .prepare(SQLITE_QUERY)
                    .expect("prepare session read");
                statement
                    .query_map([], |row| row.get::<_, String>(0))
                    .expect("read SQLite sessions")
                    .map(|row| row.expect("decode SQLite session id"))
                    .collect()
            }
            Database::Postgres { storage, .. } => sqlx::query_scalar::<_, String>(POSTGRES_QUERY)
                .fetch_all(storage.pool())
                .await
                .expect("read PostgreSQL sessions"),
        }
    }
}
