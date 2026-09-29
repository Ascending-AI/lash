use super::*;
use std::future::Future;

pub(super) const STACK_BUDGET_BYTES: usize = 2 * 1024 * 1024;

pub(crate) fn model_spec(
    model: impl Into<String>,
    variant: Option<String>,
    context_window_tokens: usize,
) -> lash_core::ModelSpec {
    let capability = capability_for_variant(variant.as_deref());
    lash_core::ModelSpec::builder(model)
        .variant(
            variant
                .map(lash_core::ReasoningSelection::Effort)
                .unwrap_or_default(),
        )
        .context_window_tokens(context_window_tokens)
        .build()
        .expect("valid model spec")
        .with_capability(capability)
}

pub(crate) fn mock_model_spec() -> lash_core::ModelSpec {
    model_spec("mock-model", None, 200_000)
}

std::thread_local! {
    /// The Restate doubles the running test built through [`double_backend`].
    /// A core over `double.lash_backend()` does not hold its double
    /// (FIG-3723), and each test runs on its own thread, so this holds every
    /// double exactly as long as the test that built it.
    static TEST_DOUBLES: std::cell::RefCell<Vec<lash_restate_test::RestateTestBackend>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The seed of the doubles [`double_backend`] builds.
const DOUBLE_SEED: u64 = 0x1a5b_d0b1;

/// The backend every facade test runs on unless it names another: Restate's
/// engine on a fresh server double, held for the rest of the running test.
/// A test that reaches the double itself (a drive hold, its store set, its
/// clock) builds one with [`restate_double`] and keeps it.
pub(crate) async fn double_backend() -> lash_core::Backend {
    let double = restate_double(DOUBLE_SEED).await;
    let backend = double.lash_backend();
    TEST_DOUBLES.with(|held| held.borrow_mut().push(double));
    backend
}

/// The backend over `double` whose installed driver starts no wall-clock
/// reconcile tick: every obligation delivery is one the test made — a
/// verb's immediate attempt or a pass it drives — so no scheduled pass can
/// claim an obligation out from under the assertion being made (FIG-3926).
fn explicit_reconcile(double: &lash_restate_test::RestateTestBackend) -> lash_core::Backend {
    lash_core::testing::runtime_helpers::LayeredBackend::over(double.lash_backend())
        .with_session_work(double.explicit_reconcile_session_work())
        .into_backend()
}

/// [`double_backend`] over the explicit-reconcile session work
/// [`explicit_reconcile`] gives.
pub(crate) async fn double_backend_explicit_reconcile() -> lash_core::Backend {
    let double = restate_double(DOUBLE_SEED).await;
    let backend = explicit_reconcile(&double);
    TEST_DOUBLES.with(|held| held.borrow_mut().push(double));
    backend
}

/// [`double_backend`] over a decorated store set: `decorate` wraps the
/// double's stores before its engine is built, so the engine and every
/// service it binds run over the decoration (a layer added to the backend
/// afterwards would not reach the engine's own processes).
pub(crate) async fn double_backend_over(
    config: lash_restate_test::ServerConfig,
    decorate: impl FnOnce(Arc<dyn lash_core::StoreSet>) -> Arc<dyn lash_core::StoreSet>,
) -> lash_core::Backend {
    let double = lash_restate_test::backend_with(DOUBLE_SEED, config, decorate)
        .await
        .expect("build the Restate double over decorated stores");
    let backend = double.lash_backend();
    TEST_DOUBLES.with(|held| held.borrow_mut().push(double));
    backend
}

/// [`double_backend_over`] over the explicit-reconcile session work
/// [`explicit_reconcile`] gives.
pub(crate) async fn double_backend_over_explicit_reconcile(
    config: lash_restate_test::ServerConfig,
    decorate: impl FnOnce(Arc<dyn lash_core::StoreSet>) -> Arc<dyn lash_core::StoreSet>,
) -> lash_core::Backend {
    let double = lash_restate_test::backend_with(DOUBLE_SEED, config, decorate)
        .await
        .expect("build the Restate double over decorated stores");
    let backend = explicit_reconcile(&double);
    TEST_DOUBLES.with(|held| held.borrow_mut().push(double));
    backend
}

/// Wall time on `core`'s clock: its held double's virtual clock, or the
/// system clock for a core no held double serves. A relay pass a test times
/// by hand reads the same clock the stores scheduled against.
pub(crate) fn core_now_ms(core: &crate::LashCore) -> u64 {
    match held_double(core) {
        Some(double) => lash_core::ClockWallTime::timestamp_ms(double.test_clock().as_ref()),
        None => lash_core::ClockWallTime::timestamp_ms(&lash_core::facade_support::SystemClock),
    }
}

/// The double [`double_backend`] built last on this test's thread.
pub(crate) fn latest_double() -> Option<lash_restate_test::RestateTestBackend> {
    TEST_DOUBLES.with(|held| held.borrow().last().cloned())
}

/// The held double `core` runs over, if [`double_backend`] built it.
pub(crate) fn held_double(core: &crate::LashCore) -> Option<lash_restate_test::RestateTestBackend> {
    let binding = core.backend.binding_identity();
    TEST_DOUBLES.with(|held| {
        held.borrow()
            .iter()
            .rev()
            .find(|double| double.lash_backend().binding_identity() == binding)
            .cloned()
    })
}

/// Wait until the held double `core` runs over has no drive of `session` in
/// flight ([`settle_session_drive`](lash_restate_test::RestateTestBackend::settle_session_drive)):
/// a handle answers before its root's scope closes (FIG-3979).
pub(crate) async fn settle_session_drive(core: &crate::LashCore, session: &str) {
    held_double(core)
        .expect("the core runs on a held double")
        .settle_session_drive(&lash_core::SessionId::from(session))
        .await;
}

/// Serve process segments on the held double `core` runs over, with `core`'s
/// own worker: the double's process workflow runs a segment only once a
/// worker is installed, as a deployment's endpoint does. A core over a
/// backend no held double serves is left alone.
pub(crate) fn serve_processes(core: &crate::LashCore) {
    if let Some(double) = held_double(core) {
        serve_processes_on(&double, core);
    }
}

/// Serve process segments on `double` with `core`'s own worker.
pub(crate) fn serve_processes_on(
    double: &lash_restate_test::RestateTestBackend,
    core: &crate::LashCore,
) {
    let worker = lash_core_worker::DurableProcessWorker::new(
        core.durable_process_worker_config()
            .expect("the core's process-worker config"),
    )
    .expect("the core's process worker");
    double.install_process_worker(worker);
}

/// The Restate double a facade test runs on (FIG-3600 S5c): lash-restate's
/// engine and services over a fresh SQLite memory store set, connected to an
/// in-process server double.
///
/// `ServerConfig::default()` schedules concurrently, so no outside gates are
/// needed. Keep the returned double alive to the end of the test (FIG-3723):
/// a core built over `double.lash_backend()` does not hold it.
pub(crate) async fn restate_double(seed: u64) -> lash_restate_test::RestateTestBackend {
    lash_restate_test::backend(seed, lash_restate_test::ServerConfig::default())
        .await
        .expect("build the Restate double")
}

/// A new deployment over `double`'s stores after `double` is gone: a restart
/// whose engine runs the driver of the first core built over it. One engine
/// serves one core's driver, so a law about another build's drive redeploys
/// rather than building a second core over the same engine.
pub(crate) async fn redeploy(
    double: lash_restate_test::RestateTestBackend,
    seed: u64,
) -> lash_restate_test::RestateTestBackend {
    let stores = Arc::clone(double.engine_stores());
    drop(double);
    lash_restate_test::backend_with(
        seed,
        lash_restate_test::ServerConfig::default(),
        move |_| stores,
    )
    .await
    .expect("redeploy the Restate double over the same stores")
}

/// Under the Restate double a session's writer claim frees when the engine
/// lane's last turn settles, and a host admit (`open`, `durable`, `create`)
/// can race that release: retry `Contended` until a bounded deadline. The
/// same release race race_recovery's open loop already tolerates.
pub(crate) async fn retry_when_claim_frees<T, F, Fut>(mut attempt: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        match attempt().await {
            Err(error)
                if format!("{error:?}").contains("Contended")
                    && std::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            outcome => return outcome,
        }
    }
}

/// A fresh SQLite memory store set: storage ports only, no engine. For a
/// test whose every use is a store port.
#[allow(
    dead_code,
    reason = "a PREP-F twin the S5c batches move their fixtures onto"
)]
pub(crate) async fn memory_store_set() -> Arc<lash_sqlite_store::SqliteStoreSet> {
    Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    )
}

/// A backend over a fresh SQLite memory store set whose effect host only
/// records: for a test that needs a backend value but runs no effect.
#[allow(
    dead_code,
    reason = "a PREP-F twin the S5c batches move their fixtures onto"
)]
pub(crate) async fn memory_store_backend() -> lash_core::Backend {
    let stores = memory_store_set().await;
    lash_conformance::recording_backend_over(stores)
}

/// A backend over a fresh SQLite memory store set stamping from `clock`,
/// whose effect host only records and which drives no session work: for a
/// test that reads what the stores stamp.
pub(crate) async fn store_backend_with_clock(
    clock: Arc<dyn lash_core::Clock>,
) -> lash_core::Backend {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
            .await
            .expect("open a SQLite memory store set"),
    );
    lash_conformance::recording_backend_over(stores)
}

/// Every turn input `double`'s durable-core catalog retains, with its
/// lifecycle state: inspection of rows no API reports, taken at a quiescent
/// point of the test.
pub(crate) fn turn_input_states(
    double: &lash_restate_test::RestateTestBackend,
) -> Vec<(String, String)> {
    let connection = rusqlite::Connection::open(
        double
            .stores()
            .database_uri(lash_sqlite_store::SqliteDatabase::DurableCore),
    )
    .expect("open the durable-core catalog");
    let mut statement = connection
        .prepare("SELECT input_id, state FROM pending_turn_inputs ORDER BY enqueue_seq")
        .expect("prepare the catalog read");
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("read the catalog")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("decode the catalog rows")
}

/// One backend with some of its ports decorated by a test that observes
/// or faults them. Every port a test does not decorate is the inner
/// backend's, and every decoration is handed the inner port it wraps, so
/// the decorated backend is still one substrate.
#[derive(Clone)]
pub(crate) struct DecoratedBackend {
    layered: lash_core::testing::runtime_helpers::LayeredBackend,
}

impl DecoratedBackend {
    pub(crate) fn over(inner: lash_core::Backend) -> Self {
        Self {
            layered: lash_core::testing::runtime_helpers::LayeredBackend::over(inner),
        }
    }

    pub(crate) fn session_store_factory(
        self,
        decorate: impl FnOnce(
            Arc<dyn lash_core::DeploymentStore>,
        ) -> Arc<dyn lash_core::DeploymentStore>,
    ) -> Self {
        Self {
            layered: self.layered.map_session_store_factory(decorate),
        }
    }

    pub(crate) fn effect_host(
        self,
        decorate: impl FnOnce(Arc<dyn lash_core::EffectHost>) -> Arc<dyn lash_core::EffectHost>,
    ) -> Self {
        Self {
            layered: self.layered.map_effect_host(decorate),
        }
    }

    pub(crate) fn process_env_store(
        self,
        decorate: impl FnOnce(
            Arc<dyn lash_core::ProcessExecutionEnvStore>,
        ) -> Arc<dyn lash_core::ProcessExecutionEnvStore>,
    ) -> Self {
        Self {
            layered: self.layered.map_process_env_store(decorate),
        }
    }

    /// Drive this backend's processes through `wire`, which receives the
    /// (possibly decorated) registry the wiring must be built over.
    pub(crate) fn process_work(
        self,
        wire: impl FnOnce(Arc<dyn lash_core::ProcessRegistry>) -> lash_core::ProcessWorkWiring,
    ) -> Self {
        Self {
            layered: self.layered.wire_process_work(wire),
        }
    }

    /// The decorated backend.
    pub(crate) fn into_backend(self) -> lash_core::Backend {
        self.layered.into_backend()
    }
}

impl From<DecoratedBackend> for lash_core::Backend {
    fn from(decorated: DecoratedBackend) -> Self {
        decorated.into_backend()
    }
}

/// The runtime settings every facade test core names: a generous commit
/// budget and single-row queued-work batching.
pub(crate) fn explicit_ephemeral_facets(
    builder: crate::core::LashCoreBuilder,
) -> crate::core::LashCoreBuilder {
    explicit_ephemeral_facets_with_budget(builder, crate::CommitBudget::bounded(1024 * 1024, 512))
}

/// [`explicit_ephemeral_facets`] with an explicit commit budget.
pub(crate) fn explicit_ephemeral_facets_with_budget(
    builder: crate::core::LashCoreBuilder,
    commit_budget: crate::CommitBudget,
) -> crate::core::LashCoreBuilder {
    builder
        .commit_budget(commit_budget)
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
}

fn capability_for_variant(variant: Option<&str>) -> lash_core::ModelCapability {
    let Some(variant) = variant else {
        return lash_core::ModelCapability::default();
    };
    lash_core::ModelCapability {
        instruction_role: Default::default(),
        native_mid_conversation_system: false,
        attachment_acceptance: Default::default(),
        google_dialect: Default::default(),
        reasoning: Some(lash_core::ReasoningCapability {
            efforts: vec![variant.to_string()],
            encoding: lash_core::ReasoningEncoding::Effort,
            disable: false,
            mandatory: false,
        }),
        cache_control: None,
        stream_termination: None,
        sampling: lash_core::SamplingCapability::Configurable,
        reasoning_retention: Default::default(),
    }
}

pub(crate) fn run_async_test_on_stack_budget<F, Fut, T>(name: &str, test: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
{
    run_async_test_on_stack_size(name, STACK_BUDGET_BYTES, test)
}

pub(crate) fn run_async_test_on_stack_size<F, Fut, T>(name: &str, stack_size: usize, test: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(stack_size)
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
        .expect("stack-budget test thread")
}

/// `send`'s settled report streamed into `sink`, the way a host with a
/// cancel token of its own waits: when `cancel` fires, the input is
/// cancelled once, recording `origin`, and the wait goes on to the answer
/// the cancel produced.
pub(crate) async fn output_into_cancelled_by(
    send: crate::SendBuilder,
    sink: &dyn TurnActivitySink,
    cancel: CancellationToken,
    origin: Option<String>,
) -> Result<TurnReport> {
    let handle = send.await?;
    let canceller = handle.cancel();
    let settle = handle.output_into(sink);
    tokio::pin!(settle);
    tokio::select! {
        report = &mut settle => return report,
        () = cancel.cancelled() => {}
    }
    let canceller = match origin {
        Some(origin) => canceller.origin(origin),
        None => canceller,
    };
    canceller.await?;
    settle.await
}

/// The durable acceptance receipt of a send: what a law reads when it
/// asserts on the pending row a send accepted before anything drives it.
pub(crate) trait AcceptedSend {
    async fn accepted(self) -> Result<lash_core::runtime::TurnInputAcceptanceReceipt>;
}

impl AcceptedSend for crate::SendBuilder {
    async fn accepted(self) -> Result<lash_core::runtime::TurnInputAcceptanceReceipt> {
        Ok(self.await?.receipt().clone())
    }
}
