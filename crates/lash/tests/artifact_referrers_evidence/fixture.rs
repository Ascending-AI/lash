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

    /// Every edge to a kernel document. A document is read with the
    /// declaration of each helper it lists, `function:<id>` (FIG-6379),
    /// held beside it under the same referrer or by the frame whose cells
    /// listed it, which retains it across those cells (FIG-6481): those
    /// edges are checked here, so a declaration held by any other referrer
    /// that holds no document fails every law that reads edges, and are left
    /// out of the answer.
    pub async fn edges(&self) -> Vec<Edge> {
        let (declarations, documents): (Vec<Edge>, Vec<Edge>) = self
            .kernel_document_edges()
            .await
            .into_iter()
            .partition(|edge| edge.artifact_ref.starts_with("function:"));
        let holders: std::collections::BTreeSet<(&str, &str)> = documents
            .iter()
            .map(|edge| (edge.kind.as_str(), edge.id.as_str()))
            .collect();
        let stray: Vec<&Edge> = declarations
            .iter()
            .filter(|edge| {
                edge.kind != "frame_environment"
                    && !holders.contains(&(edge.kind.as_str(), edge.id.as_str()))
            })
            .collect();
        assert!(
            stray.is_empty(),
            "declarations held by a referrer that is no frame and holds no document: {stray:#?}"
        );
        documents
    }

    /// SQLite only: from here on, record every insert and delete of a
    /// declaration's pointer or edge, whichever connection writes it. A
    /// declaration's blob is stored with its pointer and peeled with it.
    #[expect(
        clippy::expect_used,
        reason = "acceptance fixture installs its write witness"
    )]
    pub fn record_declaration_writes(&self) {
        let Database::Sqlite(uri) = &self.database else {
            panic!("the declaration write witness reads SQLite");
        };
        let connection = rusqlite::Connection::open_with_flags(
            uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .expect("open SQLite durable core for the write witness");
        let mut batch = String::from("CREATE TABLE law_declaration_writes (write TEXT NOT NULL);");
        for (table, event, row) in [
            ("artifact_refs", "INSERT", "NEW"),
            ("artifact_refs", "DELETE", "OLD"),
            ("artifact_referrer_edges", "INSERT", "NEW"),
            ("artifact_referrer_edges", "DELETE", "OLD"),
        ] {
            batch.push_str(&format!(
                "CREATE TRIGGER law_{table}_{event} AFTER {event} ON {table}
                 WHEN {row}.namespace = 'kernel_document' AND {row}.artifact_ref LIKE 'function:%'
                 BEGIN INSERT INTO law_declaration_writes
                     VALUES ('{table} {event} ' || {row}.artifact_ref); END;"
            ));
        }
        connection
            .execute_batch(&batch)
            .expect("install the declaration write witness");
    }

    /// Every declaration write recorded since
    /// [`Self::record_declaration_writes`], in order.
    #[expect(
        clippy::expect_used,
        reason = "acceptance fixture reads its write witness"
    )]
    pub fn declaration_writes(&self) -> Vec<String> {
        let Database::Sqlite(uri) = &self.database else {
            panic!("the declaration write witness reads SQLite");
        };
        let connection = rusqlite::Connection::open_with_flags(
            uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .expect("open SQLite durable core for the write witness");
        let mut statement = connection
            .prepare("SELECT write FROM law_declaration_writes ORDER BY rowid")
            .expect("prepare the witness read");
        statement
            .query_map([], |row| row.get(0))
            .expect("read the write witness")
            .map(|row| row.expect("decode a witnessed write"))
            .collect()
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
