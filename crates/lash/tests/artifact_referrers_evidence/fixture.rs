use std::sync::Arc;

use lash_execution_contracts::backend::StoreSet;
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
    sqlite: Option<Arc<SqliteStoreSet>>,
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
    /// Lease pins left after a settled SQLite turn, before expiry can hide a leak.
    pub fn root_lease_count(&self, session: &str) -> usize {
        let Some(stores) = &self.sqlite else {
            panic!("SQLite witness")
        };
        lash_sqlite_store::testing::root_leases_for_testing(
            stores,
            &lash_durable::ActorKey::session(session).expect("session actor"),
        )
        .expect("count lease pins")
        .len()
    }

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
                sqlite: None,
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
                sqlite: Some(Arc::clone(&stores)),
                backend: lash_conformance::backend_over(stores as Arc<dyn StoreSet>),
                database: Database::Sqlite(uri),
            }
        }
    }

    /// Every Document, Link and package edge, with no referrer filtered out.
    pub async fn edges(&self) -> Vec<Edge> {
        self.kernel_document_edges().await
    }

    /// A retained Link's defining Document has the same exact owner.
    /// Call after executions settle, while the remaining owners are live.
    pub async fn assert_link_owners(&self, edges: &[Edge]) {
        let packages = self.backend.package_store();
        for edge in edges {
            let Some(id) = edge.artifact_ref.strip_prefix("link:v1:") else {
                continue;
            };
            let id = id
                .parse::<lash_kernel_value::LinkId>()
                .expect("canonical Link identity");
            let record = packages
                .link(&id)
                .await
                .expect("read retained Link")
                .expect("the owner retains its Link");
            assert!(
                edges.contains(&Edge {
                    artifact_ref: record.link.document.to_string(),
                    kind: edge.kind.clone(),
                    id: edge.id.clone(),
                }),
                "a Link owner also retains its Document: {edge:?}"
            );
        }
    }

    /// Logical artifact publications, even if their last edge is later reclaimed.
    pub fn record_publications(&self) {
        let Database::Sqlite(uri) = &self.database else {
            panic!("SQLite witness")
        };
        let connection = rusqlite::Connection::open_with_flags(
            uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .expect("write witness");
        connection
            .execute_batch(
                "CREATE TABLE law_publications (namespace TEXT NOT NULL, artifact TEXT NOT NULL);
            CREATE TRIGGER law_publication AFTER INSERT ON artifact_refs
            BEGIN INSERT INTO law_publications VALUES (NEW.namespace, NEW.artifact_ref); END;",
            )
            .expect("publication witness");
    }

    pub fn publications(&self) -> Vec<(String, String)> {
        let Database::Sqlite(uri) = &self.database else {
            panic!("SQLite witness")
        };
        let connection = rusqlite::Connection::open_with_flags(
            uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .expect("read witness");
        let mut query = connection
            .prepare("SELECT namespace, artifact FROM law_publications ORDER BY rowid")
            .expect("witness query");
        query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query publications")
            .map(|row| row.expect("publication"))
            .collect()
    }

    /// Every kernel-document pointer the store keeps, by reference.
    #[expect(
        clippy::expect_used,
        reason = "acceptance fixture reads the durable pointer table"
    )]
    pub async fn kernel_document_pointers(&self) -> Vec<String> {
        const SQLITE_QUERY: &str = "SELECT artifact_ref FROM artifact_refs
            WHERE namespace = 'kernel_document' ORDER BY artifact_ref";
        const POSTGRES_QUERY: &str = "SELECT artifact_ref FROM lash_lash_vm_artifacts
            WHERE namespace = 'kernel_document' ORDER BY artifact_ref";
        match &self.database {
            Database::Sqlite(uri) => {
                let connection = rusqlite::Connection::open_with_flags(
                    uri,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_URI,
                )
                .expect("open SQLite durable core for pointer inspection");
                let mut statement = connection
                    .prepare(SQLITE_QUERY)
                    .expect("prepare pointer read");
                statement
                    .query_map([], |row| row.get(0))
                    .expect("read SQLite pointers")
                    .map(|row| row.expect("decode SQLite pointer"))
                    .collect()
            }
            Database::Postgres { storage, .. } => sqlx::query_scalar::<_, String>(POSTGRES_QUERY)
                .fetch_all(storage.pool())
                .await
                .expect("read PostgreSQL pointers"),
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "acceptance fixture reads the durable edge table"
    )]
    pub async fn kernel_document_edges(&self) -> Vec<Edge> {
        const SQLITE_QUERY: &str = "SELECT artifact_ref, referrer_kind, referrer_id
            FROM artifact_referrer_edges WHERE namespace = 'kernel_document'
            ORDER BY artifact_ref, referrer_kind, referrer_id";
        const POSTGRES_QUERY: &str = "SELECT artifact_ref, referrer_kind, referrer_id
            FROM lash_artifact_referrer_edges WHERE namespace = 'kernel_document'
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
