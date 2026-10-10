//! Store tiers for the park laws. PostgreSQL uses the same isolated database,
//! shared pool and off-clock accounting as the durable crash proof fixtures.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash_core_execution::StoreSet;
use lash_durable::DurableStore;
use lash_durable_test::{OffClockWork, SimClock};
use lash_postgres_store::testing::IsolatedDatabase;

#[derive(Clone, Copy)]
pub enum Tier {
    SqliteMemory,
    Postgres,
}

pub async fn open(
    tier: Tier,
    clock: Arc<SimClock>,
    keep: &Mutex<Option<IsolatedDatabase>>,
) -> (Arc<dyn StoreSet>, Arc<dyn DurableStore>) {
    let attachments = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock.clone())
        .await
        .expect("a SQLite memory store set opens");
    match tier {
        Tier::SqliteMemory => {
            let durable = Arc::new(attachments.durable_store());
            (Arc::new(attachments), durable)
        }
        Tier::Postgres => {
            let url = lash_postgres_store::testing::required_database_url();
            // Setup owns a separate runtime: its future is not Send for every
            // lifetime, and the simulation observes its own runtime's idleness.
            let isolated = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a setup runtime")
                    .block_on(IsolatedDatabase::create(&url))
            })
            .join()
            .expect("an isolated database opens");
            let pool = WaitedOutPool::open(isolated.url());
            clock.wait_out(Arc::clone(&pool) as _);
            let mut config = lash_postgres_store::testing::work_pool_of(16);
            // The matrix's clock owns leases. Server statement guards must
            // not inject an unscripted fault while parallel cells use the pool.
            config.guards.renewal.statement = lash_postgres_store::host::ServerTimeout::Disabled;
            config.guards.scheduler.statement = lash_postgres_store::host::ServerTimeout::Disabled;
            let storage = lash_postgres_store::testing::from_pool(pool.pool.clone(), &config)
                .await
                .expect("the isolated database's storage opens");
            let stores = lash_postgres_store::PostgresStoreSet::with_clock_for_testing(
                &storage,
                attachments.attachment_store(),
                clock,
            );
            let durable = StoreSet::durable_store(&stores);
            *keep.lock().expect("database lifetime") = Some(isolated);
            (Arc::new(stores), durable)
        }
    }
}

/// Network queries are invisible to the simulation runtime. It must wait
/// for busy connections before advancing virtual time and expiring a lease.
struct WaitedOutPool {
    pool: sqlx::PgPool,
    released: Arc<AtomicUsize>,
}

impl WaitedOutPool {
    fn open(url: &str) -> Arc<Self> {
        let released = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&released);
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(16)
            .min_connections(0)
            .after_release(move |_, _| {
                counted.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(true) })
            })
            .connect_lazy(url)
            .expect("the database URL parses");
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
