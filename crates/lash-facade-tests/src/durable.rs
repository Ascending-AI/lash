use super::*;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_kernel_value::identity::SessionId;

/// Catalog wrapper that counts which seam a caller reached for, and fails
/// the by-id seam once a law arms it.
///
/// A Durable Session must resolve through the non-creating by-id seam and
/// must never reach `admit_session`; these counters are what makes that a
/// test rather than a claim.
pub struct CountingDeploymentStore {
    pub inner: Arc<dyn lash_execution_contracts::deployment::DeploymentStore>,
    /// Every call of the creating seam, whatever it answered.
    pub admissions: AtomicUsize,
    pub creates: AtomicUsize,
    pub by_id_opens: AtomicUsize,
    /// The refusal every by-id lookup answers while armed.
    pub failing: StdMutex<Option<CatalogFailure>>,
}

impl CountingDeploymentStore {
    pub fn new(inner: Arc<dyn lash_execution_contracts::deployment::DeploymentStore>) -> Self {
        Self {
            inner,
            admissions: AtomicUsize::new(0),
            creates: AtomicUsize::new(0),
            by_id_opens: AtomicUsize::new(0),
            failing: StdMutex::new(None),
        }
    }

    pub fn reset(&self) {
        self.admissions.store(0, Ordering::SeqCst);
        self.by_id_opens.store(0, Ordering::SeqCst);
    }
}

#[async_trait]
impl lash_core::store::RuntimeStoreDecorator for CountingDeploymentStore {
    type Inner = dyn lash_execution_contracts::deployment::DeploymentStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn admit_session(
        &self,
        request: &lash_core::SessionStoreCreateRequest,
    ) -> std::result::Result<lash_core::store::SessionAdmission, StoreError> {
        self.admissions.fetch_add(1, Ordering::SeqCst);
        let admission = self.inner.admit_session(request).await?;
        if admission == lash_core::store::SessionAdmission::Created {
            self.creates.fetch_add(1, Ordering::SeqCst);
        }
        Ok(admission)
    }

    async fn lookup_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<lash_core::store::SessionLookup, StoreError> {
        self.by_id_opens.fetch_add(1, Ordering::SeqCst);
        if let Some(failure) = *self.failing.lock_recover() {
            return Err(failure.error());
        }
        self.inner.lookup_session(session_id).await
    }
}

impl lash_core::DeploymentStoreDecorator for CountingDeploymentStore {}

/// A SQLite memory backend whose catalog counts, and its counter.
pub async fn counting_backend() -> (
    lash_execution_contracts::backend::Backend,
    Arc<CountingDeploymentStore>,
) {
    let inner = sqlite_memory_store_backend().await;
    let counting = Arc::new(CountingDeploymentStore::new(inner.session_store_factory()));
    let catalog = Arc::clone(&counting);
    let backend = DecoratedBackend::over(inner)
        .session_store_factory(move |_| catalog)
        .into_backend();
    (backend, counting)
}

/// A core over `backend` that serves its sessions when `serve` says so.
pub fn core_serving(backend: lash_execution_contracts::backend::Backend, serve: bool) -> LashCore {
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .serve_sessions(serve)
        .build(runtime_lease_owner())
        .expect("standard core")
}

pub fn id(text: &str) -> lash_kernel_value::identity::SessionId {
    lash_kernel_value::identity::SessionId::parse(text).expect("nonblank host identity")
}

pub fn turn(text: &str) -> lash_vocabulary::identity::TurnId {
    lash_vocabulary::identity::TurnId::parse(text).expect("nonblank host identity")
}

pub async fn create(core: &LashCore, session: &str) -> lash::DurableSession {
    core.session(id(session))
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await
        .expect("create the session")
}

#[derive(Clone, Copy)]
pub enum CatalogFailure {
    Contended,
    Unsupported,
}

impl CatalogFailure {
    pub fn error(self) -> StoreError {
        match self {
            Self::Contended => StoreError::Contended,
            Self::Unsupported => StoreError::UnsupportedStoreOperation {
                operation: "lookup_session",
            },
        }
    }

    pub fn is_preserved(self, error: &EmbedError) -> bool {
        let typed = match (self, error) {
            (Self::Contended, EmbedError::Store(StoreError::Contended)) => true,
            (
                Self::Unsupported,
                EmbedError::Store(StoreError::UnsupportedStoreOperation { operation }),
            ) => *operation == "lookup_session",
            _ => false,
        };
        typed && error.is_retryable() == matches!(self, Self::Contended)
    }
}

pub async fn catalog_failure_matrix(failure: CatalogFailure) {
    let (backend, counts) = counting_backend().await;
    let core = core_serving(backend, false);
    create(&core, "catalog-failure").await;
    let durable = core
        .session(id("catalog-failure"))
        .durable()
        .await
        .expect("a durable handle resolves lazily");
    counts.reset();
    *counts.failing.lock_recover() = Some(failure);
    let results = [
        (
            "live open",
            core.session(id("catalog-failure")).open().await.map(drop),
        ),
        (
            "durable acquisition",
            durable.pending_turn_inputs().await.map(drop),
        ),
        ("tombstone read", durable.was_deleted().await.map(drop)),
        (
            "process session scope",
            core.processes()
                .session_scope(&SessionId::from("catalog-failure"))
                .await
                .map(drop),
        ),
    ];
    let failures = results
        .into_iter()
        .filter_map(|(api, result)| match result {
            Err(error) if failure.is_preserved(&error) => None,
            other => Some(format!("{api}: {other:?}")),
        })
        .collect::<Vec<_>>();
    assert_eq!(counts.admissions.load(Ordering::SeqCst), 0);
    assert!(
        counts.by_id_opens.load(Ordering::SeqCst) >= 4,
        "every existing-session API resolved through the by-id seam"
    );
    assert!(
        failures.is_empty(),
        "catalog failures lost their typed carrier or retryability: {failures:?}"
    );
    core.shutdown().await.expect("shutdown");
}

/// Counters for everything a runtime build does that durable access must
/// not.
#[derive(Default)]
pub struct RuntimeBuildCounters {
    pub plugin_materializations: AtomicUsize,
    pub session_restored_events: AtomicUsize,
}

impl RuntimeBuildCounters {
    pub fn snapshot(&self) -> (usize, usize) {
        (
            self.plugin_materializations.load(Ordering::SeqCst),
            self.session_restored_events.load(Ordering::SeqCst),
        )
    }
}

pub struct RuntimeBuildProbeFactory {
    pub counters: Arc<RuntimeBuildCounters>,
}

impl lash_core::facade_support::PluginFactory for RuntimeBuildProbeFactory {
    fn id(&self) -> &'static str {
        "durable-session-runtime-probe"
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<lash::plugins::PluginRecipe, lash::plugins::PluginError> {
        let mut reg = lash::plugins::PluginRegistrar::<RuntimeBuildProbePlugin>::new(
            lash::plugins::PluginFactory::id(self.as_ref()),
        );
        RuntimeBuildProbePlugin::declare(&mut reg)?;
        let id = self.id();
        Ok(lash::plugins::PluginRecipe::new(
            id,
            reg,
            move |_ctx, _state| {
                self.counters
                    .plugin_materializations
                    .fetch_add(1, Ordering::SeqCst);
                Ok(Arc::new(RuntimeBuildProbePlugin {
                    counters: Arc::clone(&self.counters),
                }))
            },
        ))
    }
}

impl RuntimeBuildProbeFactory {
    pub fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("durable-session-runtime-probe")
    }
}
impl lash_core::plugin::PluginMetadata for RuntimeBuildProbeFactory {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        Self::declaration()
    }
}

pub struct RuntimeBuildProbePlugin {
    pub counters: Arc<RuntimeBuildCounters>,
}

impl lash_core::facade_support::SessionPlugin for RuntimeBuildProbePlugin {
    fn id(&self) -> &'static str {
        "durable-session-runtime-probe"
    }
}
impl RuntimeBuildProbePlugin {
    pub fn declare(
        reg: &mut lash::plugins::PluginRegistrar<Self>,
    ) -> std::result::Result<(), lash::plugins::PluginError> {
        reg.lifecycle().on_event(
            lash::hook_key!("session-on-event-1"),
            move |this, _state| {
                let counters = Arc::clone(&this.counters);
                Ok(Arc::new(move |event| {
                    let counters = Arc::clone(&counters);
                    Box::pin(async move {
                        if matches!(
                            event,
                            lash_core::facade_support::PluginLifecycleEvent::SessionRestored { .. }
                        ) {
                            counters
                                .session_restored_events
                                .fetch_add(1, Ordering::SeqCst);
                        }
                        Ok(())
                    })
                }))
            },
        )?;
        Ok(())
    }
}

/// A probed core over `backend`, serving its sessions when `serve` says so.
pub fn probed_core(
    backend: lash_execution_contracts::backend::Backend,
    counters: &Arc<RuntimeBuildCounters>,
    serve: bool,
) -> LashCore {
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .plugin(Arc::new(RuntimeBuildProbeFactory {
            counters: Arc::clone(counters),
        }))
        .serve_sessions(serve)
        .build(runtime_lease_owner())
        .expect("probed core")
}

/// Read the session head's committed `tool_state`.
pub async fn persisted_tool_state(
    backend: &lash_execution_contracts::backend::Backend,
    session_id: &SessionId,
) -> String {
    let store = lash_execution_contracts::deployment::live_session_view(
        &backend.session_store_factory(),
        session_id,
    )
    .await
    .expect("session view")
    .expect("the session has a store");
    let head = store
        .load_session_head_meta()
        .await
        .expect("load the head")
        .expect("the session has a head");
    format!("{head:?}")
}

/// A catalog that creates and deletes but cannot resolve a session by id.
pub struct NoByIdLookupFactory {
    pub inner: Arc<dyn lash_execution_contracts::deployment::DeploymentStore>,
}

pub const NO_BY_ID_LOOKUP_OPERATION: &str = "SessionCatalogStore::lookup_session";

#[async_trait]
impl lash_core::store::RuntimeStoreDecorator for NoByIdLookupFactory {
    type Inner = dyn lash_execution_contracts::deployment::DeploymentStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn lookup_session(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<lash_core::store::SessionLookup, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: NO_BY_ID_LOOKUP_OPERATION,
        })
    }
}

impl lash_core::DeploymentStoreDecorator for NoByIdLookupFactory {}

pub fn drained_input(
    durable: &lash::DurableSession,
    id: &str,
) -> lash_vocabulary::identity::InputId {
    durable.input_id(&turn(id))
}

/// Whether `entry` is a message spoken in `role`.
pub fn is_role(
    entry: &lash::transcript::TranscriptEntry,
    role: lash::transcript::TranscriptRole,
) -> bool {
    matches!(&entry.item, lash::transcript::TranscriptItem::Message(message) if message.role == role)
}

/// A message entry's text blocks, one per line.
pub fn text_of(entry: &lash::transcript::TranscriptEntry) -> String {
    let lash::transcript::TranscriptItem::Message(message) = &entry.item else {
        return String::new();
    };
    message
        .blocks
        .iter()
        .filter_map(|block| match block {
            lash::transcript::TranscriptBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn whole_history() -> lash::persistence::HistoryBudget {
    lash::persistence::HistoryBudget {
        max_nodes: std::num::NonZeroU32::MIN.saturating_add(127),
        max_bytes: std::num::NonZeroU64::MIN.saturating_add(32 * 1024 * 1024 - 1),
    }
}

/// `app_lookup` answering its first call at once. Its second holds until
/// the law releases it, then tells `answering` and answers only once
/// `accepted` says the law's cancel was accepted, blocking its runtime's one
/// worker meanwhile: the turn's owner looks for the cancel only after the
/// call answered.
#[cfg(feature = "rlm")]
pub struct SecondLookupHeld {
    pub calls: std::sync::atomic::AtomicUsize,
    pub entered: Arc<tokio::sync::Notify>,
    pub release: Arc<tokio::sync::Notify>,
    pub answering: std::sync::mpsc::SyncSender<()>,
    pub accepted: Arc<StdMutex<std::sync::mpsc::Receiver<()>>>,
}

#[cfg(feature = "rlm")]
#[async_trait]
impl ToolProvider for SecondLookupHeld {
    fn tool_manifests(
        &self,
        config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash::plugins::PluginError,
    > {
        AppTools.tool_manifests(config)
    }

    fn resolve_contract(
        &self,
        name: &str,
        config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        AppTools.resolve_contract(name, config)
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
            self.entered.notify_one();
            self.release.notified().await;
            self.answering
                .send(())
                .expect("the law waits for the answer");
            self.accepted
                .lock()
                .expect("the law's lock")
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("the law's cancel is accepted");
        }
        lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })).into()
    }
}
