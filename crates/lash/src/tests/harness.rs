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

/// A fresh SQLite memory backend: the zero-infra substrate every facade
/// test runs on unless it names another (ADR 0102).
pub(crate) async fn memory_backend() -> Arc<lash_sqlite_store::SqliteBackend> {
    Arc::new(
        lash_sqlite_store::SqliteBackend::memory()
            .await
            .expect("open a SQLite memory backend"),
    )
}

/// A fresh SQLite memory backend on `clock`.
pub(crate) async fn memory_backend_with_clock(
    clock: Arc<dyn lash_core::Clock>,
) -> Arc<lash_sqlite_store::SqliteBackend> {
    Arc::new(
        lash_sqlite_store::SqliteBackend::memory_with_clock(clock)
            .await
            .expect("open a SQLite memory backend"),
    )
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

/// A raw read of `backend`'s durable-core catalog: inspection of rows no
/// API reports, taken at a quiescent point of the test.
fn core_rows<T>(
    backend: &lash_sqlite_store::SqliteBackend,
    sql: &str,
    map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Vec<T> {
    let connection = rusqlite::Connection::open(
        backend.database_uri(lash_sqlite_store::SqliteDatabase::DurableCore),
    )
    .expect("open the durable-core catalog");
    let mut statement = connection.prepare(sql).expect("prepare the catalog read");
    statement
        .query_map([], map)
        .expect("read the catalog")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("decode the catalog rows")
}

/// Every queued-run receipt the catalog retains.
pub(crate) fn sqlite_queued_run_count(backend: &lash_sqlite_store::SqliteBackend) -> usize {
    core_rows(backend, "SELECT count(*) FROM queued_runs", |row| {
        row.get::<_, i64>(0)
    })[0] as usize
}

/// Every queued-work batch in enqueue order, with the claim that holds it.
pub(crate) fn sqlite_queued_work_claims(
    backend: &lash_sqlite_store::SqliteBackend,
) -> Vec<(String, Option<String>)> {
    core_rows(
        backend,
        "SELECT batch_id, claim_id FROM queued_work_batches ORDER BY enqueue_seq",
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

/// Every turn input the catalog retains, with its lifecycle state.
pub(crate) fn sqlite_turn_input_states(
    backend: &lash_sqlite_store::SqliteBackend,
) -> Vec<(String, String)> {
    core_rows(
        backend,
        "SELECT input_id, state FROM pending_turn_inputs ORDER BY enqueue_seq",
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
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

    /// Decorate the inner backend's Lashlang artifact store.
    #[cfg(feature = "rlm")]
    pub(crate) fn module_artifacts(
        self,
        decorate: impl FnOnce(
            Arc<dyn lash_core::ModuleArtifactStore>,
        ) -> Arc<dyn lash_core::ModuleArtifactStore>,
    ) -> Self {
        Self {
            layered: self.layered.map_module_artifacts(decorate),
        }
    }

    /// Run the runtime on `clock` while the stores keep the inner
    /// backend's: the two clock domains a PostgreSQL backend has, where
    /// lease timestamps come from the database.
    pub(crate) fn runtime_clock(self, clock: Arc<dyn lash_core::Clock>) -> Self {
        Self {
            layered: self.layered.with_clock(clock),
        }
    }

    pub(crate) fn session_store_factory(
        self,
        decorate: impl FnOnce(
            Arc<dyn lash_core::SessionStoreFactory>,
        ) -> Arc<dyn lash_core::SessionStoreFactory>,
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

    pub(crate) fn process_registry(
        self,
        decorate: impl FnOnce(
            Arc<dyn lash_core::ProcessRegistry>,
        ) -> Arc<dyn lash_core::ProcessRegistry>,
    ) -> Self {
        Self {
            layered: self.layered.map_process_registry(decorate),
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
/// budget, single-row queued-work batching and no queued-work driver.
pub(crate) fn explicit_ephemeral_facets(
    builder: crate::core::LashCoreBuilder,
) -> crate::core::LashCoreBuilder {
    explicit_ephemeral_facets_with_budget(builder, crate::CommitBudget::bounded(1024 * 1024, 512))
}

pub(crate) fn explicit_ephemeral_facets_with_budget(
    builder: crate::core::LashCoreBuilder,
    commit_budget: crate::CommitBudget,
) -> crate::core::LashCoreBuilder {
    backend_work_facets_with_budget(builder, commit_budget).without_queued_work()
}

/// The ephemeral facets with the backend's queued-work driver left running,
/// for tests that drain queued work.
pub(crate) fn explicit_ephemeral_facets_with_backend_work(
    builder: crate::core::LashCoreBuilder,
) -> crate::core::LashCoreBuilder {
    backend_work_facets_with_budget(builder, crate::CommitBudget::bounded(1024 * 1024, 512))
}

/// [`explicit_ephemeral_facets_with_backend_work`] with an explicit budget.
pub(crate) fn backend_work_facets_with_budget(
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
            default_effort: None,
            aliases: Default::default(),
            encoding: lash_core::ReasoningEncoding::Effort,
            disable: None,
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
