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
    inner: Arc<dyn lash_core::Backend>,
    clock: Option<Arc<dyn lash_core::Clock>>,
    session_store_factory: Option<Arc<dyn lash_core::SessionStoreFactory>>,
    effect_host: Option<Arc<dyn lash_core::EffectHost>>,
    /// The replacement host's binding, which the decorated backend answers
    /// as its own identity so the two still agree.
    effect_host_binding: Option<String>,
    process_registry: Option<Arc<dyn lash_core::ProcessRegistry>>,
    process_env_store: Option<Arc<dyn lash_core::ProcessExecutionEnvStore>>,
    process_work: Option<lash_core::ProcessWorkWiring>,
    module_artifacts: Option<Arc<dyn lash_core::ModuleArtifactStore>>,
}

impl DecoratedBackend {
    pub(crate) fn over(inner: Arc<dyn lash_core::Backend>) -> Self {
        Self {
            inner,
            clock: None,
            session_store_factory: None,
            effect_host: None,
            effect_host_binding: None,
            process_registry: None,
            process_env_store: None,
            process_work: None,
            module_artifacts: None,
        }
    }

    /// Decorate the inner backend's Lashlang artifact store.
    #[cfg(feature = "rlm")]
    pub(crate) fn module_artifacts(
        mut self,
        decorate: impl FnOnce(
            Arc<dyn lash_core::ModuleArtifactStore>,
        ) -> Arc<dyn lash_core::ModuleArtifactStore>,
    ) -> Self {
        self.module_artifacts = Some(decorate(self.inner.module_artifacts()));
        self
    }

    /// Run the runtime on `clock` while the stores keep the inner
    /// backend's: the two clock domains a PostgreSQL backend has, where
    /// lease timestamps come from the database.
    pub(crate) fn runtime_clock(mut self, clock: Arc<dyn lash_core::Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    pub(crate) fn session_store_factory(
        mut self,
        decorate: impl FnOnce(
            Arc<dyn lash_core::SessionStoreFactory>,
        ) -> Arc<dyn lash_core::SessionStoreFactory>,
    ) -> Self {
        self.session_store_factory = Some(decorate(self.inner.session_store_factory()));
        self
    }

    pub(crate) fn effect_host(
        mut self,
        decorate: impl FnOnce(Arc<dyn lash_core::EffectHost>) -> Arc<dyn lash_core::EffectHost>,
    ) -> Self {
        let host = decorate(self.inner.effect_host());
        self.effect_host_binding = Some(host.turn_control_binding_id());
        self.effect_host = Some(host);
        self
    }

    pub(crate) fn process_registry(
        mut self,
        decorate: impl FnOnce(
            Arc<dyn lash_core::ProcessRegistry>,
        ) -> Arc<dyn lash_core::ProcessRegistry>,
    ) -> Self {
        self.process_registry = Some(decorate(self.inner.process_registry()));
        self
    }

    pub(crate) fn process_env_store(
        mut self,
        decorate: impl FnOnce(
            Arc<dyn lash_core::ProcessExecutionEnvStore>,
        ) -> Arc<dyn lash_core::ProcessExecutionEnvStore>,
    ) -> Self {
        self.process_env_store = Some(decorate(self.inner.process_env_store()));
        self
    }

    /// Drive this backend's processes through `wire`, which receives the
    /// (possibly decorated) registry the wiring must be built over.
    pub(crate) fn process_work(
        mut self,
        wire: impl FnOnce(Arc<dyn lash_core::ProcessRegistry>) -> lash_core::ProcessWorkWiring,
    ) -> Self {
        let registry = lash_core::Backend::process_registry(&self);
        self.process_work = Some(wire(registry));
        self
    }
}

impl lash_core::Backend for DecoratedBackend {
    fn binding_identity(&self) -> &str {
        self.effect_host_binding
            .as_deref()
            .unwrap_or_else(|| self.inner.binding_identity())
    }

    fn clock(&self) -> Arc<dyn lash_core::Clock> {
        self.clock.clone().unwrap_or_else(|| self.inner.clock())
    }

    fn session_store_factory(&self) -> Arc<dyn lash_core::SessionStoreFactory> {
        self.session_store_factory
            .clone()
            .unwrap_or_else(|| self.inner.session_store_factory())
    }

    fn effect_host(&self) -> Arc<dyn lash_core::EffectHost> {
        self.effect_host
            .clone()
            .unwrap_or_else(|| self.inner.effect_host())
    }

    fn process_registry(&self) -> Arc<dyn lash_core::ProcessRegistry> {
        self.process_registry
            .clone()
            .unwrap_or_else(|| self.inner.process_registry())
    }

    fn trigger_store(&self) -> Arc<dyn lash_core::TriggerStore> {
        self.inner.trigger_store()
    }

    fn process_definition_registry(&self) -> Arc<dyn lash_core::ProcessDefinitionRegistry> {
        self.inner.process_definition_registry()
    }

    fn process_env_store(&self) -> Arc<dyn lash_core::ProcessExecutionEnvStore> {
        self.process_env_store
            .clone()
            .unwrap_or_else(|| self.inner.process_env_store())
    }

    fn attachment_store(&self) -> Arc<dyn lash_core::AttachmentStore> {
        self.inner.attachment_store()
    }

    fn module_artifacts(&self) -> Arc<dyn lash_core::ModuleArtifactStore> {
        self.module_artifacts
            .clone()
            .unwrap_or_else(|| self.inner.module_artifacts())
    }

    fn process_work(&self) -> Option<lash_core::ProcessWorkWiring> {
        self.process_work
            .clone()
            .or_else(|| self.inner.process_work())
    }

    fn session_work(&self) -> Option<std::sync::Arc<dyn lash_core::runtime::SessionWorkEngine>> {
        self.inner.session_work()
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
