use super::*;

/// A trigger store of its own for a test state whose routes never read the
/// core's triggers: the trigger store of a fresh SQLite memory store set.
/// The sessions root of a test data directory, created if absent: the root a
/// test's file backend and any store the test opens beside it share.
pub(crate) fn sessions_root(data_dir: &std::path::Path) -> std::path::PathBuf {
    let root = data_dir.join("lash-sessions");
    std::fs::create_dir_all(&root).expect("create the test sessions root");
    root
}

/// The trigger store of a fresh SQLite memory store set.
pub(crate) fn memory_trigger_store() -> Arc<lash_sqlite_store::SqliteTriggerStore> {
    sync_await(async {
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set")
            .trigger_store()
    })
}

/// A backend with its effect host layered or its process work replaced,
/// every other port its own, for a test that observes the effect boundary or
/// the process-event sink.
pub(crate) struct DecoratedBackend {
    layered: lash::testing::LayeredBackend,
}

impl DecoratedBackend {
    pub(crate) fn over(inner: lash::Backend) -> Self {
        Self {
            layered: lash::testing::LayeredBackend::over(inner),
        }
    }

    /// Run every effect through `layer` before the backend's host.
    pub(crate) fn with_effect_layer(self, layer: Arc<dyn lash::testing::EffectLayer>) -> Self {
        Self {
            layered: self.layered.map_effect_host(|host| {
                Arc::new(lash::testing::LayeredEffectHost::new(host, layer))
            }),
        }
    }

    /// Serve sessions from `catalog`, a test's recording or fault-injecting
    /// decorator over (or stand-in for) the backend's own catalog.
    pub(crate) fn with_catalog(
        self,
        catalog: Arc<dyn lash::persistence::SessionStoreFactory>,
    ) -> Self {
        Self {
            layered: self.layered.map_session_store_factory(|_| catalog),
        }
    }

    /// Keep triggers in `trigger_store`, a test's decorated trigger store.
    pub(crate) fn with_trigger_store(
        self,
        trigger_store: Arc<dyn lash::triggers::TriggerStore>,
    ) -> Self {
        Self {
            layered: self.layered.map_trigger_store(|_| trigger_store),
        }
    }

    /// Drive queued turns through `driver` instead of the in-process driver.
    pub(crate) fn with_queued_work(
        self,
        driver: Arc<dyn lash::runtime::QueuedWorkSubstrate>,
    ) -> Self {
        Self {
            layered: self
                .layered
                .with_queued_work(lash::BackendQueuedWork::Engine(driver)),
        }
    }

    /// Drive processes through `wiring`, built over the backend's (possibly
    /// decorated) registry so the two stay one registry.
    pub(crate) fn with_process_work(self, wiring: lash::process::ProcessWorkWiring) -> Self {
        Self {
            layered: self.layered.wire_process_work(|_| wiring),
        }
    }
}

impl From<DecoratedBackend> for lash::Backend {
    fn from(decorated: DecoratedBackend) -> Self {
        decorated.layered.into_backend()
    }
}

/// A process registry of its own under `data_dir`'s sessions root, on
/// `clock` and with `wake_delivery`, for a test that drives registrations and
/// wake deliveries directly rather than through the core.
pub(crate) async fn standalone_process_registry(
    data_dir: &std::path::Path,
    clock: Arc<dyn lash::runtime::Clock>,
    wake_delivery: Option<lash::process::WakeDeliveryConfig>,
) -> Arc<dyn lash::process::ProcessRegistry> {
    let sessions = data_dir.join("lash-sessions");
    std::fs::create_dir_all(&sessions).expect("create the sessions root");
    let registry = lash_sqlite_store::SqliteProcessRegistry::open_with_clock(
        &sessions.join(format!("standalone-registry-{}.db", uuid::Uuid::new_v4())),
        clock,
        sessions.clone(),
    )
    .await
    .expect("open a standalone process registry");
    Arc::new(match wake_delivery {
        Some(config) => registry.with_wake_delivery_config(config),
        None => registry,
    })
}

/// The effect host of a fresh SQLite memory backend.
pub(crate) fn memory_effect_host() -> Arc<dyn lash::durability::EffectHost> {
    sync_await(async {
        lash_sqlite_store::SqliteBackend::memory()
            .await
            .expect("open a SQLite memory backend")
            .effect_host() as Arc<dyn lash::durability::EffectHost>
    })
}

/// The session catalog of a fresh SQLite memory store set.
pub(crate) fn memory_session_store_factory() -> Arc<lash_sqlite_store::SqliteSessionStoreFactory> {
    sync_await(async {
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set")
            .session_store_factory()
    })
}

pub(crate) fn detached_trigger_store() -> Arc<dyn lash::triggers::TriggerStore> {
    sync_await(async {
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set")
            .trigger_store() as Arc<dyn lash::triggers::TriggerStore>
    })
}

pub(crate) fn run_async_test_on_stack_budget<F, Fut>(name: &str, test: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(STACK_BUDGET_BYTES)
        .spawn(|| {
            let test = Box::pin(test());
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime")
                .block_on(test)
        })
        .expect("spawn stack-budget test thread")
        .join()
        .expect("stack-budget test thread");
}

pub(crate) fn run_async_test_on_stack_budget_multi_thread<F, Fut>(
    name: &str,
    worker_threads: usize,
    test: F,
) where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(STACK_BUDGET_BYTES)
        .spawn(move || {
            let test = Box::pin(test());
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(worker_threads)
                .thread_stack_size(STACK_BUDGET_BYTES)
                .enable_all()
                .build()
                .expect("tokio runtime")
                .block_on(test)
        })
        .expect("spawn stack-budget multi-thread test thread")
        .join()
        .expect("stack-budget multi-thread test thread");
}
