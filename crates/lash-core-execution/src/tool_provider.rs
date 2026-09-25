use crate::ModelGenerationClamp;
use crate::ProcessId;
use crate::SessionId;
pub(crate) use completion_support::AttemptCompletionSupport;
use std::sync::{Arc, Mutex};

use lash_sansio::llm::types::ProviderReplayMeta;
use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};

use crate::plugin::{
    PluginError, SessionGraphService, SessionLifecycleService, SessionSnapshot, SessionStateService,
};
use crate::{ToolContract, ToolDefinition, ToolId, ToolManifest, ToolOutcome};

mod attachments;
mod completion_support;
mod direct_completion;
mod dispatch;
pub mod orchestration;
mod process;
pub mod process_events;
mod session;
mod triggers;

pub use attachments::ToolAttachmentClient;
pub use direct_completion::ToolDirectCompletionClient;
pub use dispatch::ToolDispatchClient;
pub use process::{
    ExternalLaunchAudit, InternalProcessAdmin, InternalProcessContext, InternalProcessToolCall,
    InternalProcessToolDef, InternalProcessToolImplementation,
};
pub use process_events::ToolProcessEventClient;
pub use session::{ToolSessionAdmin, ToolSessionModel};
pub use triggers::ToolTriggerClient;

/// Integrator class 3 session reads available inside a recorded leaf attempt.
#[derive(Clone)]
pub struct AttemptSessionReads {
    session_id: SessionId,
    sessions: Arc<dyn SessionStateService>,
}

impl AttemptSessionReads {
    /// Integrator class 3 read of the attempt session's effective model policy.
    pub async fn model(&self) -> Result<session::ToolSessionModel, PluginError> {
        let snapshot = self.snapshot_current().await?;
        let generation = snapshot
            .policy
            .model
            .clamped_generation(&snapshot.policy.generation);
        Ok(session::ToolSessionModel {
            model: snapshot.policy.model.id,
            model_variant: snapshot.policy.model.variant,
            model_capability: snapshot.policy.model.capability,
            generation,
        })
    }

    /// Integrator class 3 snapshot of the bound session without an effect controller.
    pub async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
        self.sessions.snapshot_session(&self.session_id).await
    }

    /// Integrator class 3 snapshot of a named session through controller-free reads.
    pub async fn snapshot(
        &self,
        session_id: impl AsRef<str>,
    ) -> Result<SessionSnapshot, PluginError> {
        self.sessions
            .snapshot_session(&SessionId::from(session_id.as_ref()))
            .await
    }

    /// Integrator class 3 read of the bound session's serialized tool catalog.
    pub async fn tool_catalog(&self) -> Result<Vec<serde_json::Value>, PluginError> {
        self.sessions.tool_catalog(&self.session_id).await
    }

    /// Integrator class 3 shared read of the immutable serialized tool catalog.
    pub async fn shared_tool_catalog(&self) -> Result<Arc<Vec<serde_json::Value>>, PluginError> {
        self.sessions.shared_tool_catalog(&self.session_id).await
    }
}

/// Integrator class 3 controller-free process reads for a recorded leaf attempt.
#[derive(Clone)]
pub struct AttemptProcessReads {
    session_id: SessionId,
    processes: Arc<dyn crate::ProcessService>,
}

impl AttemptProcessReads {
    /// Integrator class 3 listing through the attempt-safe process filter.
    pub async fn list_handles_filtered(
        &self,
        filter: &crate::ProcessListFilter,
    ) -> Result<Vec<crate::ProcessHandleView>, PluginError> {
        Ok(self
            .processes
            .list_visible_for_attempt(&self.session_id, filter.list_mode())
            .await?
            .into_iter()
            .filter(|record| filter.matches_record(record))
            .map(crate::ProcessHandleView::from_record)
            .collect())
    }
}

/// Runtime-only route selected by the dispatcher for one authorized call.
///
/// The route is deliberately private to the core so provider authors observe
/// only the ordinary prepare/execute APIs and their existing execution
/// binding. Registry-backed granted calls still need their live source id,
/// however, because that source is intentionally outside the pinned catalog.
#[derive(Clone, Default)]
pub(crate) enum ToolExecutionRoute {
    #[default]
    Catalog,
    Granted {
        source_id: Option<String>,
    },
}

/// The logical root an admitted scope runs under: a turn scope is its root's
/// own; a queue drain names its queued root by its drain id until queued roots
/// run under turn scopes (FIG-3600 S8).
fn logical_root_of(scope: &crate::ExecutionScope) -> Option<crate::TurnId> {
    match scope {
        crate::ExecutionScope::Turn { turn_id, .. } => Some(turn_id.clone()),
        crate::ExecutionScope::QueueDrain { drain_id, .. } => {
            Some(crate::TurnId::from(drain_id.as_str()))
        }
        crate::ExecutionScope::Process { .. }
        | crate::ExecutionScope::SessionDelete { .. }
        | crate::ExecutionScope::RuntimeOperation { .. } => None,
    }
}

/// Integrator class 3 sealed, controller-free environment for a recorded leaf attempt.
#[derive(Clone)]
pub struct AttemptContext<'run> {
    session_id: SessionId,
    /// The runtime-owned parent scope for a child lifecycle declaration —
    /// the admitted pair the enclosing execution runs under, which is the
    /// whole input to the one owner derivation —
    /// [`crate::EffectOpener::for_scope`] — and it is never re-resolved by
    /// name (ADR 0099 §1, FIG-3417).
    parent_scope: crate::AdmittedScope,
    execution_scope_id: String,
    agent_frame_id: crate::FrameNodeId,
    sessions: AttemptSessionReads,
    processes: AttemptProcessReads,
    cancellation_token: Option<tokio_util::sync::CancellationToken>,
    /// The process this attempt executes inside, resolved once at context
    /// construction. `ToolContext` carries the same single fact.
    enclosing_process: Option<ProcessId>,
    attachment_store: Arc<crate::SessionAttachmentStore>,
    /// The dispatch-bound direct-completion client. `pub(crate)` so the
    /// attempt-atomicity laws can reach the *raw* client and prove the binding
    /// travels with it rather than with the accessor.
    pub direct_completions: crate::DirectCompletionClient<'run>,
    /// The recorded attempt this leaf body runs inside. Carried so
    /// attempt-attributed capabilities classify their journal position exactly
    /// as the legacy [`ToolContext`] path does. Boxed because this context is
    /// captured by the deep tool-dispatch futures.
    parent_invocation: Option<Box<crate::RuntimeInvocation>>,
    provider: Option<crate::ProviderHandle>,
    prepared_payload: serde_json::Value,
    tool_execution_binding: serde_json::Value,
    tool_call_id: Option<String>,
    attempt_number: u32,
    max_attempts: u32,
    /// The attempt bound this host stamps onto a child a leaf body declares.
    /// `None` where the attempt runs with no runtime execution context to read
    /// it from, which is the same case that leaves the bound unset today.
    engine_child_max_attempts: Option<std::num::NonZeroU32>,
    /// The provenance a child this body declares inherits when the attempt is
    /// running inside a durable process. `None` where the attempt is not
    /// running inside one, and the child takes the declaring session's own.
    process_spawn_provenance: Option<crate::ProcessSpawnProvenance>,
    replay_key: Option<String>,
    execution_env_spec: crate::ProcessExecutionEnvSpec,
    completion_key: Option<crate::AwaitEventKey>,
    completion_support: AttemptCompletionSupport,
    phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
    tool_execution_route: ToolExecutionRoute,
}

impl<'run> AttemptContext<'run> {
    /// The logical root this attempt runs under, read from the admitted
    /// scope it was recorded in (FIG-3607 item 6): never a live read. `None`
    /// outside a session turn (a process body, a runtime operation).
    pub fn logical_root(&self) -> Option<crate::TurnId> {
        logical_root_of(self.parent_scope.scope())
    }

    /// The runtime-owned parent scope for an explicit child lifecycle
    /// declaration.
    ///
    /// Derived through the one owner derivation — the admitted scope the
    /// enclosing execution runs under — so a same-name successor in the
    /// registry cannot rebind a child this attempt's opener still owns
    /// (FIG-3417).
    pub fn child_process_parent_scope(&self) -> Result<crate::ParentScope, PluginError> {
        let opener = crate::EffectOpener::for_scope(&self.parent_scope)
            .map_err(|error| PluginError::Session(error.to_string()))?;
        Ok(crate::ParentScope::from_owner(&opener))
    }

    pub(crate) fn from_tool_context(
        context: &ToolContext<'run>,
        execution_scope_id: String,
        completion_key: Option<crate::AwaitEventKey>,
        completion_support: AttemptCompletionSupport,
    ) -> Self {
        let phase_probe = context
            .runtime_execution_context
            .as_ref()
            .and_then(crate::RuntimeExecutionContext::attempt_phase_probe);
        let provider = context
            .runtime_dispatch
            .as_ref()
            .and_then(|dispatch| dispatch.turn_context.provider().cloned());
        Self {
            parent_scope: context.effect_controller.scoped().admitted_scope().clone(),
            session_id: context.session_id.clone(),
            execution_scope_id,
            agent_frame_id: context.agent_frame_id.clone(),
            sessions: AttemptSessionReads {
                session_id: context.session_id.clone(),
                sessions: Arc::clone(&context.sessions),
            },
            processes: AttemptProcessReads {
                session_id: context.session_id.clone(),
                processes: Arc::clone(&context.processes),
            },
            cancellation_token: context.cancellation_token.clone(),
            enclosing_process: context.enclosing_process.clone(),
            attachment_store: Arc::clone(&context.attachment_store),
            direct_completions: context.direct_completions.clone(),
            parent_invocation: context.parent_invocation.clone().map(Box::new),
            provider,
            prepared_payload: context.prepared_payload.clone(),
            tool_execution_binding: context.tool_execution_binding.clone(),
            tool_call_id: context.tool_call_id.clone(),
            attempt_number: context.attempt_number,
            max_attempts: context.max_attempts,
            engine_child_max_attempts: context
                .runtime_execution_context
                .as_ref()
                .map(crate::RuntimeExecutionContext::engine_child_max_attempts),
            process_spawn_provenance: context
                .runtime_execution_context
                .as_ref()
                .and_then(|runtime| runtime.process_spawn_provenance()),
            replay_key: context.replay_key.clone(),
            execution_env_spec: context.execution_env_spec.clone(),
            completion_key,
            completion_support,
            phase_probe,
            tool_execution_route: context.tool_execution_route.clone(),
        }
    }

    pub(crate) fn execution_route(&self) -> &ToolExecutionRoute {
        &self.tool_execution_route
    }

    /// Integrator class 3 identity for the session that owns this recorded attempt.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    /// Integrator class 3 durable turn or process scope used for intent identity.
    pub fn execution_scope_id(&self) -> &str {
        &self.execution_scope_id
    }
    /// Integrator class 3 agent-frame identity that authorized this provider attempt.
    pub fn agent_frame_id(&self) -> &crate::FrameNodeId {
        &self.agent_frame_id
    }
    /// Integrator class 3 controller-free session reads for this attempt.
    pub fn sessions(&self) -> AttemptSessionReads {
        self.sessions.clone()
    }
    /// Integrator class 3 controller-free process reads for this attempt.
    pub fn processes(&self) -> AttemptProcessReads {
        self.processes.clone()
    }
    /// Integrator class 3 cooperative cancellation token supplied by the attempt host.
    pub fn cancellation_token(&self) -> Option<&tokio_util::sync::CancellationToken> {
        self.cancellation_token.as_ref()
    }
    /// Integrator class 3 process this attempt executes inside, if any.
    pub fn enclosing_process(&self) -> Option<&str> {
        self.enclosing_process.as_deref()
    }
    /// Integrator class 3 attachment capability for durable tool output.
    pub fn attachments(&self) -> ToolAttachmentClient {
        ToolAttachmentClient {
            store: Arc::clone(&self.attachment_store),
        }
    }
    /// Integrator class 3 direct-completion client attributed to this attempt call.
    ///
    /// The attempt invocation travels with the client so the completion runs on
    /// the journal-free branch: the controller already owns one entry for this
    /// whole attempt, and a second entry emitted from inside the body would be
    /// left unre-issued by redrive.
    pub fn direct_completions(&self) -> ToolDirectCompletionClient<'run> {
        ToolDirectCompletionClient {
            session_id: self.session_id.clone(),
            tool_call_id: self.tool_call_id.clone(),
            direct_completions: self.direct_completions.clone(),
            parent_invocation: self.parent_invocation.as_deref().cloned(),
        }
    }
    /// Integrator class 3 resolved model provider visible to the attempt host.
    pub fn provider(&self) -> Option<&crate::ProviderHandle> {
        self.provider.as_ref()
    }
    /// Integrator class 3 payload sealed by the provider's prepare phase.
    pub fn prepared_payload(&self) -> &serde_json::Value {
        &self.prepared_payload
    }
    /// Integrator class 3 protocol-owned execution binding for this tool call.
    pub fn tool_execution_binding(&self) -> &serde_json::Value {
        &self.tool_execution_binding
    }
    /// Integrator class 3 stable provider call id used to derive intent identities.
    pub fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id.as_deref()
    }
    /// Integrator class 3 one-based retry attempt number.
    pub fn attempt_number(&self) -> u32 {
        self.attempt_number
    }
    /// Integrator class 3 retry ceiling sealed for this invocation.
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }
    /// The attempt bound the host stamps onto a child this body declares.
    ///
    /// A declaring start is the only caller: the bound lives on the runtime
    /// execution context, and before FIG-2999 only the in-attempt start path
    /// could read it, so a leaf-tool start registered its child with no bound
    /// at all and a deterministically failing child retried forever. `None`
    /// means there is no runtime context to read, and the child registers
    /// engine-paced exactly as it did before.
    pub fn engine_child_max_attempts(&self) -> Option<std::num::NonZeroU32> {
        self.engine_child_max_attempts
    }
    /// The provenance a child declared by this body inherits.
    ///
    /// A process's children belong to the chain that started the process, not
    /// to the ephemeral session its run executes in: they carry the chain's
    /// originator and its wake target, and the execution scope appears on no
    /// record. The in-attempt start path has always read this off the runtime
    /// execution context (`ProcessStartOptions::spawn_provenance`); a leaf
    /// start declares its child before realization, so it reads the same fact
    /// here and stamps it on the declaration. `None` means the attempt is not
    /// running inside a process and the child takes the declaring session's
    /// own originator.
    pub fn process_spawn_provenance(&self) -> Option<&crate::ProcessSpawnProvenance> {
        self.process_spawn_provenance.as_ref()
    }
    /// Integrator class 3 durable attempt replay key when supplied by the host.
    pub fn replay_key(&self) -> Option<&str> {
        self.replay_key.as_deref()
    }
    /// This accessor is part of ADR 0051's protocol and process-engine
    /// implementor class: a leaf [`ToolProvider`] declaring `StartProcess`
    /// must copy the captured environment into the durable request instead of
    /// rebuilding it from mutable host state.
    pub fn process_execution_env_spec(&self) -> crate::ProcessExecutionEnvSpec {
        self.execution_env_spec.clone()
    }
    /// Integrator class 3 decode of the sealed payload into a provider-owned type.
    pub fn decode_prepared_payload<T>(&self) -> Result<T, serde_json::Error>
    where
        T: serde::de::DeserializeOwned,
    {
        serde_json::from_value(self.prepared_payload.clone())
    }
    /// Integrator class 3 named, attempt-attributed runtime phase for fault probes.
    pub fn named_phase(&self, phase: &'static str) -> crate::runtime::RuntimeNamedPhase {
        crate::runtime::RuntimeNamedPhase::begin(self.phase_probe.clone(), phase)
    }
    /// Integrator class 3 durable completion key or typed host-capability refusal.
    pub fn completion_key(&self) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        self.completion_support.ensure_available()?;
        self.completion_key.clone().ok_or_else(|| {
            crate::RuntimeError::new(
                crate::RuntimeErrorCode::ToolCompletionKeyMissingCallId,
                "completion keys require a prepared tool call id",
            )
        })
    }
    /// Integrator class 3 canonical identity for one declared intent index.
    pub fn intent_identity(
        &self,
        intent_index: usize,
    ) -> Result<crate::ToolIntentIdentity, crate::ToolIntentRefusalReason> {
        crate::derive_tool_intent_identity_under(
            &self.session_id,
            &self.execution_scope_id,
            self.tool_call_id.as_deref(),
            intent_index,
            self.parent_invocation.as_deref(),
        )
    }
}

#[derive(Clone, Default)]
pub(crate) struct ToolCompletionState {
    key: Arc<Mutex<Option<crate::AwaitEventKey>>>,
}

impl ToolCompletionState {
    pub(crate) fn store(
        &self,
        key: crate::AwaitEventKey,
    ) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        let mut guard = self.key.lock_recover();
        if let Some(existing) = guard.as_ref() {
            return Ok(existing.clone());
        }
        *guard = Some(key.clone());
        Ok(key)
    }

    pub(crate) fn take(&self) -> Option<crate::AwaitEventKey> {
        self.key.lock_recover().take()
    }

    pub(crate) fn load(&self) -> Option<crate::AwaitEventKey> {
        self.key.lock_recover().clone()
    }
}

/// Per-call environment for [`ToolProvider::execute`]. Fields are sealed so
/// the runtime can add capabilities without breaking tool authors.
#[derive(Clone)]
pub struct ToolContext<'run> {
    pub(crate) session_id: SessionId,
    pub(crate) agent_frame_id: crate::FrameNodeId,
    pub(crate) sessions: Arc<dyn SessionStateService>,
    pub(crate) session_lifecycle: Arc<dyn SessionLifecycleService>,
    pub(crate) processes: Arc<dyn crate::ProcessService>,
    pub(crate) effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
    pub runtime_dispatch: Option<Arc<crate::tool_dispatch::ToolDispatchContext<'run>>>,
    pub(crate) runtime_execution_context: Option<crate::RuntimeExecutionContext<'run>>,
    pub(crate) cancellation_token: Option<tokio_util::sync::CancellationToken>,
    /// The process this call executes inside.
    pub(crate) enclosing_process: Option<ProcessId>,
    pub(crate) process_events: Option<ToolProcessEventContext>,
    pub(crate) attachment_store: Arc<crate::SessionAttachmentStore>,
    pub(crate) direct_completions: crate::DirectCompletionClient<'run>,
    pub(crate) prepared_payload: serde_json::Value,
    pub(crate) tool_execution_binding: serde_json::Value,
    tool_execution_route: ToolExecutionRoute,
    /// The id of the in-flight tool call that is invoking this tool.
    pub(crate) tool_call_id: Option<String>,
    pub(crate) attempt_number: u32,
    pub(crate) max_attempts: u32,
    pub(crate) replay_key: Option<String>,
    pub(crate) completion: ToolCompletionState,
    pub(crate) parent_invocation: Option<crate::RuntimeInvocation>,
    pub(crate) execution_env_spec: crate::ProcessExecutionEnvSpec,
    pub(crate) child_execution_trace_hook: Option<ToolChildExecutionTraceHook>,
    /// The realized-start sink a group child's driver installs so an
    /// orchestrating body's process starts reach its settlement's possession.
    /// `None` for every caller that is not a group child — its presence is
    /// the mark that this context was admitted under a group child's rebound
    /// dispatch; an ordinary orchestrating run's starts still ride its
    /// `ToolIntent` records.
    pub(crate) orchestrating_sinks: Option<crate::tool_dispatch::OrchestratingChildSinks>,
    /// The cancellation trio the child was validated to wait under, carried
    /// whole from its driver. `None` for every caller that is not a group
    /// child; a nested call must inherit exactly this wait — deriving one
    /// from the scope alone is always observing, which would wire a child
    /// admitted with no cooperative authority to a gate it must never see.
    pub(crate) turn_cancel_wait: Option<crate::runtime::TurnCancelWait>,
}

#[derive(Clone)]
/// Notification emitted when an orchestrating tool starts a child process.
pub struct ToolChildProcessStarted {
    /// Stable identity of the child process that started.
    pub process_id: ProcessId,
    /// Store-minted lifetime admitted by the child start.
    pub incarnation: crate::ProcessIncarnation,
    /// Durable execution attempt, when the observer saw the child after admission.
    pub attempt: Option<u32>,
    /// Optional tool-defined name for the child entry point.
    pub child_entry_name: Option<String>,
}

#[derive(Clone)]
/// Callback installed by a host to observe child processes started by tools.
pub struct ToolChildExecutionTraceHook {
    on_child_process_started: Arc<dyn Fn(ToolChildProcessStarted) + Send + Sync>,
}

impl ToolChildExecutionTraceHook {
    pub fn new(
        on_child_process_started: impl Fn(ToolChildProcessStarted) + Send + Sync + 'static,
    ) -> Self {
        Self {
            on_child_process_started: Arc::new(on_child_process_started),
        }
    }

    /// Notify the host that a tool started the supplied child process.
    pub fn child_process_started(&self, event: ToolChildProcessStarted) {
        (self.on_child_process_started)(event);
    }
}

#[derive(Clone)]
pub(crate) struct ToolProcessEventContext {
    process_id: ProcessId,
    execution_write_authority: crate::ProcessExecutionWriteAuthority,
    process_work: crate::ProcessWorkWiring,
    store: Option<Arc<dyn crate::RuntimePersistence>>,
    session_store_factory: Option<Arc<dyn crate::SessionStoreFactory>>,
    session_graph: Arc<dyn SessionGraphService>,
    queued_work: Arc<dyn crate::SessionWorkEngine>,
    process_wake_delivery_policy: crate::DeliveryPolicy,
    clock: Arc<dyn crate::Clock>,
}

pub struct ToolContextBuilder<'run> {
    session_id: SessionId,
    agent_frame_id: crate::FrameNodeId,
    sessions: Arc<dyn SessionStateService>,
    session_lifecycle: Arc<dyn SessionLifecycleService>,
    session_graph: Arc<dyn SessionGraphService>,
    processes: Arc<dyn crate::ProcessService>,
    effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
    runtime_dispatch: Option<Arc<crate::tool_dispatch::ToolDispatchContext<'run>>>,
    runtime_execution_context: Option<crate::RuntimeExecutionContext<'run>>,
    cancellation_token: Option<tokio_util::sync::CancellationToken>,
    enclosing_process: Option<ProcessId>,
    process_events: Option<ToolProcessEventContext>,
    attachment_store: Arc<crate::SessionAttachmentStore>,
    direct_completions: crate::DirectCompletionClient<'run>,
    prepared_payload: serde_json::Value,
    tool_execution_binding: serde_json::Value,
    tool_execution_route: ToolExecutionRoute,
    tool_call_id: Option<String>,
    completion: ToolCompletionState,
    parent_invocation: Option<crate::RuntimeInvocation>,
    execution_env_spec: crate::ProcessExecutionEnvSpec,
    child_execution_trace_hook: Option<ToolChildExecutionTraceHook>,
    orchestrating_sinks: Option<crate::tool_dispatch::OrchestratingChildSinks>,
    turn_cancel_wait: Option<crate::runtime::TurnCancelWait>,
}

impl<'run> ToolContextBuilder<'run> {
    pub(crate) fn from_dispatch(
        dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
    ) -> Self {
        Self {
            session_id: dispatch.session_id.clone(),
            agent_frame_id: dispatch.agent_frame_id.clone(),
            sessions: Arc::clone(&dispatch.sessions),
            session_lifecycle: Arc::clone(&dispatch.session_lifecycle),
            session_graph: Arc::clone(&dispatch.session_graph),
            processes: Arc::clone(&dispatch.processes),
            effect_controller: dispatch.effect_controller.clone(),
            runtime_dispatch: Some(Arc::clone(&dispatch)),
            runtime_execution_context: None,
            cancellation_token: None,
            enclosing_process: None,
            process_events: None,
            attachment_store: Arc::clone(&dispatch.attachment_store),
            direct_completions: dispatch.direct_completions.clone(),
            prepared_payload: serde_json::Value::Null,
            tool_execution_binding: serde_json::Value::Null,
            tool_execution_route: ToolExecutionRoute::Catalog,
            tool_call_id: None,
            completion: ToolCompletionState::default(),
            parent_invocation: dispatch.parent_invocation.clone(),
            execution_env_spec: dispatch.execution_env_spec.clone(),
            child_execution_trace_hook: None,
            orchestrating_sinks: None,
            turn_cancel_wait: None,
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn tool_call_id(mut self, tool_call_id: impl Into<Option<String>>) -> Self {
        self.tool_call_id = tool_call_id.into();
        self
    }

    pub fn prepared_call(mut self, call: &PreparedToolCall) -> Self {
        self.tool_call_id = Some(call.call_id.clone());
        self.prepared_payload = call.prepared_payload.clone();
        self
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn tool_execution_binding(mut self, binding: serde_json::Value) -> Self {
        self.tool_execution_binding = binding;
        self
    }

    pub fn cancellation_token(
        mut self,
        cancellation_token: Option<tokio_util::sync::CancellationToken>,
    ) -> Self {
        self.cancellation_token = cancellation_token;
        self
    }

    pub(crate) fn runtime_execution_context(
        mut self,
        context: crate::RuntimeExecutionContext<'run>,
    ) -> Self {
        self.runtime_execution_context = Some(context);
        self
    }

    /// Name the process this call executes inside. This is the one accessor
    /// hosts write; the process-event append target set via
    /// [`Self::process_events`] must agree with it.
    pub fn enclosing_process(mut self, process_id: Option<ProcessId>) -> Self {
        self.enclosing_process = process_id;
        self
    }

    #[allow(clippy::too_many_arguments)]
    pub fn process_events(
        mut self,
        process_id: impl Into<ProcessId>,
        execution_write_authority: crate::ProcessExecutionWriteAuthority,
        process_work: crate::ProcessWorkWiring,
        store: Option<Arc<dyn crate::RuntimePersistence>>,
        session_store_factory: Option<Arc<dyn crate::SessionStoreFactory>>,
        queued_work: Arc<dyn crate::SessionWorkEngine>,
        process_wake_delivery_policy: crate::DeliveryPolicy,
        clock: Arc<dyn crate::Clock>,
    ) -> Self {
        let process_id = process_id.into();
        // The event append target and the enclosing process are the same fact.
        // When the host already named one, they must agree; when it has not,
        // the write authority's id is it.
        match &self.enclosing_process {
            Some(enclosing) => assert_eq!(
                enclosing, &process_id,
                "process_events target must equal the context's enclosing process"
            ),
            None => self.enclosing_process = Some(process_id.clone()),
        }
        self.process_events = Some(ToolProcessEventContext {
            process_id,
            execution_write_authority,
            process_work,
            store,
            session_store_factory,
            session_graph: Arc::clone(&self.session_graph),
            queued_work,
            process_wake_delivery_policy,
            clock,
        });
        self
    }

    pub fn parent_invocation(mut self, metadata: Option<crate::RuntimeInvocation>) -> Self {
        self.parent_invocation = metadata;
        self
    }

    pub fn child_execution_trace_hook(mut self, hook: Option<ToolChildExecutionTraceHook>) -> Self {
        self.child_execution_trace_hook = hook;
        self
    }

    /// Installs the sinks an orchestrating group child's driver drains: the
    /// realized starts its settlement possesses and the refusal a nested call
    /// met. Internal: only the tool-child driver
    /// sets one, which is also what marks the context as admitted under a
    /// group child's rebound dispatch.
    pub(crate) fn orchestrating_sinks(
        mut self,
        buffer: crate::tool_dispatch::OrchestratingChildSinks,
    ) -> Self {
        self.orchestrating_sinks = Some(buffer);
        self
    }

    /// Installs the cancellation trio the child waits under, computed once by
    /// its driver from the recorded cancellation authority. Internal: only
    /// the tool-child driver sets one, and every nested retry sleep and
    /// deferred wait inside the child inherits it exactly.
    pub(crate) fn turn_cancel_wait(mut self, wait: crate::runtime::TurnCancelWait) -> Self {
        self.turn_cancel_wait = Some(wait);
        self
    }

    pub fn build(self) -> ToolContext<'run> {
        ToolContext {
            session_id: self.session_id,
            agent_frame_id: self.agent_frame_id,
            sessions: self.sessions,
            session_lifecycle: self.session_lifecycle,
            processes: self.processes,
            effect_controller: self.effect_controller,
            runtime_dispatch: self.runtime_dispatch,
            runtime_execution_context: self.runtime_execution_context,
            cancellation_token: self.cancellation_token,
            enclosing_process: self.enclosing_process,
            process_events: self.process_events,
            attachment_store: self.attachment_store,
            direct_completions: self.direct_completions,
            prepared_payload: self.prepared_payload,
            tool_execution_binding: self.tool_execution_binding,
            tool_execution_route: self.tool_execution_route,
            tool_call_id: self.tool_call_id,
            attempt_number: 1,
            max_attempts: 1,
            replay_key: None,
            completion: self.completion,
            parent_invocation: self.parent_invocation,
            execution_env_spec: self.execution_env_spec,
            child_execution_trace_hook: self.child_execution_trace_hook,
            orchestrating_sinks: self.orchestrating_sinks,
            turn_cancel_wait: self.turn_cancel_wait,
        }
    }
}

impl<'run> ToolContext<'run> {
    /// The logical root this call runs under, read from the admitted scope
    /// of its effect controller (FIG-3607 item 6): never a live read. `None`
    /// outside a session turn (a process body, a runtime operation).
    pub fn logical_root(&self) -> Option<crate::TurnId> {
        logical_root_of(self.effect_controller.scoped().admitted_scope().scope())
    }

    pub(crate) fn install_prederived_completion_key(&self, key: Option<crate::AwaitEventKey>) {
        if let Some(key) = key {
            let _ = self.completion.store(key);
        }
    }
    pub(crate) fn replay_validation_trace(&self) -> Option<crate::RuntimeEffectReplayTrace> {
        self.runtime_execution_context
            .as_ref()
            .and_then(crate::RuntimeExecutionContext::replay_validation_trace)
    }

    pub(crate) fn to_static(&self) -> Option<ToolContext<'static>> {
        Some(ToolContext {
            session_id: self.session_id.clone(),
            agent_frame_id: self.agent_frame_id.clone(),
            sessions: Arc::clone(&self.sessions),
            session_lifecycle: Arc::clone(&self.session_lifecycle),
            processes: Arc::clone(&self.processes),
            effect_controller: self.effect_controller.to_static()?,
            runtime_dispatch: match self.runtime_dispatch.as_ref() {
                Some(dispatch) => Some(Arc::new(dispatch.to_static()?)),
                None => None,
            },
            runtime_execution_context: match self.runtime_execution_context.as_ref() {
                Some(context) => Some(context.to_static()?),
                None => None,
            },
            cancellation_token: self.cancellation_token.clone(),
            enclosing_process: self.enclosing_process.clone(),
            process_events: self.process_events.clone(),
            attachment_store: Arc::clone(&self.attachment_store),
            direct_completions: self.direct_completions.to_static()?,
            prepared_payload: self.prepared_payload.clone(),
            tool_execution_binding: self.tool_execution_binding.clone(),
            tool_execution_route: self.tool_execution_route.clone(),
            tool_call_id: self.tool_call_id.clone(),
            attempt_number: self.attempt_number,
            max_attempts: self.max_attempts,
            replay_key: self.replay_key.clone(),
            completion: self.completion.clone(),
            parent_invocation: self.parent_invocation.clone(),
            execution_env_spec: self.execution_env_spec.clone(),
            child_execution_trace_hook: self.child_execution_trace_hook.clone(),
            orchestrating_sinks: self.orchestrating_sinks.clone(),
            turn_cancel_wait: self.turn_cancel_wait.clone(),
        })
    }

    #[cfg(any(test, feature = "testing"))]
    #[expect(
        clippy::too_many_arguments,
        reason = "testing constructor mirrors the sealed runtime tool context dependencies"
    )]
    #[expect(
        clippy::expect_used,
        reason = "test-only builder: `FrameNodeId::new` rejects only the empty string, and the literal here is not"
    )]
    pub(crate) fn builder(
        session_id: SessionId,
        sessions: Arc<dyn SessionStateService>,
        session_lifecycle: Arc<dyn SessionLifecycleService>,
        session_graph: Arc<dyn SessionGraphService>,
        processes: Arc<dyn crate::ProcessService>,
        effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
        attachment_store: Arc<crate::SessionAttachmentStore>,
        direct_completions: crate::DirectCompletionClient<'run>,
    ) -> ToolContextBuilder<'run> {
        ToolContextBuilder {
            session_id,
            agent_frame_id: crate::FrameNodeId::new("test-frame")
                .expect("test frame identity is non-empty"),
            sessions,
            session_lifecycle,
            session_graph,
            processes,
            effect_controller,
            runtime_dispatch: None,
            runtime_execution_context: None,
            cancellation_token: None,
            enclosing_process: None,
            process_events: None,
            attachment_store,
            direct_completions,
            prepared_payload: serde_json::Value::Null,
            tool_execution_binding: serde_json::Value::Null,
            tool_execution_route: ToolExecutionRoute::Catalog,
            tool_call_id: None,
            completion: ToolCompletionState::default(),
            parent_invocation: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            child_execution_trace_hook: None,
            orchestrating_sinks: None,
            turn_cancel_wait: None,
        }
    }

    pub fn from_dispatch(
        dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
    ) -> ToolContextBuilder<'run> {
        ToolContextBuilder::from_dispatch(dispatch)
    }

    /// Exposes session id to protocol and process-engine implementors while preparing or executing
    /// an authorized tool call.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Exposes agent frame id to protocol and process-engine implementors while preparing or
    /// executing an authorized tool call.
    pub fn agent_frame_id(&self) -> &crate::FrameNodeId {
        &self.agent_frame_id
    }

    /// Overrides the current frame lineage in an isolated tool-provider test.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_agent_frame_id_for_testing(mut self, agent_frame_id: crate::FrameNodeId) -> Self {
        self.agent_frame_id = agent_frame_id;
        self
    }

    /// Exposes sessions to protocol and process-engine implementors while preparing or executing an
    /// authorized tool call.
    ///
    /// The returned admin reads session state; a tool that needs a related
    /// session to run a turn starts a `ProcessInput::SessionTurn` process
    /// instead, as `lash-subagents` does.
    pub fn sessions(&self) -> ToolSessionAdmin {
        ToolSessionAdmin {
            session_id: self.session_id.clone(),
            sessions: Arc::clone(&self.sessions),
        }
    }

    /// Exposes dispatch to protocol and process-engine implementors while preparing or executing an
    /// authorized tool call.
    pub fn dispatch(&self) -> ToolDispatchClient<'run> {
        ToolDispatchClient {
            context: self.clone(),
        }
    }

    /// Exposes triggers to protocol and process-engine implementors while preparing or executing an
    /// authorized tool call.
    pub fn triggers(&self) -> ToolTriggerClient<'run> {
        ToolTriggerClient {
            context: self.clone(),
        }
    }

    pub(crate) fn process_admin(&self) -> InternalProcessAdmin<'run> {
        InternalProcessAdmin {
            session_id: self.session_id.clone(),
            agent_frame_id: self.agent_frame_id.clone(),
            processes: Arc::clone(&self.processes),
            effect_controller: self.effect_controller.clone(),
            parent_invocation: self.parent_invocation.clone(),
            tool_call_id: self.tool_call_id.clone(),
            execution_env_spec: self.execution_env_spec.clone(),
            orchestrating_sinks: self.orchestrating_sinks.clone(),
        }
    }

    /// Exposes emit child process started to protocol and process-engine implementors while
    /// preparing or executing an authorized tool call.
    pub fn emit_child_process_started(
        &self,
        process_id: impl Into<ProcessId>,
        incarnation: crate::ProcessIncarnation,
        attempt: Option<u32>,
        child_entry_name: Option<String>,
    ) {
        let Some(hook) = &self.child_execution_trace_hook else {
            return;
        };
        hook.child_process_started(ToolChildProcessStarted {
            process_id: process_id.into(),
            incarnation,
            attempt,
            child_entry_name,
        });
    }

    /// Exposes direct completions to protocol and process-engine implementors while preparing or
    /// executing an authorized tool call.
    pub fn direct_completions(&self) -> ToolDirectCompletionClient<'run> {
        ToolDirectCompletionClient {
            session_id: self.session_id.clone(),
            tool_call_id: self.tool_call_id.clone(),
            direct_completions: self.direct_completions.clone(),
            parent_invocation: self.parent_invocation.clone(),
        }
    }

    /// Provides session-scoped attachment operations to tool implementors so tool-produced blobs
    /// participate in durable intent and retention tracking.
    pub fn attachments(&self) -> ToolAttachmentClient {
        ToolAttachmentClient {
            store: Arc::clone(&self.attachment_store),
        }
    }

    /// Exposes process events to protocol and process-engine implementors while preparing or
    /// executing an authorized tool call.
    pub fn process_events(&self) -> ToolProcessEventClient {
        ToolProcessEventClient {
            context: self.process_events.clone(),
        }
    }

    /// Exposes cooperative cancellation to tool implementors, returning `None` when the execution
    /// boundary supplied no cancellation scope.
    pub fn cancellation_token(&self) -> Option<&tokio_util::sync::CancellationToken> {
        self.cancellation_token.as_ref()
    }

    /// The cancellation trio this context's nested waits inherit, installed
    /// by the tool-child driver from the child's recorded authority.
    /// `None` for every context that is not a group child's — the fallback
    /// callers that reach this accessor only do so under a group-child
    /// context, so a `None` here degrades to an unobserved wait rather than
    /// a scope-derived observing one.
    pub(crate) fn turn_cancel_wait(&self) -> Option<&crate::runtime::TurnCancelWait> {
        self.turn_cancel_wait.as_ref()
    }

    pub fn named_phase(&self, phase: &'static str) -> crate::runtime::RuntimeNamedPhase {
        match self.runtime_execution_context.as_ref() {
            Some(context) => context.named_phase(phase),
            None => crate::runtime::RuntimeNamedPhase::begin(None, phase),
        }
    }

    /// Exposes the process this call executes inside to protocol and process-engine
    /// implementors while preparing or executing an authorized tool call.
    pub fn enclosing_process(&self) -> Option<&str> {
        self.enclosing_process.as_deref()
    }

    /// Exposes tool call id to protocol and process-engine implementors while preparing or
    /// executing an authorized tool call.
    pub fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id.as_deref()
    }

    /// Exposes prepared payload to protocol and process-engine implementors while preparing or
    /// executing an authorized tool call.
    pub fn prepared_payload(&self) -> &serde_json::Value {
        &self.prepared_payload
    }

    /// Exposes tool execution binding to protocol and process-engine implementors while preparing
    /// or executing an authorized tool call.
    pub fn tool_execution_binding(&self) -> &serde_json::Value {
        &self.tool_execution_binding
    }

    pub fn decode_prepared_payload<T>(&self) -> Result<T, serde_json::Error>
    where
        T: serde::de::DeserializeOwned,
    {
        serde_json::from_value(self.prepared_payload.clone())
    }

    /// Current one-based attempt number for tool implementors handling this call.
    pub fn attempt_number(&self) -> u32 {
        self.attempt_number
    }

    /// Exposes max attempts to protocol and process-engine implementors while preparing or
    /// executing an authorized tool call.
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Exposes the durable replay key to tool implementors, returning `None` for calls that are not
    /// replay-scoped.
    pub fn replay_key(&self) -> Option<&str> {
        self.replay_key.as_deref()
    }

    /// Obtain the durable completion key for this call, required before returning
    /// [`ToolOutcome::Pending`](crate::ToolOutcome::Pending).
    ///
    /// A tool that defers its outcome (waiting on a webhook, human approval, or another
    /// service) calls this, hands the returned [`AwaitEventKey`](crate::AwaitEventKey)
    /// to whatever will complete the work out-of-band, and then returns
    /// `ToolOutcome::Pending(..)`. The key names the durable wait the runtime parks the
    /// call on; the external resolver delivers the result against it later.
    ///
    /// The key is stored on the context and consumed by the dispatcher when the tool returns
    /// `Pending`.
    /// Returning `Pending` without first calling this fails the call with
    /// `pending_tool_missing_completion_key`.
    pub async fn completion_key(&self) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        let tool_call_id = self.tool_call_id.clone().ok_or_else(|| {
            crate::RuntimeError::new(
                crate::RuntimeErrorCode::ToolCompletionKeyMissingCallId,
                "completion keys require a prepared tool call id",
            )
        })?;
        let scoped = self.effect_controller.scoped();
        let preparation = scoped
            .controller()
            .prepare_completion_key(
                scoped.execution_scope(),
                crate::AwaitEventWaitIdentity::tool_completion(tool_call_id),
                true,
            )
            .await?;
        match preparation {
            crate::CompletionKeyPreparation::Issued(key) => self.completion.store(key),
            crate::CompletionKeyPreparation::Unsupported
            | crate::CompletionKeyPreparation::NotNeeded => Err(crate::RuntimeError::new(
                crate::RuntimeErrorCode::AwaitEventUnsupported,
                "completion keys require an effect controller that issues durable await-event keys",
            )),
        }
    }

    pub(crate) fn take_completion_key(&self) -> Option<crate::AwaitEventKey> {
        self.completion.take()
    }

    /// Sets the process this call executes inside, along with the
    /// cooperative cancellation token the host pairs with it.
    pub fn with_enclosing_process(
        mut self,
        process_id: impl Into<ProcessId>,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Self {
        self.enclosing_process = Some(process_id.into());
        self.cancellation_token = Some(cancellation_token);
        self
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn with_process_events_for_testing(
        mut self,
        process_id: impl Into<ProcessId>,
        registry: Arc<dyn crate::ProcessRegistry>,
        execution_write_authority: crate::ProcessExecutionWriteAuthority,
    ) -> Self {
        let process_id = process_id.into();
        match &self.enclosing_process {
            Some(enclosing) => assert_eq!(
                enclosing, &process_id,
                "process_events target must equal the context's enclosing process"
            ),
            None => self.enclosing_process = Some(process_id.clone()),
        }
        let watched = crate::facade_support::watch_process_registry(registry);
        let port = Arc::new(crate::NativeProcessWork::for_registry(Arc::clone(
            watched.registry(),
        )));
        let process_work = crate::ProcessWorkWiring::new(watched, port);
        self.process_events = Some(ToolProcessEventContext {
            execution_write_authority,
            process_id,
            process_work,
            store: None,
            session_store_factory: None,
            session_graph: Arc::new(crate::plugin::NoopSessionManager),
            queued_work: Arc::new(crate::NoSessionWork::new()),
            process_wake_delivery_policy: crate::DeliveryPolicy::EarliestSafeBoundary,
            clock: Arc::new(crate::SystemClock),
        });
        self
    }

    pub(crate) fn with_retry_context(
        mut self,
        tool_name: &str,
        attempt_number: u32,
        max_attempts: u32,
    ) -> Self {
        self.attempt_number = attempt_number.max(1);
        self.max_attempts = max_attempts.max(1);
        self.replay_key = self
            .tool_call_id
            .as_ref()
            .map(|call_id| format!("lash-tool:{}:{call_id}:{tool_name}", self.session_id));
        self
    }

    pub(crate) fn with_prepared_payload(mut self, payload: serde_json::Value) -> Self {
        self.prepared_payload = payload;
        self
    }

    /// A body that declares tool intents derives its declaration identity from
    /// the prepared call id, and a body that appends to the process it runs
    /// inside needs that process's id. Neither is something a mock host has,
    /// so a unit test of such a body sets them here rather than reaching past
    /// the sealed context.
    #[cfg(any(test, feature = "testing"))]
    pub fn __with_attempt_binding_for_testing(
        mut self,
        tool_call_id: Option<String>,
        enclosing_process: Option<ProcessId>,
    ) -> Self {
        self.tool_call_id = tool_call_id;
        self.enclosing_process = enclosing_process;
        self
    }

    /// [`mock_tool_context`](crate::testing::mock_tool_context) runs under a
    /// `RuntimeOperation` scope, which names no opener — production attempts
    /// always run under a turn, drain or process scope. A fixture that
    /// exercises an owner-derived answer (a declared child's parent scope)
    /// binds the real scope here rather than leaning on the mock default.
    #[cfg(any(test, feature = "testing"))]
    pub fn __with_scoped_effect_controller_for_testing(
        mut self,
        scoped: crate::ScopedEffectController<'static>,
    ) -> Self {
        self.effect_controller = crate::runtime::RuntimeEffectControllerHandle::borrowed(scoped);
        self
    }

    pub(crate) fn with_tool_execution_binding(mut self, binding: serde_json::Value) -> Self {
        self.tool_execution_binding = binding;
        self
    }

    pub(crate) fn with_granted_source_id(mut self, source_id: Option<String>) -> Self {
        self.tool_execution_route = ToolExecutionRoute::Granted { source_id };
        self
    }

    pub(crate) fn with_attempt_dispatch(
        mut self,
        dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
        parent_invocation: crate::RuntimeInvocation,
    ) -> Self {
        self.effect_controller = dispatch.effect_controller.clone();
        self.direct_completions = dispatch.direct_completions.clone();
        self.runtime_dispatch = Some(dispatch);
        self.parent_invocation = Some(parent_invocation);
        self
    }

    /// Constructor reserved for `lash_core::testing` helpers. Do not call directly;
    /// use [`lash_core::testing::mock_tool_context`] instead.
    #[cfg(any(test, feature = "testing"))]
    #[expect(
        clippy::too_many_arguments,
        reason = "test-only constructor mirrors the sealed runtime tool context"
    )]
    pub fn __for_testing(
        session_id: SessionId,
        sessions: Arc<dyn SessionStateService>,
        session_lifecycle: Arc<dyn SessionLifecycleService>,
        session_graph: Arc<dyn SessionGraphService>,
        processes: Arc<dyn crate::ProcessService>,
        attachment_store: Arc<crate::SessionAttachmentStore>,
        direct_completions: crate::DirectCompletionClient<'static>,
        tool_call_id: Option<String>,
    ) -> ToolContext<'static> {
        ToolContext::builder(
            session_id,
            sessions,
            session_lifecycle,
            session_graph,
            processes,
            crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
                crate::testing::UnavailableEffectController,
            )),
            attachment_store,
            direct_completions,
        )
        .tool_call_id(tool_call_id)
        .build()
    }
}

/// Runtime-prepared executable tool call.
///
/// The raw model/provider identity remains visible, but any argument rewrites
/// and provider-owned context projections are frozen before the call crosses a
/// runtime effect or process boundary.
// `PartialEq` but not `Eq`: `args` and `prepared_payload` are `serde_json::Value`.
// Comparison exists so a retained tool-child request can prove it round-tripped
// its input unchanged (ADR 0099 §3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreparedToolCall {
    pub call_id: String,
    pub tool_id: ToolId,
    pub tool_name: String,
    pub args: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<ProviderReplayMeta>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub prepared_payload: serde_json::Value,
}

impl PreparedToolCall {
    /// Freezes an identity preparation for protocol and process-engine implementors without
    /// rewriting the model-supplied call arguments or provider metadata.
    pub fn identity(tool_id: ToolId, call: crate::sansio::PendingToolCall) -> Self {
        Self {
            call_id: call.call_id,
            tool_id,
            tool_name: call.tool_name,
            args: call.args,
            replay: call.replay,
            prepared_payload: serde_json::Value::Null,
        }
    }

    /// Reconstructs a fully prepared call for protocol and process-engine implementors crossing an
    /// effect or process boundary, preserving the supplied replay metadata and prepared payload.
    pub fn from_parts(
        call_id: impl Into<String>,
        tool_id: impl Into<ToolId>,
        tool_name: impl Into<String>,
        args: serde_json::Value,
        replay: Option<ProviderReplayMeta>,
        prepared_payload: serde_json::Value,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool_id: tool_id.into(),
            tool_name: tool_name.into(),
            args,
            replay,
            prepared_payload,
        }
    }
}

/// One ordered child inside a runtime-prepared tool batch.
///
/// The call itself carries the executable provider payload. `replay_suffix`
/// is the deterministic suffix used for child effects such as retry sleeps or
/// pending completion awaits when the batch is the durable parent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedToolBatchCall {
    pub call: PreparedToolCall,
    pub replay_suffix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_grant: Option<Box<ToolExecutionGrant>>,
}

/// Runtime-prepared executable tool batch.
///
/// The vector order is source order. Calls run concurrently, but launches and
/// pending completion consumption are projected back through this order.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedToolBatch {
    pub batch_id: crate::BatchId,
    pub calls: Vec<PreparedToolBatchCall>,
}

impl PreparedToolBatch {
    /// Freezes source-order prepared calls for protocol and process-engine implementors; execution
    /// may be concurrent, but launch and completion projection retain this order.
    pub fn new(batch_id: impl Into<crate::BatchId>, calls: Vec<PreparedToolCall>) -> Self {
        let batch_id = batch_id.into();
        let calls = calls
            .into_iter()
            .enumerate()
            .map(|(index, call)| PreparedToolBatchCall {
                replay_suffix: format!("child:{index}:{}", call.call_id),
                call,
                execution_grant: None,
            })
            .collect();
        Self { batch_id, calls }
    }

    pub(crate) fn new_with_grants(
        batch_id: impl Into<crate::BatchId>,
        calls: Vec<(PreparedToolCall, Option<ToolExecutionGrant>)>,
    ) -> Self {
        let batch_id = batch_id.into();
        let calls = calls
            .into_iter()
            .enumerate()
            .map(|(index, (call, execution_grant))| PreparedToolBatchCall {
                replay_suffix: format!("child:{index}:{}", call.call_id),
                call,
                execution_grant: execution_grant.map(Box::new),
            })
            .collect();
        Self { batch_id, calls }
    }
}

/// Explicit authority to execute a tool outside Tool Catalog membership.
///
/// Normal tool calls are authorized by catalog membership. A grant is a
/// separate, caller-provided capability used by deferred resolution flows: it
/// carries the manifest/contract to validate the call plus an opaque host
/// execution binding that providers can inspect from the prepare and execute
/// contexts.
// `PartialEq` but not `Eq`: `execution_binding` is a `serde_json::Value`, whose
// float arm has no total equality. Comparison exists so a retained tool-child
// request can prove it round-tripped its admitted authority unchanged
// (ADR 0099 §3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolExecutionGrant {
    /// Tool identity and model-facing metadata authorized by the grant.
    pub(crate) manifest: ToolManifest,
    /// Contract used to validate granted call arguments without consulting the
    /// current Tool Catalog.
    pub(crate) contract: Box<ToolContract>,
    /// Explicit registry source route for registry-backed execution. Direct
    /// non-registry providers may ignore this; [`ToolRegistry`](crate::ToolRegistry)
    /// requires it.
    pub source_id: Option<String>,
    /// Opaque host routing payload passed to prepare and execute contexts.
    pub execution_binding: serde_json::Value,
}

impl ToolExecutionGrant {
    pub fn from_definition(definition: ToolDefinition) -> Self {
        Self {
            manifest: definition.manifest(),
            contract: Box::new(definition.contract()),
            source_id: None,
            execution_binding: serde_json::Value::Null,
        }
    }

    /// Returns the tool identity and model-facing metadata authorized by this grant.
    pub fn manifest(&self) -> &ToolManifest {
        &self.manifest
    }

    /// Returns the contract used to validate granted call arguments.
    pub fn contract(&self) -> &ToolContract {
        &self.contract
    }

    /// Sets the source id carried by a `ToolExecutionGrant` for protocol and process-engine
    /// implementors while preparing or executing an authorized tool call.
    pub fn with_source_id(mut self, source_id: impl Into<String>) -> Self {
        self.source_id = Some(source_id.into());
        self
    }

    /// Sets the execution binding carried by a `ToolExecutionGrant` for protocol and process-engine
    /// implementors while preparing or executing an authorized tool call.
    pub fn with_execution_binding(mut self, execution_binding: serde_json::Value) -> Self {
        self.execution_binding = execution_binding;
        self
    }
}

#[derive(Clone)]
pub struct ToolPrepareContext {
    session_id: SessionId,
    sessions: Arc<dyn SessionStateService>,
    turn_context: crate::TurnContext,
    tool_call_id: Option<String>,
    tool_execution_binding: serde_json::Value,
    tool_execution_route: ToolExecutionRoute,
}

impl ToolPrepareContext {
    pub(crate) fn with_execution_binding(
        session_id: SessionId,
        sessions: Arc<dyn SessionStateService>,
        turn_context: crate::TurnContext,
        tool_call_id: Option<String>,
        tool_execution_binding: serde_json::Value,
    ) -> Self {
        Self {
            session_id,
            sessions,
            turn_context,
            tool_call_id,
            tool_execution_binding,
            tool_execution_route: ToolExecutionRoute::Catalog,
        }
    }

    pub(crate) fn with_granted_source_id(mut self, source_id: Option<String>) -> Self {
        self.tool_execution_route = ToolExecutionRoute::Granted { source_id };
        self
    }

    pub(crate) fn execution_route(&self) -> &ToolExecutionRoute {
        &self.tool_execution_route
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id.as_deref()
    }

    pub fn tool_execution_binding(&self) -> &serde_json::Value {
        &self.tool_execution_binding
    }

    pub fn turn_context(&self) -> &crate::TurnContext {
        &self.turn_context
    }

    pub fn plugin_input<T>(&self, plugin_id: &'static str) -> Option<&T>
    where
        T: 'static,
    {
        self.turn_context.plugin_input::<T>(plugin_id)
    }

    /// Snapshots the current session for protocol and tool implementors preparing an authorized
    /// call; failures preserve the plugin error contract.
    pub async fn session_snapshot(&self) -> Result<SessionSnapshot, PluginError> {
        self.sessions.snapshot_session(&self.session_id).await
    }

    /// Captures the spawn-time [`crate::SessionPluginInit`] payload a
    /// `ParentFork` creation request must carry for a peer of the current
    /// session.
    pub async fn session_plugin_init(&self) -> Result<crate::SessionPluginInit, PluginError> {
        self.sessions.session_plugin_init(&self.session_id).await
    }

    pub async fn tool_catalog(&self) -> Result<Vec<serde_json::Value>, PluginError> {
        self.sessions.tool_catalog(&self.session_id).await
    }

    /// Returns the shared canonical catalog snapshot for protocol and tool implementors that
    /// prepare multiple calls without rebuilding the projection.
    pub async fn shared_tool_catalog(
        &self,
    ) -> Result<std::sync::Arc<Vec<serde_json::Value>>, PluginError> {
        self.sessions.shared_tool_catalog(&self.session_id).await
    }
}

pub struct ToolPrepareCall<'a> {
    pub tool_id: ToolId,
    pub pending: crate::sansio::PendingToolCall,
    pub context: &'a ToolPrepareContext,
}

/// Per-call inputs handed to [`ToolProvider::execute`].
///
/// The immutable manifest couples the stable tool ID and provider-facing name.
/// Dispatch owns authorization and route selection; this view is not an
/// authorization token.
pub struct ToolCall<'a> {
    manifest: &'a ToolManifest,
    pub args: &'a serde_json::Value,
    pub context: &'a AttemptContext<'a>,
}

impl<'a> ToolCall<'a> {
    /// Only the runtime dispatcher builds these; the manifest is the coupling between stable
    /// ID and provider-facing name.
    pub fn new(
        manifest: &'a ToolManifest,
        args: &'a serde_json::Value,
        context: &'a AttemptContext<'a>,
    ) -> Self {
        Self {
            manifest,
            args,
            context,
        }
    }

    /// The stable tool ID carried by the pinned manifest.
    pub fn tool_id(&self) -> &'a ToolId {
        &self.manifest.id
    }

    /// The provider-facing tool name carried by the pinned manifest.
    pub fn name(&self) -> &'a str {
        &self.manifest.name
    }
}

/// Trait for providing leaf tools to the sandbox.
///
/// Implementations supply cheap [`ToolManifest`]s, lazily resolved
/// [`ToolContract`]s, and a single required
/// [`execute`](Self::execute) method that handles every call. Tools that
/// need session state read it from `call.context`.
///
/// Lash contains an `execute` panic as a typed call failure. Containment does
/// not establish that the host object's own interior-mutability state still
/// satisfies its invariants; hosts own replacement or repair before reuse.
#[async_trait::async_trait]
pub trait ToolProvider: Send + Sync + 'static {
    fn tool_manifests(&self) -> Vec<ToolManifest>;
    fn resolve_manifest(&self, name: &str) -> Option<ToolManifest> {
        self.tool_manifests()
            .into_iter()
            .find(|manifest| manifest.name == name)
    }
    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        self.tool_manifests()
            .into_iter()
            .find(|manifest| manifest.id == *id)
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>>;
    fn resolve_contract_by_id(&self, id: &ToolId) -> Option<Arc<ToolContract>> {
        let manifest = self.resolve_manifest_by_id(id)?;
        self.resolve_contract(&manifest.name)
    }
    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        Ok(PreparedToolCall::identity(call.tool_id, call.pending))
    }
    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome;
    fn attempt_may_defer(&self, _tool_id: &ToolId) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_context_builder_carries_call_payload_and_cancellation_state() {
        let cancellation = tokio_util::sync::CancellationToken::new();
        let prepared = PreparedToolCall::from_parts(
            "call-1",
            "tool:demo_tool",
            "demo_tool",
            serde_json::json!({ "input": true }),
            None,
            serde_json::json!({ "prepared": true }),
        );

        let context = ToolContext::builder(
            SessionId::from("session-1"),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::UnavailableProcessService),
            crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
                crate::testing::UnavailableEffectController,
            )),
            Arc::new(crate::SessionAttachmentStore::unavailable()),
            crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
        )
        .prepared_call(&prepared)
        .cancellation_token(Some(cancellation.clone()))
        .enclosing_process(Some("process-1".into()))
        .build();

        assert_eq!(context.session_id(), "session-1");
        assert_eq!(context.tool_call_id(), Some("call-1"));
        assert_eq!(
            context.prepared_payload(),
            &serde_json::json!({ "prepared": true })
        );
        assert_eq!(context.enclosing_process(), Some("process-1"));
        assert!(context.cancellation_token().is_some());
    }

    #[test]
    fn enclosing_process_travels_from_tool_context_to_attempt_context() {
        let context = crate::testing::mock_tool_context()
            .with_enclosing_process("process-1", tokio_util::sync::CancellationToken::new());
        let attempt = crate::AttemptContext::__for_testing(&context, "attempt-scope".to_string());
        assert_eq!(attempt.enclosing_process(), Some("process-1"));
    }

    // -----------------------------------------------------------------------
    // FIG-3417: the lifecycle parent a child start declares comes from ONE
    // shared derivation — the admitted execution scope, which for a process
    // already carries the incarnation the admission authority bound. Nothing
    // on this path re-resolves the reusable process name against a registry.
    // -----------------------------------------------------------------------

    fn tool_context_under_scope(admitted: crate::AdmittedScope) -> ToolContext<'static> {
        let controller = crate::ScopedEffectController::shared(
            Arc::new(crate::testing::UnavailableEffectController),
            admitted,
        )
        .expect("the test scope validates");
        ToolContext::builder(
            SessionId::from("session-1"),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::UnavailableProcessService),
            crate::runtime::RuntimeEffectControllerHandle::borrowed(controller),
            Arc::new(crate::SessionAttachmentStore::unavailable()),
            crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
        )
        .build()
    }

    /// A recorded leaf attempt under a process scope parents on the pinned
    /// incarnation — the attempt carries no registry query to re-resolve the
    /// name, only the admission-time pin.
    #[tokio::test]
    async fn an_attempt_under_a_process_scope_parents_on_the_pinned_incarnation() {
        let incarnation = crate::ProcessIncarnation::from_registration_sequence(3);
        let context = tool_context_under_scope(crate::AdmittedScope::process(
            crate::ProcessRef::new(ProcessId::from("worker"), incarnation),
        ));
        let attempt = crate::AttemptContext::__for_testing(&context, "attempt-scope".to_string());
        assert_eq!(
            attempt
                .child_process_parent_scope()
                .expect("the pinned incarnation is the parent"),
            crate::ParentScope::process(crate::ProcessRef::new(
                ProcessId::from("worker"),
                incarnation,
            )),
        );
    }

    /// A process scope nobody bound an admitted incarnation to cannot reach
    /// this path at all: the controller's construction input — an
    /// `AdmittedScope` — refuses the unpinned pair, so the late refusal that
    /// used to live in `child_process_parent_scope` has no input left.
    #[tokio::test]
    async fn an_attempt_under_an_unpinned_process_scope_is_unconstructible() {
        assert!(matches!(
            crate::AdmittedScope::new(crate::ExecutionScope::process("worker"), None),
            Err(crate::AdmittedScopeError::ProcessIncarnationMissing { .. })
        ));
    }

    /// The orchestrating surface takes the same shared derivation: the pinned
    /// incarnation, not a registry lookup.
    #[tokio::test]
    async fn an_orchestrating_context_parents_on_the_pinned_incarnation() {
        let incarnation = crate::ProcessIncarnation::from_registration_sequence(2);
        let context = tool_context_under_scope(crate::AdmittedScope::process(
            crate::ProcessRef::new(ProcessId::from("worker"), incarnation),
        ));
        let orchestration = crate::OrchestrationContext::new(context);
        assert_eq!(
            orchestration
                .child_process_parent_scope()
                .expect("the pinned incarnation is the parent"),
            crate::ParentScope::process(crate::ProcessRef::new(
                ProcessId::from("worker"),
                incarnation,
            )),
        );
    }
}
