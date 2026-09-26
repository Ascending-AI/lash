use crate::ProcessId;
use crate::SessionId;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::definition_ref::{
    ProcessDefinitionRef, ProcessDefinitionRefusal, ProcessDefinitionResolution, ProcessSignature,
};
use super::events::ProcessAwaitOutput;
use super::events::ProcessEventType;
use super::model::{
    ProcessExecutionContext, ProcessExecutionEnvSpec, ProcessIdentity, ProcessRegistration,
};

/// Opaque engine-owned state carried between in-process execution segments.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SegmentHandover {
    pub reason: crate::BoundaryReason,
    pub program_hash: String,
    pub engine_state: Vec<u8>,
}

/// The single bounded continuation durably retained for a process incarnation.
///
/// This is registry-internal execution state: it is deliberately not a process
/// event and therefore never appears in change feeds or provenance.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PersistedSegmentHandover {
    pub segment_ordinal: u64,
    /// The segment execution that wrote this handover: the nonce its
    /// admission recorded. A second put by the same writer is that
    /// execution's own retried write, and the store keeps the bytes it holds:
    /// the engine state carries measured wall-clock time, so a redriven
    /// segment re-derives the same handover with different bytes. Empty names
    /// no writer and matches none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub writer: String,
    pub handover: SegmentHandover,
}

impl PersistedSegmentHandover {
    pub fn program_hash(&self) -> &str {
        &self.handover.program_hash
    }
}

/// Result of one process invocation. A segment boundary is never terminal.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessRunOutcome {
    /// The run ended. `prelude` is its terminal batch (FIG-3571): the
    /// execution-owned events the run still owes the log (its pending
    /// effect-summary occurrences, then its `process.effect_omissions`
    /// record), which the runner commits ahead of the terminal event in the
    /// completion's own transaction
    /// ([`ProcessLifecycle::complete_process_with_prelude`](super::registry_concerns::ProcessLifecycle::complete_process_with_prelude)).
    Terminal {
        output: Box<ProcessAwaitOutput>,
        prelude: Vec<super::events::ProcessEventAppendRequest>,
    },
    SegmentBoundary(SegmentHandover),
}

impl ProcessRunOutcome {
    /// This is an **integrator class 3: process-engine implementor** seam.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminal { .. })
    }

    /// Borrow the terminal output.
    ///
    /// This is an **integrator class 3: process-engine implementor** seam.
    pub fn terminal_output(&self) -> Option<&ProcessAwaitOutput> {
        match self {
            Self::Terminal { output, .. } => Some(output),
            Self::SegmentBoundary(_) => None,
        }
    }
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

    pub fn into_plugin_error(self) -> crate::PluginError {
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
        Self::Terminal {
            output: Box::new(output),
            prelude: Vec::new(),
        }
    }
}

pub type ProcessEngineShutdownFuture<'run> =
    Pin<Box<dyn Future<Output = Result<(), crate::PluginError>> + Send + 'run>>;

pub struct ProcessEngineRunGuard<'run> {
    shutdown: Option<Box<dyn FnOnce(bool) -> ProcessEngineShutdownFuture<'run> + Send + 'run>>,
}

impl<'run> ProcessEngineRunGuard<'run> {
    pub fn new(
        shutdown: impl FnOnce(bool) -> ProcessEngineShutdownFuture<'run> + Send + 'run,
    ) -> Self {
        Self {
            shutdown: Some(Box::new(shutdown)),
        }
    }

    pub async fn shutdown(mut self, parent_ended: bool) -> Result<(), crate::PluginError> {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown(parent_ended).await?;
        }
        Ok(())
    }
}

pub struct ProcessEngineRuntimeContext<'run> {
    context: crate::RuntimeExecutionContext<'run>,
    guard: ProcessEngineRunGuard<'run>,
}

impl<'run> ProcessEngineRuntimeContext<'run> {
    pub fn new(
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

    pub async fn shutdown(self, parent_ended: bool) -> Result<(), crate::PluginError> {
        self.guard.shutdown(parent_ended).await
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
    process_id: ProcessId,
    process_work: crate::ProcessWorkWiring,
    execution_write_authority: super::model::ProcessExecutionWriteAuthority,
    store: Option<Arc<dyn crate::RuntimePersistence>>,
    session_store_factory: Option<Arc<dyn crate::SessionStoreFactory>>,
    queued_work: Arc<dyn crate::SessionWorkEngine>,
    process_wake_delivery_policy: crate::DeliveryPolicy,
    clock: Arc<dyn crate::Clock>,
}

impl ProcessEngineProcessContext {
    #[allow(clippy::too_many_arguments)]
    fn new(
        process_id: ProcessId,
        process_work: crate::ProcessWorkWiring,
        execution_write_authority: super::model::ProcessExecutionWriteAuthority,
        store: Option<Arc<dyn crate::RuntimePersistence>>,
        session_store_factory: Option<Arc<dyn crate::SessionStoreFactory>>,
        queued_work: Arc<dyn crate::SessionWorkEngine>,
        process_wake_delivery_policy: crate::DeliveryPolicy,
        clock: Arc<dyn crate::Clock>,
    ) -> Self {
        Self {
            process_id,
            process_work,
            execution_write_authority,
            store,
            session_store_factory,
            queued_work,
            process_wake_delivery_policy,
            clock,
        }
    }

    pub async fn record(&self) -> Result<Option<super::model::ProcessRecord>, crate::PluginError> {
        self.process_work
            .registry()
            .get_process(&self.process_id)
            .await
    }

    /// Read a page of this run's own process events strictly after
    /// `after_sequence`.
    pub async fn event_page(
        &self,
        after_sequence: u64,
        limit: std::num::NonZeroUsize,
        mode: super::events::ProcessEventQueryMode,
    ) -> Result<
        super::events::ProcessEventReadOutcome<super::events::ProcessEventPage>,
        crate::PluginError,
    > {
        self.process_work
            .registry()
            .event_page_after(&self.process_id, after_sequence, limit, mode)
            .await
    }

    pub async fn emit(
        &self,
        request: super::events::ProcessEventAppendRequest,
    ) -> Result<super::events::ProcessEvent, crate::PluginError> {
        let result = self
            .process_work
            .registry()
            .append_event_with_authority(&self.process_id, request, &self.execution_write_authority)
            .await?;
        crate::tool_provider::process_events::enqueue_wake_delivery(
            Arc::clone(self.process_work.registry()),
            self.store.clone(),
            self.session_store_factory.as_ref(),
            result.wake_delivery,
            None,
            Arc::clone(&self.queued_work),
            self.process_wake_delivery_policy,
            Arc::clone(&self.clock),
        )
        .await?;
        Ok(result.event)
    }

    /// Enter `wait` as a run boundary, committing the run's pending
    /// `prelude` in the same transaction (FIG-3571).
    pub async fn set_wait(
        &self,
        wait: super::model::WaitState,
        prelude: Vec<super::events::ProcessEventAppendRequest>,
    ) -> Result<super::model::ProcessRecord, crate::PluginError> {
        self.process_work
            .registry()
            .set_process_wait_with_authority(
                &self.process_id,
                wait,
                prelude,
                &self.execution_write_authority,
            )
            .await
    }

    /// Leave the current wait as a run boundary, committing the run's
    /// pending `prelude` in the same transaction (FIG-3571).
    pub async fn clear_wait(
        &self,
        prelude: Vec<super::events::ProcessEventAppendRequest>,
    ) -> Result<super::model::ProcessRecord, crate::PluginError> {
        self.process_work
            .registry()
            .clear_process_wait_with_authority(
                &self.process_id,
                prelude,
                &self.execution_write_authority,
            )
            .await
    }

    pub async fn await_terminal(
        &self,
        process_id: &ProcessId,
    ) -> Result<ProcessAwaitOutput, crate::PluginError> {
        loop {
            match self
                .process_work
                .port()
                .await_process_terminal(process_id)
                .await?
            {
                crate::ProcessTerminalWait::Terminal(output) => return Ok(output),
                crate::ProcessTerminalWait::Reattach => continue,
            }
        }
    }
}

pub struct ProcessEngineRunContext<'run> {
    registration: ProcessRegistration,
    /// The minted id of the process this run executes: the opener an engine
    /// mints identities against (ADR 0099 §1, ADR 0107).
    process_id: ProcessId,
    execution_context: ProcessExecutionContext,
    processes: ProcessEngineProcessContext,
    session_id: SessionId,
    plugins: Arc<crate::PluginSession>,
    tool_catalog: Arc<crate::ToolCatalog>,
    store: Option<Arc<dyn crate::RuntimePersistence>>,
    session_store_factory: Option<Arc<dyn crate::SessionStoreFactory>>,
    queued_work: Arc<dyn crate::SessionWorkEngine>,
    process_registry_available: bool,
    cancellation: CancellationToken,
    turn_phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
    scoped_effect_controller: crate::ScopedEffectController<'run>,
    handover: Option<SegmentHandover>,
    runtime_context_builder: Option<RuntimeContextBuilder<'run>>,
}

impl<'run> ProcessEngineRunContext<'run> {
    #[allow(clippy::too_many_arguments)]
    #[expect(
        clippy::expect_used,
        reason = "the process worker installs the write authority"
    )]
    pub fn new(
        registration: ProcessRegistration,
        process_id: ProcessId,
        execution_context: ProcessExecutionContext,
        process_work: crate::ProcessWorkWiring,
        session_id: SessionId,
        plugins: Arc<crate::PluginSession>,
        tool_catalog: Arc<crate::ToolCatalog>,
        store: Option<Arc<dyn crate::RuntimePersistence>>,
        session_store_factory: Option<Arc<dyn crate::SessionStoreFactory>>,
        queued_work: Arc<dyn crate::SessionWorkEngine>,
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
            process_id.clone(),
            process_work,
            execution_write_authority,
            store.clone(),
            session_store_factory.clone(),
            Arc::clone(&queued_work),
            process_wake_delivery_policy,
            clock,
        );
        Self {
            registration,
            process_id,
            execution_context,
            processes,
            session_id,
            plugins,
            tool_catalog,
            store,
            session_store_factory,
            queued_work,
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

    /// The minted id of the process this run executes: the logical opener
    /// (ADR 0099 §1).
    pub fn process_id(&self) -> &ProcessId {
        &self.process_id
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

    /// Exposes store to protocol and process-engine implementors while running a durable
    /// process.
    pub fn store(&self) -> Option<Arc<dyn crate::RuntimePersistence>> {
        self.store.clone()
    }

    /// Exposes session store factory to protocol and process-engine implementors while running
    /// a durable process.
    pub fn session_store_factory(&self) -> Option<Arc<dyn crate::SessionStoreFactory>> {
        self.session_store_factory.clone()
    }

    /// Exposes the required queued-work port to process-engine implementors.
    pub fn queued_work(&self) -> Arc<dyn crate::SessionWorkEngine> {
        Arc::clone(&self.queued_work)
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

    pub fn named_phase(&self, phase: &'static str) -> crate::runtime::RuntimeNamedPhase {
        crate::runtime::RuntimeNamedPhase::begin(self.turn_phase_probe.clone(), phase)
    }

    pub fn turn_phase_probe(&self) -> Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>> {
        self.turn_phase_probe.clone()
    }

    /// Process-engine implementors must pass this `Arc` (or an `Arc::clone` of it) to
    /// [`Self::into_runtime_context`].
    pub fn resolved_tool_catalog(&self) -> Result<Arc<crate::ToolCatalog>, crate::PluginError> {
        Ok(Arc::clone(&self.tool_catalog))
    }

    /// Extracts the runtime context using the catalog captured for this process execution.
    ///
    /// `tool_catalog` must be the `Arc` returned by [`Self::resolved_tool_catalog`]; this keeps
    /// definitions and execution routes on the same immutable resident snapshot.
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

/// Settle one engine's staged start artifacts onto the owner of the process a
/// start has just registered.
///
/// The staging owner is stable per process id and therefore shared by every
/// concurrent attempt at the same start, so the transfer can find its source
/// edge already severed by another attempt. Engine start artifacts are named by
/// the same immutable payload the registration carries, so protecting them under
/// the process owner reaches exactly the state the transfer would have left
/// (FIG-3090). The engine's own refusal is untouched.
pub async fn settle_started_process_engine_artifacts(
    engine: &dyn ProcessEngine,
    staging_owner: &crate::ArtifactOwner,
    process_owner: &crate::ArtifactOwner,
    payload: &serde_json::Value,
    staged: bool,
) -> Result<(), crate::PluginError> {
    if !staged {
        return engine.protect_start_artifacts(process_owner, payload).await;
    }
    match engine
        .transfer_start_artifacts(staging_owner, process_owner, payload)
        .await
    {
        Ok(()) => {}
        Err(error) if crate::artifact_staging_owner_edge_is_missing(&error) => {
            engine
                .protect_start_artifacts(process_owner, payload)
                .await?;
        }
        Err(error) => return Err(error),
    }
    engine.retire_artifact_owner(staging_owner).await
}

#[async_trait::async_trait]
/// Deployment extension point for non-kernel process runtimes.
///
/// Core built-ins (`ToolCall`, `SessionTurn`, and `External`) are intentionally not registered
/// here; they are kernel primitives with direct orchestration support.
pub trait ProcessEngine: Send + Sync {
    fn kind(&self) -> &'static str;

    /// The executable generation a run of `payload` would run as (FIG-3571):
    /// the value the incarnation's start record carries and every later
    /// attempt and segment must match before its first step. `None` for an
    /// engine whose runs carry no generation, or for a payload the engine
    /// refuses anyway.
    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<crate::ExecutableGeneration> {
        None
    }

    async fn run(
        &self,
        context: ProcessEngineRunContext<'_>,
        payload: serde_json::Value,
    ) -> Result<ProcessRunOutcome, ProcessInfraError>;

    /// Protect artifacts named by a start payload under its replayable staging
    /// owner before process registration.
    async fn protect_start_artifacts(
        &self,
        _owner: &crate::ArtifactOwner,
        _payload: &serde_json::Value,
    ) -> Result<(), crate::PluginError> {
        Ok(())
    }

    /// Atomically transfer start-time artifact protection to the registered
    /// process owner. Implementations must make replay idempotent.
    async fn transfer_start_artifacts(
        &self,
        _from: &crate::ArtifactOwner,
        _to: &crate::ArtifactOwner,
        _payload: &serde_json::Value,
    ) -> Result<(), crate::PluginError> {
        Ok(())
    }

    /// Release artifacts named by a payload for one exact owner.
    async fn release_artifacts(
        &self,
        _owner: &crate::ArtifactOwner,
        _payload: &serde_json::Value,
    ) -> Result<(), crate::PluginError> {
        Ok(())
    }

    /// Permanently retire an execution owner and reclaim its untransferred
    /// artifacts.
    async fn retire_artifact_owner(
        &self,
        _owner: &crate::ArtifactOwner,
    ) -> Result<(), crate::PluginError> {
        Ok(())
    }

    /// Answer what this engine's stored artifact says about a definition
    /// reference: its authoritative signature and the signal event types the
    /// definition declares.
    ///
    /// The signature travelling on the reference is a **claim**. This method
    /// never reads it; the registry compares the claim against what is returned
    /// here and refuses a disagreement before any durable row exists. An engine
    /// that stores no artifacts leaves the default, which asserts *unknown*
    /// rather than rubber-stamping the claim: a reference that claims a
    /// signature such an engine cannot vouch for is refused, and an unclaimed
    /// one is admitted as unknown.
    ///
    /// This is an **integrator class 3: process-engine implementor** seam.
    async fn resolve(
        &self,
        reference: &ProcessDefinitionRef,
    ) -> Result<ProcessDefinitionResolution, ProcessDefinitionRefusal> {
        let _ = reference;
        Ok(ProcessDefinitionResolution::new(
            ProcessSignature::Unknown,
            Vec::new(),
        ))
    }
}

/// A process identity the engine registry produced, and the signal event types
/// that came with it.
///
/// This is the only way an identity reaches a [`ProcessRegistration`]. A
/// definition reference can therefore only appear on a durable row if the
/// engine that owns the definition resolved it and agreed with the signature
/// the reference claimed: a fabricated claim is refused before the row exists,
/// and no caller can construct this value carrying one.
#[derive(Clone, Debug, PartialEq)]
pub struct AdmittedProcessIdentity {
    identity: ProcessIdentity,
    signals: Vec<ProcessEventType>,
}

impl AdmittedProcessIdentity {
    pub(crate) fn admitted(identity: ProcessIdentity, signals: Vec<ProcessEventType>) -> Self {
        Self { identity, signals }
    }

    /// Replay an identity that was admitted once and then durably pinned: a
    /// trigger subscription's recorded target identity (ADR 0095 — a delivery
    /// fires the definition pinned at registration and never re-resolves it),
    /// or a process row a remote peer already created and is now reporting.
    ///
    /// This is a **replay**, never an admission. Do not reach for it on a path
    /// that creates a durable row from caller-supplied input: there the
    /// registry's [`admit`](ProcessEngineRegistry::admit) is the only route,
    /// because it is what checks a signature claim.
    pub fn pinned(identity: ProcessIdentity) -> Self {
        Self {
            identity,
            signals: Vec::new(),
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn for_testing(identity: ProcessIdentity) -> Self {
        Self {
            identity,
            signals: Vec::new(),
        }
    }

    /// Borrow the admitted identity.
    pub fn identity(&self) -> &ProcessIdentity {
        &self.identity
    }

    /// Borrow the signal event types the engine resolved for this definition.
    pub fn signals(&self) -> &[ProcessEventType] {
        &self.signals
    }

    pub fn into_parts(self) -> (ProcessIdentity, Vec<ProcessEventType>) {
        (self.identity, self.signals)
    }
}

/// Pure admission policy for immutable, recorded engine-start inputs.
///
/// The function pointer cannot capture an engine, store, catalog, or session.
#[derive(Clone, Copy)]
pub struct ProcessEngineAdmission {
    kind: &'static str,
    admit: fn(
        &'static str,
        &serde_json::Value,
        Option<&ProcessExecutionEnvSpec>,
    ) -> Result<ProcessIdentity, crate::PluginError>,
}

impl ProcessEngineAdmission {
    pub const fn new(
        kind: &'static str,
        admit: fn(
            &'static str,
            &serde_json::Value,
            Option<&ProcessExecutionEnvSpec>,
        ) -> Result<ProcessIdentity, crate::PluginError>,
    ) -> Self {
        Self { kind, admit }
    }

    pub const fn accepting(kind: &'static str) -> Self {
        Self::new(kind, |kind, _, _| Ok(ProcessIdentity::new(kind)))
    }

    pub fn kind(self) -> &'static str {
        self.kind
    }

    pub fn admit(
        self,
        payload: &serde_json::Value,
        env_spec: Option<&ProcessExecutionEnvSpec>,
    ) -> Result<ProcessIdentity, crate::PluginError> {
        (self.admit)(self.kind, payload, env_spec)
    }
}

#[derive(Clone)]
pub struct ProcessEngineRegistration {
    engine: Arc<dyn ProcessEngine>,
    admission: ProcessEngineAdmission,
}

impl ProcessEngineRegistration {
    pub fn new(
        engine: Arc<dyn ProcessEngine>,
        admission: ProcessEngineAdmission,
    ) -> Result<Self, crate::PluginError> {
        if engine.kind() != admission.kind() {
            return Err(crate::PluginError::Registration(format!(
                "process engine kind `{}` does not match admission kind `{}`",
                engine.kind(),
                admission.kind()
            )));
        }
        Ok(Self { engine, admission })
    }

    /// Pair an engine with the default recorded-input admission policy.
    pub fn accepting(engine: Arc<dyn ProcessEngine>) -> Self {
        let admission = ProcessEngineAdmission::accepting(engine.kind());
        Self { engine, admission }
    }
}

#[derive(Clone, Default)]
pub struct ProcessEngineRegistry {
    engines: Arc<BTreeMap<String, Arc<dyn ProcessEngine>>>,
    admissions: Arc<BTreeMap<String, ProcessEngineAdmission>>,
}

impl ProcessEngineRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_registration(self, registration: ProcessEngineRegistration) -> Self {
        let mut engines = (*self.engines).clone();
        let mut admissions = (*self.admissions).clone();
        let ProcessEngineRegistration { engine, admission } = registration;
        engines.insert(engine.kind().to_string(), engine);
        admissions.insert(admission.kind().to_string(), admission);
        Self {
            engines: Arc::new(engines),
            admissions: Arc::new(admissions),
        }
    }

    /// Retire an execution artifact owner across every installed engine.
    pub async fn retire_artifact_owner(
        &self,
        owner: &crate::ArtifactOwner,
    ) -> Result<(), crate::PluginError> {
        for engine in self.engines.values() {
            engine.retire_artifact_owner(owner).await?;
        }
        Ok(())
    }

    /// Release engine artifacts retained by one pruned process record.
    pub async fn release_process_artifacts(
        &self,
        record: &crate::ProcessRecord,
    ) -> Result<(), crate::PluginError> {
        let crate::ProcessInput::Engine { kind, payload } = record.input.as_ref() else {
            return Ok(());
        };
        self.require(kind)?
            .release_artifacts(&crate::ArtifactOwner::process(record.id.clone()), payload)
            .await
    }

    /// Release engine artifacts from the durable evidence Process Prune left
    /// after deleting the full process row.
    pub async fn release_pruned_process_artifacts(
        &self,
        cleanup: &crate::ProcessArtifactCleanup,
    ) -> Result<(), crate::PluginError> {
        let crate::ProcessInput::Engine { kind, payload } = &*cleanup.input else {
            return Ok(());
        };
        self.require(kind)?
            .release_artifacts(
                &crate::ArtifactOwner::process(cleanup.process_id.clone()),
                payload,
            )
            .await
    }

    /// This is the single enforcement point for unique engine kinds across everything
    /// registered on a runtime host, whether the engine was wired directly or contributed
    /// through the plugin contract.
    pub(crate) fn try_with_engine(
        self,
        registration: ProcessEngineRegistration,
    ) -> Result<Self, crate::PluginError> {
        if self.engines.contains_key(registration.engine.kind()) {
            return Err(crate::PluginError::Registration(format!(
                "duplicate process engine kind `{}`; each engine kind may be registered once",
                registration.engine.kind()
            )));
        }
        Ok(self.with_registration(registration))
    }

    pub(crate) fn get(&self, kind: &str) -> Option<Arc<dyn ProcessEngine>> {
        self.engines.get(kind).cloned()
    }

    /// Resolve the engine a `ProcessInput::Engine` start names, refusing a kind
    /// this host never registered.
    ///
    pub fn require(&self, kind: &str) -> Result<Arc<dyn ProcessEngine>, crate::PluginError> {
        self.get(kind).ok_or_else(|| {
            crate::PluginError::Session(format!("process engine `{kind}` is not configured"))
        })
    }

    /// Admit a start using only immutable recorded inputs, then verify every
    /// definition reference the admission derived against the owning engine.
    ///
    /// This is the single boundary at which a signature claim is checked. The
    /// admission policy is pure and cannot read an artifact, so it derives the
    /// reference the recorded payload names; the engine then says what that
    /// definition's signature actually is, and a disagreement refuses the start
    /// before any durable row exists.
    pub async fn admit(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        env_spec: Option<&ProcessExecutionEnvSpec>,
    ) -> Result<AdmittedProcessIdentity, crate::PluginError> {
        let identity = self
            .admissions
            .get(kind)
            .copied()
            .ok_or_else(|| {
                crate::PluginError::Session(format!("process engine `{kind}` is not configured"))
            })?
            .admit(payload, env_spec)?;
        let Some(reference) = identity.definition.clone() else {
            return Ok(AdmittedProcessIdentity::admitted(identity, Vec::new()));
        };
        let resolution = match self.resolve(&reference).await {
            Ok(resolution) => resolution,
            // An engine that cannot read its definition right now has said
            // nothing about the claim. Start admission is a recorded-input
            // decision (FIG-1838/FIG-1521): a live artifact-store outage belongs
            // to retryable execution after the row is durable, so an *unclaimed*
            // reference is admitted unresolved rather than refused for the
            // lifetime of the intent. A reference that does claim a signature is
            // still refused — an unverifiable claim never creates a row.
            Err(ProcessDefinitionRefusal::UnresolvableDefinition { .. })
                if reference.signature.is_unknown() =>
            {
                return Ok(AdmittedProcessIdentity::admitted(identity, Vec::new()));
            }
            Err(refusal) => return Err(refusal.into()),
        };
        // The durable row pins the engine's authority, not the claim that
        // arrived: they are equal by the check above when a claim was made, and
        // an unclaimed reference adopts the artifact's signature here rather
        // than recording "unknown" forever.
        let identity = ProcessIdentity {
            definition: Some(reference.with_resolved_signature(resolution.signature)),
            ..identity
        };
        Ok(AdmittedProcessIdentity::admitted(
            identity,
            resolution.signals,
        ))
    }

    pub async fn resolve(
        &self,
        reference: &ProcessDefinitionRef,
    ) -> Result<ProcessDefinitionResolution, ProcessDefinitionRefusal> {
        let engine = self.get(reference.engine_kind.as_str()).ok_or_else(|| {
            ProcessDefinitionRefusal::UnknownEngine {
                engine_kind: reference.engine_kind.clone(),
            }
        })?;
        let resolution = engine.resolve(reference).await?;
        // An unknown claim asserts nothing and adopts the authority. A known
        // claim must be the authority exactly: this is the single point where a
        // fabricated signature is refused, and it runs before the row exists.
        if !reference.signature.is_unknown() && resolution.signature != reference.signature {
            return Err(ProcessDefinitionRefusal::SignatureMismatch {
                engine_kind: reference.engine_kind.clone(),
                claimed: reference.signature.clone(),
                authoritative: resolution.signature,
            });
        }
        Ok(resolution)
    }
}

#[cfg(test)]
#[path = "engine_resolve_tests.rs"]
mod resolve_tests;
