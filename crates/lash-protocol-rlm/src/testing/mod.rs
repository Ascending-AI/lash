mod cell_conformance;
mod kernel_door_tests;

use std::cell::RefCell;
use std::sync::Arc;

/// A fresh storage backend for tests that do not execute durable effects.
pub(crate) async fn memory_backend() -> lash_core::Backend {
    memory_store_backend().await
}

/// A fresh Restate server double under `seed` with `config`: lash-restate's
/// engine over a SQLite memory store set, the twin of [`memory_backend`] for a
/// kernel test whose effects run on an engine. Hold the double to the end of
/// the test and never build a core over the handle itself (FIG-3723); a turn
/// runs on `double.open_handler(scope)`'s scoped controller. Under
/// `Scheduling::Serial`, a scripted provider that waits on the test holds
/// `double.server().outside_gates().enter()` across the wait.
pub(crate) async fn kernel_double(
    seed: u64,
    config: lash_restate_test::ServerConfig,
) -> lash_restate_test::RestateTestBackend {
    lash_restate_test::backend(seed, config)
        .await
        .expect("build the Restate server double")
}

/// A process table the Restate server double serves: the double's registry
/// and execution-env store, which a cell's recorded starts write, and the
/// worker the double's process workflow runs their segments on. The double
/// must outlive every process it admits, so the table owns it.
pub(crate) struct DoubleProcesses {
    double: lash_restate_test::RestateTestBackend,
    backend: lash_core::Backend,
}

impl DoubleProcesses {
    /// A fresh double under `seed`.
    pub(crate) async fn new(seed: u64) -> Self {
        let double = kernel_double(seed, lash_restate_test::ServerConfig::default()).await;
        let backend = double.lash_backend();
        Self { double, backend }
    }

    /// The double itself, for a cell run in one of its handlers.
    pub(crate) fn double(&self) -> &lash_restate_test::RestateTestBackend {
        &self.double
    }

    /// The double's backend: a worker's runtime host is built over it.
    pub(crate) fn backend(&self) -> &lash_core::Backend {
        &self.backend
    }

    /// The registry a cell's recorded starts register on.
    pub(crate) fn registry(&self) -> Arc<dyn lash_core::ProcessRegistry> {
        self.backend.process_registry()
    }

    /// The store a recorded start publishes its execution env to.
    pub(crate) fn env_store(&self) -> Arc<dyn lash_core::ProcessExecutionEnvStore> {
        self.backend.process_env_store()
    }

    /// Serve process segments with a worker over `factories` and
    /// `runtime_host`, which must be built over [`Self::backend`].
    pub(crate) fn install_worker(
        &self,
        factories: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
        runtime_host: lash_core::facade_support::RuntimeHostConfig,
        session_policy: lash_core::SessionPolicy,
    ) {
        let worker = lash_core_worker::DurableProcessWorker::new(
            lash_core_worker::DurableProcessWorkerConfig::new(
                Arc::new(lash_core::facade_support::PluginHost::new(factories)),
                runtime_host,
                self.backend.process_work(),
                Arc::new(lash_core::NoSessionWork::new()),
                lash_core::testing::runtime_lease_owner(),
            )
            .with_session_policy(session_policy),
        )
        .expect("valid double process worker");
        self.double.install_process_worker(worker);
    }

    /// Open a handler on the double for `admitted`: its controller serves
    /// effects the way a deployment's turn handler does.
    pub(crate) async fn open_handler(
        &self,
        admitted: lash_core::AdmittedScope,
    ) -> lash_restate_test::OpenHandler {
        self.double
            .open_handler(admitted)
            .await
            .expect("open a handler on the double")
    }

    /// Deliver the due ProcessStart obligations to the double.
    pub(crate) async fn admit_pending(&self) {
        let relay = lash_core::runtime::process_start::ProcessStartRelay::new(
            self.backend
                .obligation_ledger(lash_core::store::ObligationKind::ProcessStart),
            self.registry(),
            Arc::clone(self.backend.process_work().port()),
            Arc::new(lash_core::facade_support::SystemClock),
        );
        lash_core::runtime::drive::relay::relay_due(
            &relay,
            &lash_core::facade_support::SystemClock,
            std::num::NonZeroUsize::new(1024).expect("nonzero"),
        )
        .await
        .expect("deliver process starts");
    }

    /// Await `process_id`'s terminal registry record.
    pub(crate) async fn await_terminal(
        &self,
        process_id: &lash_core::ProcessId,
    ) -> lash_core::ProcessAwaitOutput {
        lash_core::NoProcessWork::for_registry(self.registry())
            .await_terminal(process_id)
            .await
            .expect("await the process's terminal record")
    }
}

std::thread_local! {
    /// The store sets the running test opened, held as its backends are.
    static TEST_STORE_SETS: std::cell::RefCell<Vec<std::sync::Arc<lash_sqlite_store::SqliteStoreSet>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A fresh SQLite memory store set, storage only (no engine), held for the
/// rest of the running test: the twin of [`memory_backend`] for a test that reaches
/// only store ports.
pub(crate) async fn memory_store_set() -> std::sync::Arc<lash_sqlite_store::SqliteStoreSet> {
    let stores = std::sync::Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    TEST_STORE_SETS.with(|held| held.borrow_mut().push(std::sync::Arc::clone(&stores)));
    stores
}

/// [`memory_store_set`] as a backend whose effect host is the recording
/// double: for a test that needs a `Backend` value but runs no effect.
pub(crate) async fn memory_store_backend() -> lash_core::Backend {
    lash_conformance::recording_backend_over(memory_store_set().await)
}

thread_local! {
    /// One artifact backend per test thread so repeat reads use the same store.
    static ARTIFACT_BACKEND: RefCell<Option<lash_core::Backend>> =
        const { RefCell::new(None) };
}

pub(crate) async fn memory_artifact_store() -> lashlang::LashlangArtifacts {
    let existing = ARTIFACT_BACKEND.with(|held| held.borrow().clone());
    let backend = match existing {
        Some(backend) => backend,
        None => {
            let backend = memory_backend().await;
            ARTIFACT_BACKEND.with(|held| *held.borrow_mut() = Some(backend.clone()));
            backend
        }
    };
    lashlang::LashlangArtifacts::of_backend(&backend)
}

pub(crate) fn memory_backend_blocking() -> lash_core::Backend {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build a current-thread runtime")
                    .block_on(memory_backend())
            })
            .join()
            .expect("open the memory backend on its own thread")
    })
}

pub(crate) fn memory_artifact_store_blocking() -> lashlang::LashlangArtifacts {
    lashlang::LashlangArtifacts::of_backend(&memory_backend_blocking())
}

pub(crate) async fn fresh_memory_artifact_store() -> lashlang::LashlangArtifacts {
    lashlang::LashlangArtifacts::of_backend(&memory_backend().await)
}

/// The scope a context built with no parent invocation claims: the builder's
/// default test turn. Open the handler [`double_ports`] lends for it.
pub(crate) fn default_cell_scope() -> lash_core::AdmittedScope {
    lash_core::AdmittedScope::turn(
        lash_core::SessionId::from("test-session"),
        lash_core::TurnId::from("test-turn"),
    )
}

/// Every port of `double`, with the controller `handler` lends serving the
/// context's effects, as a Restate deployment serves a turn's cell from the
/// turn's handler.
///
/// Open `handler` on `double` for the scope the context claims
/// ([`default_cell_scope`], or the scope of the invocation the context
/// installs), drop the context, then close the handler. The borrow keeps the
/// context from outliving the handler.
pub(crate) fn double_ports<'h>(
    double: &lash_restate_test::RestateTestBackend,
    handler: &'h lash_restate_test::OpenHandler,
) -> lash_core::testing::TestExecutionPorts<'h> {
    lash_core::testing::TestExecutionPorts::lent(&double.lash_backend(), handler.scoped())
}

/// [`double_ports`] with `layer` in front of the double's host and of the
/// controller `handler` lends, for a law that observes or perturbs the effect
/// seam.
pub(crate) fn double_ports_over_layer<'h>(
    double: &lash_restate_test::RestateTestBackend,
    handler: &'h lash_restate_test::OpenHandler,
    layer: Arc<dyn lash_core::testing::EffectLayer>,
) -> lash_core::testing::TestExecutionPorts<'h> {
    let backend = double.lash_backend();
    let lent =
        lash_core::testing::LayeredEffectHost::layer_scoped(handler.scoped(), Arc::clone(&layer))
            .expect("layer the handler's controller");
    lash_core::testing::TestExecutionPorts {
        effect_host: Arc::new(lash_core::testing::LayeredEffectHost::new(
            backend.effect_host(),
            layer,
        )),
        ..lash_core::testing::TestExecutionPorts::lent(&backend, lent)
    }
}

/// [`double_ports`] for a handler attempt the server may re-run
/// (`run_in_handler`, `run_crashed_then_redriven`): every port of `backend`,
/// the double's `lash_backend()`, with `scoped`, the controller the attempt's
/// handler hands it, serving the context's effects.
pub(crate) fn attempt_ports<'a>(
    backend: &lash_core::Backend,
    scoped: lash_core::ScopedEffectController<'a>,
) -> lash_core::testing::TestExecutionPorts<'a> {
    lash_core::testing::TestExecutionPorts::lent(backend, scoped)
}

/// [`attempt_ports`] with `layer` in front of `backend`'s host and of
/// `scoped`, as [`double_ports_over_layer`] puts it.
pub(crate) fn attempt_ports_over_layer<'a>(
    backend: &lash_core::Backend,
    scoped: lash_core::ScopedEffectController<'a>,
    layer: Arc<dyn lash_core::testing::EffectLayer>,
) -> lash_core::testing::TestExecutionPorts<'a> {
    let lent = lash_core::testing::LayeredEffectHost::layer_scoped(scoped, Arc::clone(&layer))
        .expect("layer the attempt's controller");
    lash_core::testing::TestExecutionPorts {
        effect_host: Arc::new(lash_core::testing::LayeredEffectHost::new(
            backend.effect_host(),
            layer,
        )),
        ..lash_core::testing::TestExecutionPorts::lent(backend, lent)
    }
}

/// A fresh memory store set's process registry, for a trigger router whose
/// deliveries no law inspects.
pub(crate) async fn memory_process_registry() -> Arc<dyn lash_core::ProcessRegistry> {
    lash_core::StoreSet::process_registry(memory_store_set().await.as_ref())
}

/// A fresh memory store set's trigger store.
pub(crate) async fn memory_trigger_store() -> Arc<dyn lash_core::TriggerStore> {
    lash_core::StoreSet::trigger_store(memory_store_set().await.as_ref())
}
pub(crate) fn recorded_test_render() -> lash_core::RecordedRender {
    lash_core::RecordedRender {
        renderer_id: crate::render::CodeRendererSlot::default()
            .0
            .id()
            .to_string(),
        params: serde_json::to_value(crate::render::ResolvedRlmRender::default())
            .expect("test render params serialize"),
    }
}
