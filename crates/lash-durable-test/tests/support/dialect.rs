//! The database a scenario runs over: SQLite in memory, a SQLite file or an
//! isolated PostgreSQL database, each with the store set and the durable
//! store a node runs on.

use std::sync::{Arc, Mutex};

use lash_core_execution::StoreSet;
use lash_durable::DurableStore;
use lash_durable_test::SimClock;
use lash_sansio::sync::MutexExt as _;

/// Where a scenario's database lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    SqliteMemory,
    SqliteFile,
    Postgres,
}

/// The PostgreSQL server a PostgreSQL leg runs on, or `None` when the run
/// was handed none and the leg is skipped.
pub fn postgres_url() -> Option<String> {
    std::env::var("LASH_POSTGRES_DATABASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
}

/// A fresh isolated PostgreSQL database, provisioned from the schema.
///
/// It is made on a thread and runtime of its own: its future is not `Send`
/// for every lifetime, as a scenario's must be, and the simulation runs on
/// one current-thread runtime whose quiescence it alone observes.
fn isolated_database(url: String) -> lash_postgres_store::testing::IsolatedDatabase {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a setup runtime")
            .block_on(lash_postgres_store::testing::IsolatedDatabase::create(&url))
    })
    .join()
    .expect("the isolated database is created")
}

/// The store set and the durable store on `clock` over a fresh database of
/// `dialect`. What must outlive the scenario's database (a temporary
/// directory, an isolated PostgreSQL database) is pushed onto `keep`.
pub async fn open(
    dialect: Dialect,
    postgres_url: Option<&str>,
    clock: Arc<SimClock>,
    keep: &Mutex<Vec<Box<dyn std::any::Any + Send>>>,
) -> (Arc<dyn StoreSet>, Arc<dyn DurableStore>) {
    match dialect {
        Dialect::SqliteMemory => {
            let stores = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
                .await
                .expect("an in-memory store set opens");
            let database = Arc::new(stores.durable_store());
            (Arc::new(stores), database)
        }
        Dialect::SqliteFile => {
            let dir = tempfile::tempdir().expect("a temporary directory");
            let stores = lash_sqlite_store::SqliteStoreSet::open_with_clock(
                dir.path().join("lash.db"),
                clock,
            )
            .await
            .expect("a file store set opens");
            keep.lock_recover().push(Box::new(dir));
            let database = Arc::new(stores.durable_store());
            (Arc::new(stores), database)
        }
        Dialect::Postgres => {
            let url = postgres_url.expect("a PostgreSQL URL").to_owned();
            let isolated = isolated_database(url);
            let storage = lash_postgres_store::PostgresStorage::connect(isolated.url())
                .await
                .expect("the isolated database opens");
            // Every port reads the virtual clock, the durable store's
            // included: a host's mail is due when the nodes' clock says.
            let stores = lash_postgres_store::PostgresStoreSet::with_clock_for_testing(
                &storage,
                Arc::new(lash_core_store::attachments::UnavailableAttachmentStore),
                clock,
            );
            let database = lash_core_execution::StoreSet::durable_store(&stores);
            keep.lock_recover().push(Box::new(isolated));
            (Arc::new(stores), database)
        }
    }
}
