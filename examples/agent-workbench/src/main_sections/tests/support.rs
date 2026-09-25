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

/// The Restate double a workbench test runs on (FIG-3600 S5c): lash-restate's
/// engine and services over a fresh SQLite memory store set, connected to an
/// in-process server double. The twin of `test_file_backend`.
///
/// Keep the returned double alive to the end of the test (FIG-3723); hand
/// `double.lash_backend()` to the core.
#[allow(
    dead_code,
    reason = "a PREP-F twin the S5c batches move their fixtures onto"
)]
pub(crate) async fn test_double_backend(seed: u64) -> lash_restate_test::RestateTestBackend {
    lash_restate_test::backend(seed, lash_restate_test::ServerConfig::default())
        .await
        .expect("build the Restate double")
}

/// A backend with its effect host layered or its process work replaced,
/// every other port its own, for a test that observes the effect boundary or
/// the process-event sink.
pub(crate) struct DecoratedBackend {
    inner: Arc<dyn lash::Backend>,
    catalog: Arc<dyn lash::persistence::SessionStoreFactory>,
    trigger_store: Arc<dyn lash::triggers::TriggerStore>,
    effect_host: Arc<dyn lash::durability::EffectHost>,
    process_work: Option<lash::process::ProcessWorkWiring>,
    session_work: Option<Arc<dyn lash::runtime::SessionWorkEngine>>,
}

impl DecoratedBackend {
    pub(crate) fn over(inner: Arc<dyn lash::Backend>) -> Self {
        Self {
            catalog: inner.session_store_factory(),
            trigger_store: inner.trigger_store(),
            effect_host: inner.effect_host(),
            process_work: inner.process_work(),
            session_work: inner.session_work(),
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
        driver: Arc<dyn lash::runtime::SessionWorkEngine>,
    ) -> Self {
        self.session_work = Some(driver);
        self
    }

    /// Drive processes through `wiring`, whose registry decorates the
    /// backend's own.
    pub(crate) fn with_process_work(mut self, wiring: lash::process::ProcessWorkWiring) -> Self {
        self.process_work = Some(wiring);
        self
    }
}

impl lash::Backend for DecoratedBackend {
    fn binding_identity(&self) -> &str {
        self.inner.binding_identity()
    }

    fn clock(&self) -> Arc<dyn lash::runtime::Clock> {
        self.inner.clock()
    }

    fn session_store_factory(&self) -> Arc<dyn lash::persistence::SessionStoreFactory> {
        Arc::clone(&self.catalog)
    }

    fn effect_host(&self) -> Arc<dyn lash::durability::EffectHost> {
        Arc::clone(&self.effect_host)
    }

    fn process_registry(&self) -> Arc<dyn lash::process::ProcessRegistry> {
        match &self.process_work {
            Some(wiring) => Arc::clone(wiring.registry()),
            None => self.inner.process_registry(),
        }
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

    fn process_work(&self) -> Option<lash::process::ProcessWorkWiring> {
        self.process_work.clone()
    }

    fn session_work(&self) -> Option<Arc<dyn lash::runtime::SessionWorkEngine>> {
        self.session_work.clone()
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
