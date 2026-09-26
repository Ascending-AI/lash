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
    inner: lash::Backend,
    catalog: Arc<dyn lash::persistence::SessionStoreFactory>,
    trigger_store: Arc<dyn lash::triggers::TriggerStore>,
    effect_host: Arc<dyn lash::durability::EffectHost>,
    process_work: Option<lash::process::ProcessWorkWiring>,
    queued_work: lash::BackendQueuedWork,
}

impl DecoratedBackend {
    pub(crate) fn over(inner: lash::Backend) -> Self {
        Self {
            catalog: inner.session_store_factory(),
            trigger_store: inner.trigger_store(),
            effect_host: inner.effect_host(),
            process_work: inner.process_work(),
            queued_work: inner.queued_work(),
            inner,
        }
    }

    /// Run every effect through `layer` before the backend's host.
    pub(crate) fn with_effect_layer(mut self, layer: Arc<dyn lash::testing::EffectLayer>) -> Self {
        self.effect_host = Arc::new(lash::testing::LayeredEffectHost::new(
            self.effect_host,
            layer,
        ));
        self
    }

    /// Serve sessions from `catalog`, a test's recording or fault-injecting
    /// decorator over (or stand-in for) the backend's own catalog.
    pub(crate) fn with_catalog(
        mut self,
        catalog: Arc<dyn lash::persistence::SessionStoreFactory>,
    ) -> Self {
        self.catalog = catalog;
        self
    }

    /// Keep triggers in `trigger_store`, a test's decorated trigger store.
    pub(crate) fn with_trigger_store(
        mut self,
        trigger_store: Arc<dyn lash::triggers::TriggerStore>,
    ) -> Self {
        self.trigger_store = trigger_store;
        self
    }

    /// Drive queued turns through `driver` instead of the in-process driver.
    pub(crate) fn with_queued_work(
        mut self,
        driver: Arc<dyn lash::runtime::QueuedWorkSubstrate>,
    ) -> Self {
        self.queued_work = lash::BackendQueuedWork::Engine(driver);
        self
    }

    /// Drive processes through `wiring`, whose registry decorates the
    /// backend's own.
    pub(crate) fn with_process_work(mut self, wiring: lash::process::ProcessWorkWiring) -> Self {
        self.process_work = Some(wiring);
        self
    }
}

impl From<DecoratedBackend> for lash::Backend {
    fn from(decorated: DecoratedBackend) -> Self {
        let inner_stores = decorated.inner.stores();
        lash::Backend::new(Arc::new(DecoratedEngine {
            stores: Arc::new(DecoratedStoreSet {
                inner: inner_stores,
                catalog: decorated.catalog,
                trigger_store: decorated.trigger_store,
            }),
            build_generation: decorated.inner.build_generation().clone(),
            effect_host: decorated.effect_host,
            process_work: decorated.process_work,
            queued_work: decorated.queued_work,
        }))
    }
}

struct DecoratedEngine {
    stores: Arc<DecoratedStoreSet>,
    build_generation: lash::BuildGeneration,
    effect_host: Arc<dyn lash::durability::EffectHost>,
    process_work: Option<lash::process::ProcessWorkWiring>,
    queued_work: lash::BackendQueuedWork,
}

impl lash::EffectEngine for DecoratedEngine {
    fn build_generation(&self) -> &lash::BuildGeneration {
        &self.build_generation
    }

    fn stores(&self) -> Arc<dyn lash::durability::StoreSet> {
        Arc::clone(&self.stores) as Arc<dyn lash::durability::StoreSet>
    }

    fn effect_host(&self) -> Arc<dyn lash::durability::EffectHost> {
        Arc::clone(&self.effect_host)
    }

    fn process_work(&self) -> Option<lash::process::ProcessWorkWiring> {
        self.process_work.clone()
    }

    fn queued_work(&self) -> lash::BackendQueuedWork {
        self.queued_work.clone()
    }
}

struct DecoratedStoreSet {
    inner: Arc<dyn lash::durability::StoreSet>,
    catalog: Arc<dyn lash::persistence::SessionStoreFactory>,
    trigger_store: Arc<dyn lash::triggers::TriggerStore>,
}

impl lash::durability::StoreSet for DecoratedStoreSet {
    fn binding_identity(&self) -> &lash::StoreBindingId {
        self.inner.binding_identity()
    }

    fn clock(&self) -> Arc<dyn lash::runtime::Clock> {
        self.inner.clock()
    }

    fn session_store_factory(&self) -> Arc<dyn lash::persistence::SessionStoreFactory> {
        Arc::clone(&self.catalog)
    }

    fn process_registry(&self) -> Arc<dyn lash::process::ProcessRegistry> {
        self.inner.process_registry()
    }

    fn process_continuations(&self) -> Arc<dyn lash::process::ProcessContinuationStore> {
        self.inner.process_continuations()
    }

    fn trigger_store(&self) -> Arc<dyn lash::triggers::TriggerStore> {
        Arc::clone(&self.trigger_store)
    }

    fn process_definition_registry(&self) -> Arc<dyn lash::process::ProcessDefinitionRegistry> {
        self.inner.process_definition_registry()
    }

    fn process_env_store(&self) -> Arc<dyn lash::persistence::ProcessExecutionEnvStore> {
        self.inner.process_env_store()
    }

    fn attachment_store(&self) -> Arc<dyn lash::persistence::AttachmentStore> {
        self.inner.attachment_store()
    }

    fn module_artifacts(&self) -> Arc<dyn lash::persistence::ModuleArtifactStore> {
        self.inner.module_artifacts()
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
