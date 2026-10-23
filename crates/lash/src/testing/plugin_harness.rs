//! Author assertions over real sessions, admissions and durable SQLite state.
use lash_core::plugin::{PluginFactory, PluginMetadata, PluginRecipe};
use lash_core::testing::CallbackCounts;
use lash_protocol_types::sync::MutexExt;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod certification;
pub use certification::*;
pub use lash_core::testing::CallbackCounts as PluginCallbackCounts;

/// Whether the author supplies an extension or the session protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginRole {
    Extension,
    Protocol,
}

/// Host limits used on publication, materialization and every reopen.
#[derive(Clone)]
pub struct HostPolicy {
    pub plugin_role: PluginRole,
    pub data_limits: crate::RuntimeDataLimits,
    pub commit_budget: crate::CommitBudget,
    pub execution_budgets: crate::ExecutionBudgets,
    /// A failed scenario ends with a readable error instead of hanging.
    pub scenario_timeout: Duration,
}
impl HostPolicy {
    pub fn standard() -> Self {
        Self {
            plugin_role: PluginRole::Extension,
            data_limits: crate::RuntimeDataLimits::standard(),
            commit_budget: crate::CommitBudget::default(),
            execution_budgets: crate::ExecutionBudgets::recommended(),
            scenario_timeout: Duration::from_secs(15),
        }
    }
}

/// Harness infrastructure errors preserve runtime and storage causes.
#[derive(Debug, thiserror::Error)]
pub enum PluginHarnessError {
    #[error(transparent)]
    Runtime(#[from] crate::EmbedError),
    #[error(transparent)]
    Store(#[from] lash_core::StoreError),
    #[error(transparent)]
    Plugin(#[from] crate::plugins::PluginError),
    #[error(transparent)]
    Durable(#[from] crate::durable::DurableError),
    #[error(transparent)]
    Diagnostic(#[from] crate::plugins::diagnostics::DiagnosticError),
    #[error("plugin harness runtime: {0}")]
    Worker(String),
    #[error(
        "a supporting protocol requires an extension under certification and no other supporting protocol"
    )]
    SupportingProtocolConflict,
    #[error("scenario did not reach its model pause within the host timeout")]
    Timeout,
}

struct CountedFactory {
    inner: Arc<dyn PluginFactory>,
    counts: CallbackCounts,
}
impl PluginMetadata for CountedFactory {
    fn plugin_declaration(&self) -> crate::plugins::PluginDeclaration {
        self.inner.plugin_declaration()
    }
}
#[async_trait::async_trait]
impl PluginFactory for CountedFactory {
    fn initialize_state(
        &self,
        owner: &lash_vocabulary::identity::RuntimeOwner,
        config: &crate::plugins::PluginConfig,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, crate::plugins::PluginError>
    {
        self.inner.initialize_state(owner, config)
    }
    fn migrate_format(
        &self,
        from: crate::plugins::FormatVersion,
        namespace: crate::plugins::FormatNamespace,
        value: serde_json::Value,
    ) -> Result<serde_json::Value, crate::plugins::FormatRefusal> {
        self.inner.migrate_format(from, namespace, value)
    }
    fn encode_format(
        &self,
        to: crate::plugins::FormatVersion,
        namespace: crate::plugins::FormatNamespace,
        value: &serde_json::Value,
    ) -> Result<serde_json::Value, crate::plugins::FormatRefusal> {
        self.inner.encode_format(to, namespace, value)
    }
    fn bound_backend(&self) -> Option<&str> {
        self.inner.bound_backend()
    }
    async fn shutdown(&self) -> Result<(), crate::plugins::PluginError> {
        self.inner.shutdown().await
    }

    fn id(&self) -> &'static str {
        self.inner.id()
    }
    fn recipe(self: Arc<Self>) -> Result<PluginRecipe, crate::plugins::PluginError> {
        Ok(Arc::clone(&self.inner)
            .recipe()?
            .counted(self.counts.clone()))
    }
}

struct Script {
    responses: Mutex<VecDeque<crate::provider::LlmResponse>>,
    requests: Mutex<Vec<crate::provider::LlmRequest>>,
    calls: AtomicUsize,
    pause_at: AtomicUsize,
    paused: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}
struct Worker {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A plugin author's factory running on lash's durable engine over SQLite
/// memory. Each boot has a fresh Tokio runtime. Crash destroys that runtime,
/// including its executing tasks; reopen retains only the SQL stores and the
/// host-owned factory and script. Hosts still submit through `send()`.
pub struct PluginHarness {
    factory: Arc<dyn PluginFactory>,
    policy: HostPolicy,
    supporting: Vec<Arc<dyn PluginFactory>>,
    supporting_protocol: Option<Arc<dyn PluginFactory>>,
    backend: crate::Backend,
    script: Arc<Script>,
    counts: CallbackCounts,
    core: Option<crate::LashCore>,
    worker: Option<Worker>,
    session: lash_vocabulary::identity::SessionId,
}
impl PluginHarness {
    /// Construct an isolated database and boot the real runtime.
    pub async fn new(
        factory: Arc<dyn PluginFactory>,
        policy: HostPolicy,
    ) -> Result<Self, PluginHarnessError> {
        let stores = Arc::new(
            lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .map_err(|error| lash_core::StoreError::Backend(error.to_string()))?,
        );
        let backend = crate::durable::DurableBackendBuilder::new(stores)
            .config(lash_durable::DurableSettings::standard())
            .build()
            .map_err(|error| PluginHarnessError::Worker(error.to_string()))?;
        let mut harness = Self {
            factory,
            policy,
            supporting: Vec::new(),
            supporting_protocol: None,
            backend,
            script: Arc::new(Script {
                responses: Mutex::new(VecDeque::new()),
                requests: Mutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
                pause_at: AtomicUsize::new(usize::MAX),
                paused: tokio::sync::Notify::new(),
                release: tokio::sync::Semaphore::new(0),
            }),
            counts: CallbackCounts::default(),
            core: None,
            worker: None,
            session: lash_vocabulary::identity::SessionId::parse("plugin-certification")
                .map_err(|error| PluginHarnessError::Worker(error.to_string()))?,
        };
        harness
            .boot(Some(Arc::clone(&harness.factory)), true)
            .await?;
        Ok(harness)
    }
    pub fn core(&self) -> Result<&crate::LashCore, PluginHarnessError> {
        self.core
            .as_ref()
            .ok_or_else(|| PluginHarnessError::Worker("runtime is crashed; reopen it first".into()))
    }
    /// Supply a scenario's real service plugin before creating the session.
    pub async fn install(
        &mut self,
        factory: Arc<dyn PluginFactory>,
    ) -> Result<(), PluginHarnessError> {
        self.supporting.push(factory);
        self.reopen().await
    }
    /// Install the scenario's real protocol, including its bound VM service.
    /// The same factory is retained on every reopen and removal observation.
    /// Only an extension can have a supporting protocol; install before creation.
    pub async fn install_protocol(
        &mut self,
        factory: Arc<dyn PluginFactory>,
    ) -> Result<(), PluginHarnessError> {
        if self.policy.plugin_role == PluginRole::Protocol || self.supporting_protocol.is_some() {
            return Err(PluginHarnessError::SupportingProtocolConflict);
        }
        self.supporting_protocol = Some(factory);
        self.reopen().await
    }
    pub fn session_id(&self) -> &lash_vocabulary::identity::SessionId {
        &self.session
    }
    pub fn requests(&self) -> Vec<crate::provider::LlmRequest> {
        self.script.requests.lock_recover().clone()
    }
    pub fn callback_counts(
        &self,
    ) -> std::collections::BTreeMap<crate::plugins::PluginCallbackIdentity, u64> {
        self.counts.snapshot()
    }
    /// Resolver entries are independent of the provider selected to execute.
    pub fn deferred_resolution_counts(
        &self,
    ) -> std::collections::BTreeMap<crate::plugins::PluginCallbackIdentity, u64> {
        self.counts.resolution_snapshot()
    }
    /// Responses are consumed only after any armed pause, so a killed model
    /// call's response is available to the fresh runtime.
    pub fn script(&self, responses: impl IntoIterator<Item = crate::provider::LlmResponse>) {
        self.script.responses.lock_recover().extend(responses);
    }
    /// Pause before the Nth next model response. The earlier callback
    /// decisions have crossed their production publication boundary.
    pub fn pause_model_after(&self, responses: usize) {
        self.script.pause_at.store(
            self.script.calls.load(Ordering::SeqCst) + responses + 1,
            Ordering::SeqCst,
        );
    }
    pub async fn wait_for_model_pause(&self) -> Result<(), PluginHarnessError> {
        tokio::time::timeout(self.policy.scenario_timeout, self.script.paused.notified())
            .await
            .map_err(|_| PluginHarnessError::Timeout)
    }
    pub async fn create(
        &self,
        options: crate::plugins::PluginOptions,
    ) -> Result<(), PluginHarnessError> {
        self.core()?
            .session(self.session.clone())
            .create(crate::SessionCreation::root(
                crate::SessionSpec::new(
                    "plugin-script",
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(64),
                    crate::NoProgressBudget::bounded(8),
                )
                .plugin_options(options),
            ))
            .await?;
        Ok(())
    }
    pub async fn session(&self) -> Result<crate::DurableSession, PluginHarnessError> {
        Ok(self.core()?.session(self.session.clone()).durable().await?)
    }
    /// Send a real durable input and return its acceptance handle.
    pub async fn send(
        &self,
        input: crate::TurnInput,
    ) -> Result<crate::SendHandle, PluginHarnessError> {
        Ok(self.session().await?.send(input).await?)
    }
    /// Publish through a real turn, destroy the runtime and materialize the
    /// same plugin from its committed state on a fresh runtime.
    pub async fn publish_then_reopen(
        &mut self,
        input: crate::TurnInput,
    ) -> Result<crate::TurnOutput, PluginHarnessError> {
        let output = self.send(input).await?.output().await?;
        self.crash();
        self.reopen().await?;
        // Opening validates and binds the author's plugin under host limits.
        let _session = self.core()?.session(self.session.clone()).open().await?;
        Ok(output)
    }
    /// Destroy all execution tasks without draining or invoking shutdown hooks.
    pub fn crash(&mut self) {
        self.worker.take();
        self.core.take();
    }
    pub async fn reopen(&mut self) -> Result<(), PluginHarnessError> {
        self.script.pause_at.store(usize::MAX, Ordering::SeqCst);
        self.boot(Some(Arc::clone(&self.factory)), true).await
    }
    /// Reopen at the next model call so authors can inspect restored state
    /// before any new, unrecorded work advances it.
    pub async fn reopen_at_model_pause(&mut self) -> Result<(), PluginHarnessError> {
        self.pause_model_after(0);
        self.boot(Some(Arc::clone(&self.factory)), true).await?;
        self.wait_for_model_pause().await
    }
    pub fn release_model(&self) {
        self.script.pause_at.store(usize::MAX, Ordering::SeqCst);
        self.script.release.add_permits(1);
    }
    pub async fn reopen_without_plugin(&mut self) -> Result<(), PluginHarnessError> {
        self.crash();
        self.script.pause_at.store(usize::MAX, Ordering::SeqCst);
        self.boot(None, false).await
    }
    async fn boot(
        &mut self,
        factory: Option<Arc<dyn PluginFactory>>,
        serve: bool,
    ) -> Result<(), PluginHarnessError> {
        self.crash();
        let backend = self.backend.clone();
        let supporting = self.supporting.clone();
        let supporting_protocol = self.supporting_protocol.clone();
        let policy = self.policy.clone();
        let script = Arc::clone(&self.script);
        let counts = self.counts.clone();
        let (ready, receiver) = tokio::sync::oneshot::channel();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("plugin-harness".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready.send(Err(PluginHarnessError::Worker(error.to_string())));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let provider = crate::testing::TestProvider::builder()
                        .complete(move |request| {
                            let script = Arc::clone(&script);
                            async move {
                                script.requests.lock_recover().push(request);
                                let call = script.calls.fetch_add(1, Ordering::SeqCst) + 1;
                                if script.pause_at.load(Ordering::SeqCst) == call {
                                    script.paused.notify_one();
                                    if let Ok(permit) = script.release.acquire().await {
                                        permit.forget();
                                    }
                                }
                                script.responses.lock_recover().pop_front().ok_or_else(|| {
                                    crate::provider::LlmTransportError::new(
                                        "scripted model ran out of responses",
                                    )
                                })
                            }
                        })
                        .build()
                        .into_handle();
                    let builder = match policy.plugin_role {
                        PluginRole::Extension if supporting_protocol.is_none() => {
                            crate::LashCore::standard_builder(backend)
                        }
                        PluginRole::Extension => crate::LashCore::builder(backend),
                        PluginRole::Protocol => crate::LashCore::builder(backend),
                    };
                    let mut builder = builder
                        .serve_sessions(serve)
                        .commit_budget(policy.commit_budget)
                        .data_limits(policy.data_limits)
                        .data_retention(crate::DataRetention::standard())
                        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
                        .execution_budgets(policy.execution_budgets)
                        .delta_coalescing(crate::DeltaCoalescing::recommended())
                        .llm_profiles(crate::testing::single_llm_profile_registry(
                            "plugin-script",
                            crate::testing::test_llm_profile_metadata("plugin-script"),
                            provider,
                        ));
                    if let Some(inner) = factory {
                        let counted: Arc<dyn PluginFactory> =
                            Arc::new(CountedFactory { inner, counts });
                        builder = match policy.plugin_role {
                            PluginRole::Extension => builder.plugin(counted),
                            PluginRole::Protocol => builder.protocol_plugin(counted),
                        };
                    } else if policy.plugin_role == PluginRole::Protocol {
                        // Observer-only replacement: no callback of the removed
                        // protocol is installed, and no turn is executed here.
                        if let Some(protocol) =
                            lash_core::testing::test_protocol_factories_ending_without_done()
                                .into_iter()
                                .next()
                        {
                            builder = builder.protocol_plugin(protocol);
                        }
                    }
                    if let Some(protocol) = supporting_protocol {
                        builder = builder.protocol_plugin(protocol);
                    }
                    for plugin in supporting {
                        builder = builder.plugin(plugin);
                    }
                    let result = builder
                        .build(crate::persistence::LeaseOwnerIdentity::opaque(
                            crate::persistence::LeaseOwnerId::new("plugin-harness"),
                            crate::persistence::LeaseIncarnationId::new(
                                uuid::Uuid::new_v4().to_string(),
                            ),
                        ))
                        .map_err(PluginHarnessError::from);
                    let _ = ready.send(result);
                    let _ = stopped.await;
                });
                // Dropping the runtime aborts every spawned execution task.
            })
            .map_err(|error| PluginHarnessError::Worker(error.to_string()))?;
        self.worker = Some(Worker {
            stop: Some(stop),
            thread: Some(thread),
        });
        self.core = Some(
            receiver
                .await
                .map_err(|error| PluginHarnessError::Worker(error.to_string()))??,
        );
        Ok(())
    }
}
impl Drop for PluginHarness {
    fn drop(&mut self) {
        self.crash();
    }
}
impl PluginHarness {
    /// Materialize from committed storage and read the plugin namespaces.
    pub async fn plugin_state(&self) -> Result<crate::plugins::PluginState, PluginHarnessError> {
        let runtime = self
            .core()?
            .session(self.session.clone())
            .open_runtime()
            .await?;
        let plugins = runtime.assembled_plugin_session();
        plugins.materialize()?;
        if let Some(run) = self.session().await?.current_turn().await? {
            let namespaces = self
                .backend
                .durable()
                .turn_namespaces(&self.session, &run)
                .await?;
            plugins.begin_run();
            plugins.restore_run(namespaces)?;
        }
        Ok(plugins.committed_state()?)
    }
}
