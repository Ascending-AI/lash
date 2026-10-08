//! The database a scenario runs over: SQLite in memory, a SQLite file or an
//! isolated PostgreSQL database, each with the store set and the durable
//! store a node runs on.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash_core_execution::StoreSet;
use lash_durable::DurableStore;
use lash_durable_test::{OffClockWork, SimClock};
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
    // Test code: the PostgreSQL leg reads its server from the environment
    // the target's runner hands it.
    #[allow(clippy::disallowed_methods)]
    std::env::var("LASH_POSTGRES_DATABASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
}

/// A fresh isolated PostgreSQL database, cloned from the process template.
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
/// `dialect`. What must outlive the scenario's store set (a temporary
/// database or attachment directory, an isolated PostgreSQL database) is
/// pushed onto `keep`.
pub async fn open(
    dialect: Dialect,
    postgres_url: Option<&str>,
    clock: Arc<SimClock>,
    keep: &Mutex<Vec<Box<dyn std::any::Any + Send>>>,
) -> (Arc<dyn StoreSet>, Arc<dyn DurableStore>) {
    match dialect {
        Dialect::SqliteMemory => {
            let stores = crate::sim::memory(clock).await;
            let database = Arc::new(stores.durable_store());
            (Arc::new(stores), database)
        }
        Dialect::SqliteFile => {
            let dir = tempfile::tempdir().expect("a temporary directory");
            let stores = crate::sim::file(dir.path().join("lash.db"), clock).await;
            keep.lock_recover().push(Box::new(dir));
            let database = Arc::new(stores.durable_store());
            (Arc::new(stores), database)
        }
        Dialect::Postgres => {
            let url = postgres_url.expect("a PostgreSQL URL").to_owned();
            let setup_started = std::time::Instant::now();
            let isolated = isolated_database(url);
            let setup_ms = setup_started.elapsed().as_secs_f64() * 1000.;
            let pool = WaitedOutPool::open(isolated.url());
            clock.wait_out(Arc::clone(&pool) as _);
            crate::sim::wait_out_renders(&clock);
            let storage =
                lash_postgres_store::testing::from_pool(pool.pool.clone(), &pool_config())
                    .await
                    .expect("the isolated database opens");
            eprintln!(
                "Postgres fixture: setup_ms={setup_ms:.3} verified_open_ms={:.3}",
                setup_started.elapsed().as_secs_f64() * 1000.
            );
            // Every port reads the virtual clock, the durable store's
            // included: a host's mail is due when the nodes' clock says.
            let attachments = tempfile::tempdir().expect("an attachment directory");
            let stores = lash_postgres_store::PostgresStoreSet::with_clock_for_testing(
                &storage,
                Arc::new(lash_core_store::attachments::FileAttachmentStore::new(
                    attachments.path(),
                )),
                clock,
            );
            lash_core_execution::testing::process_execution_env_fixture(
                stores.process_env_store().as_ref(),
            )
            .await;
            let database = lash_core_execution::StoreSet::durable_store(&stores);
            keep.lock_recover().extend([
                Box::new(isolated) as Box<dyn std::any::Any + Send>,
                Box::new(attachments),
            ]);
            (Arc::new(stores), database)
        }
    }
}

/// How many connections the PostgreSQL leg's one pool holds: every role
/// shares it.
const POOL_CONNECTIONS: u32 = 16;

/// The PostgreSQL leg's pool, which every role of the storage shares. Its
/// queries answer over the network, which the simulation's runtime does not
/// see, so the clock waits it out: it is busy while a connection is in use,
/// and each connection's release ends one piece of its work.
struct WaitedOutPool {
    pool: sqlx::PgPool,
    released: Arc<AtomicUsize>,
}

impl WaitedOutPool {
    fn open(url: &str) -> Arc<Self> {
        let released = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&released);
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(POOL_CONNECTIONS)
            .min_connections(0)
            .after_release(move |_, _| {
                counted.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(true) })
            })
            .connect_lazy(url)
            .expect("the isolated database's URL parses");
        Arc::new(Self { pool, released })
    }
}

impl OffClockWork for WaitedOutPool {
    fn busy(&self) -> bool {
        self.pool.size() > self.pool.num_idle() as u32
    }

    fn ended(&self) -> usize {
        self.released.load(Ordering::SeqCst)
    }
}

/// The fixture configuration, its work fitted to the one shared pool.
fn pool_config() -> lash_postgres_store::PostgresHostConfig {
    lash_postgres_store::testing::work_pool_of(POOL_CONNECTIONS)
}
