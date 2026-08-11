use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::events::ProcessAwaitOutput;
use super::model::{
    ProcessExecutionContext, ProcessExecutionEnvSpec, ProcessIdentity, ProcessRegistration,
};
use super::registry::ProcessRegistry;

/// Opaque engine-owned state carried between in-process execution segments.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SegmentHandover {
    pub reason: crate::BoundaryReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_hash: Option<String>,
    pub engine_state: Vec<u8>,
}

/// The single bounded continuation durably retained for a process incarnation.
///
/// This is registry-internal execution state: it is deliberately not a process
/// event and therefore never appears in change feeds or provenance.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PersistedSegmentHandover {
    pub segment_ordinal: u64,
    pub program_hash: String,
    pub handover: SegmentHandover,
}

/// Result of one process invocation. A segment boundary is never terminal.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessRunOutcome {
    Terminal(Box<ProcessAwaitOutput>),
    SegmentBoundary(SegmentHandover),
}

/// Failure of the host/runtime infrastructure needed to execute a process.
///
/// Infrastructure failures are not producer outcomes and must not be persisted
/// as terminal process failures. The worker leaves the row claimable so its
/// configured retry pacing and attempt budget can decide what happens next.
#[derive(Debug)]
pub struct ProcessInfraError {
    source: crate::PluginError,
}

impl ProcessInfraError {
    /// Constructs a `ProcessInfraError` for protocol and process-engine implementors while running
    /// a durable process.
    pub fn new(source: crate::PluginError) -> Self {
        Self { source }
    }

    pub(crate) fn into_plugin_error(self) -> crate::PluginError {
        self.source
    }
}

impl std::fmt::Display for ProcessInfraError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for ProcessInfraError {}

impl From<crate::PluginError> for ProcessInfraError {
    fn from(source: crate::PluginError) -> Self {
        Self::new(source)
    }
}

impl From<ProcessAwaitOutput> for ProcessRunOutcome {
    fn from(output: ProcessAwaitOutput) -> Self {
        Self::Terminal(Box::new(output))
    }
}

pub type ProcessEngineShutdownFuture<'run> = Pin<Box<dyn Future<Output = ()> + Send + 'run>>;

pub struct ProcessEngineRunGuard<'run> {
    shutdown: Option<Box<dyn FnOnce() -> ProcessEngineShutdownFuture<'run> + Send + 'run>>,
}

impl<'run> ProcessEngineRunGuard<'run> {
    pub(crate) fn new(
        shutdown: impl FnOnce() -> ProcessEngineShutdownFuture<'run> + Send + 'run,
    ) -> Self {
        Self {
            shutdown: Some(Box::new(shutdown)),
        }
    }

    pub async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown().await;
        }
    }
}

pub struct ProcessEngineRuntimeContext<'run> {
    context: crate::RuntimeExecutionContext<'run>,
    guard: ProcessEngineRunGuard<'run>,
}

impl<'run> ProcessEngineRuntimeContext<'run> {
    pub(crate) fn new(
        context: crate::RuntimeExecutionContext<'run>,
        guard: ProcessEngineRunGuard<'run>,
    ) -> Self {
        Self { context, guard }
    }

    pub fn context(&self) -> &crate::RuntimeExecutionContext<'run> {
        &self.context
    }

    pub fn into_parts(
        self,
    ) -> (
        crate::RuntimeExecutionContext<'run>,
        ProcessEngineRunGuard<'run>,
    ) {
        (self.context, self.guard)
    }

    pub async fn shutdown(self) {
        self.guard.shutdown().await;
    }
}

type RuntimeContextBuilder<'run> = Box<
    dyn FnOnce(
            Arc<crate::ToolCatalog>,
        ) -> Result<ProcessEngineRuntimeContext<'run>, crate::PluginError>
        + Send
        + 'run,
>;

/// Process-registry capabilities scoped to one engine execution.
///
/// Engines can inspect their own record and event history, maintain their
/// durable wait, emit execution-owned events through the installed authority,
/// and await other process handles. The underlying registry is deliberately
/// not exposed: host-owned signal/cancel appends and lifecycle writes remain
/// outside the engine extension boundary.
#[derive(Clone)]
pub struct ProcessEngineProcessContext {
    process_id: String,
    registry: Arc<dyn ProcessRegistry>,
    execution_write_authority: super::model::ProcessExecutionWriteAuthority,
    awaiter: super::awaiter::ProcessAwaiter,
    store: Option<Arc<dyn crate::RuntimePersistence>>,
    session_store_factory: Option<Arc<dyn crate::SessionStoreFactory>>,
    queued_work_driver: Option<crate::QueuedWorkDriver>,
    process_wake_delivery_policy: crate::DeliveryPolicy,
    clock: Arc<dyn crate::Clock>,
}

impl ProcessEngineProcessContext {
    #[allow(clippy::too_many_arguments)]
    fn new(
        process_id: String,
        registry: Arc<dyn ProcessRegistry>,
        execution_write_authority: super::model::ProcessExecutionWriteAuthority,
        awaiter: super::awaiter::ProcessAwaiter,
        store: Option<Arc<dyn crate::RuntimePersistence>>,
        session_store_factory: Option<Arc<dyn crate::SessionStoreFactory>>,
        queued_work_driver: Option<crate::QueuedWorkDriver>,
        process_wake_delivery_policy: crate::DeliveryPolicy,
        clock: Arc<dyn crate::Clock>,
    ) -> Self {
        Self {
            process_id,
            registry,
            execution_write_authority,
            awaiter,
            store,
            session_store_factory,
            queued_work_driver,
            process_wake_delivery_policy,
            clock,
        }
    }

    pub async fn record(&self) -> Result<Option<super::model::ProcessRecord>, crate::PluginError> {
        self.registry.get_process(&self.process_id).await
    }

    pub async fn events_after(
        &self,
        after_sequence: u64,
    ) -> Result<Vec<super::events::ProcessEvent>, crate::PluginError> {
        self.registry
            .events_after(&self.process_id, after_sequence)
            .await
    }

    pub async fn emit(
        &self,
        request: super::events::ProcessEventAppendRequest,
    ) -> Result<super::events::ProcessEvent, crate::PluginError> {
        let result = self
            .registry
            .append_event_with_authority(&self.process_id, request, &self.execution_write_authority)
            .await?;
        crate::tool_provider::process_events::enqueue_wake_delivery(
            Arc::clone(&self.registry),
            self.store.clone(),
            self.session_store_factory.as_ref(),
            result.wake_delivery,
            None,
            self.queued_work_driver.as_ref(),
            self.process_wake_delivery_policy,
            Arc::clone(&self.clock),
        )
        .await?;
        Ok(result.event)
    }

    pub async fn set_wait(
        &self,
        wait: super::model::WaitState,
    ) -> Result<super::model::ProcessRecord, crate::PluginError> {
        self.registry
            .set_process_wait_with_authority(
                &self.process_id,
                wait,
                &self.execution_write_authority,
            )
            .await
    }

    pub async fn clear_wait(&self) -> Result<super::model::ProcessRecord, crate::PluginError> {
        self.registry
            .clear_process_wait_with_authority(&self.process_id, &self.execution_write_authority)
            .await
    }

    pub async fn await_terminal(
        &self,
        process_id: &str,
    ) -> Result<ProcessAwaitOutput, crate::PluginError> {
        self.awaiter.await_terminal(process_id).await
    }
}

pub struct ProcessEngineRunContext<'run> {
    registration: ProcessRegistration,
    execution_context: ProcessExecutionContext,
    processes: ProcessEngineProcessContext,
    session_id: String,
    plugins: Arc<crate::PluginSession>,
    store: Option<Arc<dyn crate::RuntimePersistence>>,
    session_store_factory: Option<Arc<dyn crate::SessionStoreFactory>>,
    queued_work_driver: Option<crate::QueuedWorkDriver>,
    process_registry_available: bool,
    cancellation: CancellationToken,
    turn_phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
    scoped_effect_controller: crate::ScopedEffectController<'run>,
    handover: Option<SegmentHandover>,
    runtime_context_builder: Option<RuntimeContextBuilder<'run>>,
}

impl<'run> ProcessEngineRunContext<'run> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        registry: Arc<dyn ProcessRegistry>,
        process_awaiter: super::awaiter::ProcessAwaiter,
        session_id: String,
        plugins: Arc<crate::PluginSession>,
        store: Option<Arc<dyn crate::RuntimePersistence>>,
        session_store_factory: Option<Arc<dyn crate::SessionStoreFactory>>,
        queued_work_driver: Option<crate::QueuedWorkDriver>,
        process_wake_delivery_policy: crate::DeliveryPolicy,
        clock: Arc<dyn crate::Clock>,
        process_registry_available: bool,
        cancellation: CancellationToken,
        turn_phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
        scoped_effect_controller: crate::ScopedEffectController<'run>,
        handover: Option<SegmentHandover>,
        runtime_context_builder: RuntimeContextBuilder<'run>,
    ) -> Self {
        let execution_write_authority = execution_context
            .execution_write_authority
            .clone()
            .expect("process worker installs execution write authority");
        let processes = ProcessEngineProcessContext::new(
            registration.id.clone(),
            registry,
            execution_write_authority,
            process_awaiter,
            store.clone(),
            session_store_factory.clone(),
            queued_work_driver.clone(),
            process_wake_delivery_policy,
            clock,
        );
        Self {
            registration,
            execution_context,
            processes,
            session_id,
            plugins,
            store,
            session_store_factory,
            queued_work_driver,
            process_registry_available,
            cancellation,
            turn_phase_probe,
            scoped_effect_controller,
            handover,
            runtime_context_builder: Some(runtime_context_builder),
        }
    }

    /// Exposes registration to protocol and process-engine implementors while running a durable
    /// process.
    pub fn registration(&self) -> &ProcessRegistration {
        &self.registration
    }

    /// Exposes execution context to protocol and process-engine implementors while running a
    /// durable process.
    pub fn execution_context(&self) -> &ProcessExecutionContext {
        &self.execution_context
    }

    /// Exposes processes to protocol and process-engine implementors while running a durable
    /// process.
    pub fn processes(&self) -> ProcessEngineProcessContext {
        self.processes.clone()
    }

    /// Exposes session id to protocol and process-engine implementors while running a durable
    /// process.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Exposes plugins to protocol and process-engine implementors while running a durable process.
    pub fn plugins(&self) -> Arc<crate::PluginSession> {
        Arc::clone(&self.plugins)
    }

    /// Exposes store to protocol and process-engine implementors while running a durable process.
    /// Returns `None` when no store is present.
    pub fn store(&self) -> Option<Arc<dyn crate::RuntimePersistence>> {
        self.store.clone()
    }

    /// Exposes session store factory to protocol and process-engine implementors while running a
    /// durable process. Returns `None` when no session store factory is present.
    pub fn session_store_factory(&self) -> Option<Arc<dyn crate::SessionStoreFactory>> {
        self.session_store_factory.clone()
    }

    /// Exposes queued work driver to protocol and process-engine implementors while running a
    /// durable process. Returns `None` when no queued work driver is present.
    pub fn queued_work_driver(&self) -> Option<crate::QueuedWorkDriver> {
        self.queued_work_driver.clone()
    }

    /// Exposes process registry available to protocol and process-engine implementors while running
    /// a durable process.
    pub fn process_registry_available(&self) -> bool {
        self.process_registry_available
    }

    /// Exposes cancellation token to protocol and process-engine implementors while running a
    /// durable process.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Exposes effect controller to protocol and process-engine implementors while running a
    /// durable process.
    pub fn effect_controller(&self) -> &dyn crate::RuntimeEffectController {
        self.scoped_effect_controller.controller()
    }

    /// Exposes scoped effect controller to protocol and process-engine implementors while running a
    /// durable process.
    pub fn scoped_effect_controller(&self) -> crate::ScopedEffectController<'run> {
        self.scoped_effect_controller.clone()
    }

    /// Transfers the persisted segment handover to a process-engine implementor exactly once,
    /// returning `None` after it has been taken or when no predecessor exists.
    pub fn take_handover(&mut self) -> Option<SegmentHandover> {
        self.handover.take()
    }

    #[doc(hidden)]
    pub fn named_phase(&self, phase: &'static str) -> crate::runtime::RuntimeNamedPhase {
        crate::runtime::RuntimeNamedPhase::begin(self.turn_phase_probe.clone(), phase)
    }

    #[doc(hidden)]
    pub fn turn_phase_probe(&self) -> Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>> {
        self.turn_phase_probe.clone()
    }

    /// Exposes resolved tool catalog to protocol and process-engine implementors while running a
    /// durable process.
    pub fn resolved_tool_catalog(&self) -> Result<Arc<crate::ToolCatalog>, crate::PluginError> {
        self.plugins.resolved_tool_catalog(&self.session_id)
    }

    /// Extracts the runtime context outcome for protocol and process-engine implementors while
    /// running a durable process.
    pub fn into_runtime_context(
        mut self,
        tool_catalog: Arc<crate::ToolCatalog>,
    ) -> Result<ProcessEngineRuntimeContext<'run>, crate::PluginError> {
        let builder = self.runtime_context_builder.take().ok_or_else(|| {
            crate::PluginError::Session("process engine runtime context was already built".into())
        })?;
        builder(tool_catalog)
    }
}

pub struct ProcessEngineValidationContext<'a> {
    plugin_host: &'a crate::PluginHost,
    tool_catalog: Arc<crate::ToolCatalog>,
    process_registry_available: bool,
}

impl<'a> ProcessEngineValidationContext<'a> {
    pub(crate) fn new(
        plugin_host: &'a crate::PluginHost,
        tool_catalog: Arc<crate::ToolCatalog>,
        process_registry_available: bool,
    ) -> Self {
        Self {
            plugin_host,
            tool_catalog,
            process_registry_available,
        }
    }

    /// Exposes plugin host to protocol and process-engine implementors while running a durable
    /// process.
    pub fn plugin_host(&self) -> &crate::PluginHost {
        self.plugin_host
    }

    /// Exposes tool catalog to protocol and process-engine implementors while running a durable
    /// process.
    pub fn tool_catalog(&self) -> &crate::ToolCatalog {
        self.tool_catalog.as_ref()
    }

    /// Exposes process registry available to protocol and process-engine implementors while running
    /// a durable process.
    pub fn process_registry_available(&self) -> bool {
        self.process_registry_available
    }
}

#[async_trait::async_trait]
/// Deployment extension point for non-kernel process runtimes.
///
/// Core built-ins (`ToolCall`, `SessionTurn`, and `External`) are intentionally
/// not registered here; they are kernel primitives with direct orchestration
/// support. Implement `ProcessEngine` for process kinds stored as
/// [`ProcessInput::Engine`](super::model::ProcessInput::Engine).
pub trait ProcessEngine: Send + Sync {
    fn kind(&self) -> &'static str;

    async fn validate_start(
        &self,
        _context: ProcessEngineValidationContext<'_>,
        _payload: &serde_json::Value,
        _env_spec: Option<&ProcessExecutionEnvSpec>,
    ) -> Result<(), crate::PluginError> {
        Ok(())
    }

    async fn run(
        &self,
        context: ProcessEngineRunContext<'_>,
        payload: serde_json::Value,
    ) -> Result<ProcessRunOutcome, ProcessInfraError>;

    fn identity(&self, payload: &serde_json::Value) -> ProcessIdentity {
        let _ = payload;
        ProcessIdentity::new(self.kind())
    }
}

#[derive(Clone, Default)]
pub struct ProcessEngineRegistry {
    engines: Arc<BTreeMap<String, Arc<dyn ProcessEngine>>>,
}

impl ProcessEngineRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_engine(self, engine: Arc<dyn ProcessEngine>) -> Self {
        let mut engines = (*self.engines).clone();
        engines.insert(engine.kind().to_string(), engine);
        Self {
            engines: Arc::new(engines),
        }
    }

    /// Register an engine, rejecting a duplicate
    /// [`ProcessEngine::kind`]. This is the single enforcement point for unique
    /// engine kinds across everything registered on a runtime host, whether the
    /// engine was wired directly or contributed through the plugin contract.
    pub(crate) fn try_with_engine(
        self,
        engine: Arc<dyn ProcessEngine>,
    ) -> Result<Self, crate::PluginError> {
        if self.engines.contains_key(engine.kind()) {
            return Err(crate::PluginError::Registration(format!(
                "duplicate process engine kind `{}`; each engine kind may be registered once",
                engine.kind()
            )));
        }
        Ok(self.with_engine(engine))
    }

    pub(crate) fn get(&self, kind: &str) -> Option<Arc<dyn ProcessEngine>> {
        self.engines.get(kind).cloned()
    }

    pub(crate) fn require(&self, kind: &str) -> Result<Arc<dyn ProcessEngine>, crate::PluginError> {
        self.get(kind).ok_or_else(|| {
            crate::PluginError::Session(format!("process engine `{kind}` is not configured"))
        })
    }
}
