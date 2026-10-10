use std::future::Future;
use std::sync::{Arc, Mutex};

use integrator_contract::stores::IntegratorStores;
use lash::persistence::{ConformanceDeployment, ConformanceProcessRegistry};
use lash::runtime::{ActorContext, Clock, SystemClock};
use lash_sqlite_store::SqliteStoreSet;

/// Both views share one substrate: the integrator's production ports and the
/// backend's test-only mutation/inspection hooks required by some laws.
#[derive(Clone)]
pub struct Fixture {
    pub stores: Arc<IntegratorStores>,
    pub deployment: Arc<dyn ConformanceDeployment>,
    pub registry: Arc<dyn ConformanceProcessRegistry>,
    substrate: SqliteStoreSet,
}

impl Fixture {
    pub async fn open(clock: Arc<dyn Clock>) -> Self {
        Self::over(
            SqliteStoreSet::memory_with_clock(clock)
                .await
                .expect("open certification substrate"),
        )
    }

    fn over(substrate: SqliteStoreSet) -> Self {
        Self {
            stores: Arc::new(IntegratorStores::new(Arc::new(substrate.clone()))),
            deployment: substrate.session_store_factory(),
            registry: substrate.process_registry(),
            substrate,
        }
    }

    pub async fn reopen(&self) -> Self {
        Self::over(self.substrate.reopen().await.expect("reopen substrate"))
    }

    pub fn host(&self) -> ActorContext {
        ActorContext::detached(lash_conformance::backend_over(self.stores.clone()))
    }
}

/// Keep every substrate alive until its law finishes, including named SQLite
/// memory anchors. The macros may construct several fresh stores in one law.
#[derive(Clone, Default)]
pub struct Retained(Arc<Mutex<Vec<Fixture>>>);

impl Retained {
    pub fn keep(&self, fixture: &Fixture) {
        self.0
            .lock()
            .expect("fixture retention")
            .push(fixture.clone());
    }

    pub fn fresh(&self) -> Fixture {
        let fixture = sync_await(Fixture::open(Arc::new(SystemClock)));
        self.keep(&fixture);
        fixture
    }
}

/// Registration factories are synchronous and can be called inside Tokio.
/// Open their async stores on a separate thread instead of nesting runtimes.
pub fn sync_await<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> T {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("fixture runtime")
            .block_on(future)
    })
    .join()
    .expect("fixture runtime thread")
}
