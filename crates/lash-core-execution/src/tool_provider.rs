use crate::ProcessId;
use crate::SessionId;
pub(crate) use completion_support::AttemptCompletionSupport;
use std::sync::{Arc, Mutex};

use lash_sansio::llm::types::ProviderReplayMeta;
use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};

use crate::plugin::{PluginError, SessionLifecycleService, SessionSnapshot, SessionStateService};
use crate::{ToolContract, ToolDefinition, ToolId, ToolManifest, ToolOutcome};

mod attachments;
mod completion_support;
mod direct_completion;
mod isolation;
mod session;

pub use attachments::ToolAttachmentClient;
pub use direct_completion::ToolDirectCompletionClient;
pub use isolation::{IsolatedProcessBinding, IsolatedProcessRequest};
pub use session::ToolSessionLlmProfile;

/// Integrator class 3 session reads available inside a recorded leaf attempt.
///
/// Under a session owner every read answers from the session. Under a process
/// owner, the model and the tool catalog answer from the process's captured
/// environment and pinned catalog, the current snapshot refuses with
/// [`PluginError::NotASessionRuntime`], and a named snapshot works for any
/// explicit session.
#[derive(Clone)]
pub struct AttemptSessionReads {
    owner: crate::RuntimeOwner,
    sessions: Arc<dyn SessionStateService>,
    /// The policy of the environment the attempt runs under: a process
    /// owner's model reads answer from it.
    policy: crate::SessionPolicy,
    /// The catalog the attempt was dispatched against: a process owner's
    /// catalog reads answer from it. `None` outside a runtime dispatch.
    tool_catalog: Option<Arc<crate::ToolCatalog>>,
}

impl AttemptSessionReads {
    /// Integrator class 3 read of the attempt owner's effective model policy.
    pub async fn model(&self) -> Result<session::ToolSessionLlmProfile, PluginError> {
        let policy = match &self.owner {
            crate::RuntimeOwner::Session(session_id) => {
                self.sessions.snapshot_session(session_id).await?.policy
            }
            crate::RuntimeOwner::Process(_) => self.policy.clone(),
        };
        let Some(config) = policy.model else {
            return Err(PluginError::Session(
                "the attempt owner has selected no model".to_string(),
            ));
        };
        Ok(session::ToolSessionLlmProfile {
            model: config,
            attachment_acceptance: policy.attachment_acceptance,
            generation: policy.generation,
        })
    }

    /// Integrator class 3 snapshot of the bound session without an effect
    /// controller. A process owner has no session and is refused.
    pub async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
        match &self.owner {
            crate::RuntimeOwner::Session(session_id) => {
                self.sessions.snapshot_session(session_id).await
            }
            crate::RuntimeOwner::Process(process_id) => Err(crate::runtime::not_a_session_runtime(
                "snapshot_current",
                process_id,
            )),
        }
    }

    /// Integrator class 3 snapshot of a named session through controller-free reads.
    pub async fn snapshot(&self, session_id: &SessionId) -> Result<SessionSnapshot, PluginError> {
        self.sessions.snapshot_session(session_id).await
    }

    /// Integrator class 3 read of the owner's serialized tool catalog.
    pub async fn tool_catalog(&self) -> Result<Vec<serde_json::Value>, PluginError> {
        match &self.owner {
            crate::RuntimeOwner::Session(session_id) => {
                self.sessions.tool_catalog(session_id).await
            }
            crate::RuntimeOwner::Process(_) => Ok(self.pinned_catalog()?.as_ref().clone()),
        }
    }

    /// Integrator class 3 shared read of the immutable serialized tool catalog.
    pub async fn shared_tool_catalog(&self) -> Result<Arc<Vec<serde_json::Value>>, PluginError> {
        match &self.owner {
            crate::RuntimeOwner::Session(session_id) => {
                self.sessions.shared_tool_catalog(session_id).await
            }
            crate::RuntimeOwner::Process(_) => self.pinned_catalog(),
        }
    }

    fn pinned_catalog(&self) -> Result<Arc<Vec<serde_json::Value>>, PluginError> {
        let catalog = self.tool_catalog.as_ref().ok_or_else(|| {
            PluginError::Session(format!(
                "`{}` has no pinned tool catalog outside a runtime dispatch",
                self.owner
            ))
        })?;
        Ok(Arc::new(crate::tool_registry::project_tool_catalog(
            catalog.tools.iter().cloned(),
        )))
    }
}

/// Integrator class 3 controller-free process reads for a recorded leaf attempt.
#[derive(Clone)]
pub struct AttemptProcessReads {
    owner: crate::RuntimeOwner,
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
            .list_visible_for_attempt(&self.owner, filter.list_mode())
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

/// Integrator class 3 sealed, controller-free environment for a recorded leaf attempt.
#[derive(Clone)]
pub struct AttemptContext<'run> {
    fleet_format: crate::FleetFormat,
    /// Who the attempt runs for: a session on its admitted frame, or a
    /// process.
    owner: crate::ExecutionOwner,
    /// The runtime-owned parent scope for a child lifecycle declaration —
    /// the admitted pair the enclosing execution runs under, which is the
    /// whole input to the one owner derivation —
    /// [`crate::EffectOpener::for_scope`] — and it is never re-resolved by
    /// name (ADR 0099 §1, FIG-3417).
    parent_scope: crate::AdmittedScope,
    execution_scope_id: String,
    sessions: AttemptSessionReads,
    processes: AttemptProcessReads,
    cancellation_token: Option<tokio_util::sync::CancellationToken>,
    /// The process this attempt executes inside, resolved once at context
    /// construction. `ToolContext` carries the same single fact.
    enclosing_process: Option<ProcessId>,
    attachment_store: Arc<crate::RuntimeAttachmentStore>,
    /// The dispatch-bound direct-completion client. `pub(crate)` so the
    /// attempt-atomicity laws can reach the *raw* client and prove the binding
    /// travels with it rather than with the accessor.
    pub direct_completions: crate::DirectCompletionClient<'run>,
    /// The recorded attempt this leaf body runs inside. Carried so
    /// attempt-attributed capabilities classify their journal position exactly
    /// as the legacy [`ToolContext`] path does. Boxed because this context is
    /// captured by the deep tool-dispatch futures.
    parent_invocation: Option<Box<crate::RuntimeInvocation>>,
    prepared_payload: serde_json::Value,
    tool_execution_binding: serde_json::Value,
    call_id: lash_sansio::ToolCallId,
    attempt_number: u32,
    max_attempts: u32,
    /// The provenance a child this body declares inherits when the attempt is
    /// running inside a durable process. `None` where the attempt is not
    /// running inside one, and the child takes the declaring session's own.
    process_spawn_provenance: Option<crate::ProcessSpawnProvenance>,
    process_lineage: Option<crate::ProcessLineage>,
    /// The execution-environment reference this attempt inherits from the
    /// durable process it runs inside — already published under a durable
    /// owner, so a declaration carrying it publishes nothing at realization.
    /// `None` outside a process execution, where the spec must be published.
    inherited_process_execution_env_ref: Option<crate::ProcessExecutionEnvRef>,
    execution_env_spec: crate::ProcessExecutionEnvSpec,
    completion: AttemptCompletionSupport,
    phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
    tool_execution_route: ToolExecutionRoute,
    /// The catalog the attempt was dispatched against. `None` outside a
    /// runtime dispatch.
    tool_catalog: Option<Arc<crate::ToolCatalog>>,
    definition_engines: crate::ProcessEngineRegistry,
}

impl<'run> AttemptContext<'run> {
    pub fn fleet_format(&self) -> crate::FleetFormat {
        self.fleet_format
    }

    pub fn definition_engines(&self) -> &crate::ProcessEngineRegistry {
        &self.definition_engines
    }

    /// The logical run this attempt runs under, read from the admitted
    /// scope it was recorded in (FIG-3607 item 6): never a live read. `None`
    /// outside a session turn (a process body, a runtime operation).
    pub fn logical_run(&self) -> Option<crate::TurnId> {
        self.parent_scope.scope().logical_run()
    }

    /// The start context a child start declared by this attempt draws its
    /// lifetime from (FIG-3607 R2): materialized from the admitted scope the
    /// enclosing execution runs under and the lineage of the process it runs
    /// inside, with no live reads. A plugin resolves its [`LifetimePolicy`]
    /// against it and declares the decision; realization records this same
    /// context's ancestry beside it.
    ///
    /// [`LifetimePolicy`]: crate::LifetimePolicy
    pub fn start_cx(&self) -> Result<crate::StartCx, PluginError> {
        crate::StartCx::materialize(&self.parent_scope, self.process_lineage.as_ref())
            .map_err(|error| PluginError::Session(error.to_string()))
    }

    pub(crate) fn from_tool_context(
        context: &ToolContext<'run>,
        execution_scope_id: String,
        completion: AttemptCompletionSupport,
    ) -> Self {
        let phase_probe = context
            .runtime_execution_context
            .as_ref()
            .and_then(crate::RuntimeExecutionContext::attempt_phase_probe);
        Self {
            fleet_format: context
                .runtime_execution_context
                .as_ref()
                .map(crate::RuntimeExecutionContext::fleet_format)
                .unwrap_or_else(crate::FleetFormat::current),
            definition_engines: context
                .runtime_dispatch
                .as_ref()
                .map(|dispatch| dispatch.process_engines.clone())
                .unwrap_or_default(),
            parent_scope: context.effect_controller.admitted_scope().clone(),
            owner: context.owner.clone(),
            execution_scope_id,
            sessions: AttemptSessionReads {
                owner: context.owner.runtime_owner(),
                sessions: Arc::clone(&context.sessions),
                policy: context.execution_env_spec.policy.clone(),
                tool_catalog: context
                    .runtime_dispatch
                    .as_ref()
                    .map(|dispatch| Arc::clone(&dispatch.tool_catalog)),
            },
            processes: AttemptProcessReads {
                owner: context.owner.runtime_owner(),
                processes: Arc::clone(&context.processes),
            },
            cancellation_token: context.cancellation_token.clone(),
            enclosing_process: context.enclosing_process.clone(),
            attachment_store: Arc::clone(&context.attachment_store),
            direct_completions: context.direct_completions.clone(),
            parent_invocation: context.parent_invocation.clone().map(Box::new),
            prepared_payload: context.prepared_payload.clone(),
            tool_execution_binding: context.tool_execution_binding.clone(),
            call_id: context.call_id.clone(),
            attempt_number: context.attempt_number,
            max_attempts: context.max_attempts,
            process_spawn_provenance: context
                .runtime_execution_context
                .as_ref()
                .and_then(|runtime| runtime.process_spawn_provenance()),
            process_lineage: context.process_lineage(),
            inherited_process_execution_env_ref: context
                .runtime_execution_context
                .as_ref()
                .and_then(|runtime| runtime.inherited_process_execution_env_ref()),
            execution_env_spec: context.execution_env_spec.clone(),
            completion,
            phase_probe,
            tool_execution_route: context.tool_execution_route.clone(),
            tool_catalog: context
                .runtime_dispatch
                .as_ref()
                .map(|dispatch| Arc::clone(&dispatch.tool_catalog)),
        }
    }

    pub(crate) fn execution_route(&self) -> &ToolExecutionRoute {
        &self.tool_execution_route
    }

    /// Integrator class 3: who this recorded attempt runs for.
    pub fn owner(&self) -> &crate::ExecutionOwner {
        &self.owner
    }
    /// Integrator class 3 identity for the session that owns this recorded
    /// attempt, or [`PluginError::NotASessionRuntime`] inside a process.
    pub fn session_id(&self) -> Result<&SessionId, PluginError> {
        self.owner.require_session("attempt_session_id")
    }
    /// Integrator class 3 durable turn or process scope used for intent identity.
    pub fn execution_scope_id(&self) -> &str {
        &self.execution_scope_id
    }
    /// Integrator class 3 agent-frame identity that authorized this provider
    /// attempt, or [`PluginError::NotASessionRuntime`] inside a process.
    pub fn agent_frame_id(&self) -> Result<&crate::FrameNodeId, PluginError> {
        self.owner.require_agent_frame("attempt_agent_frame_id")
    }
    /// Integrator class 3 controller-free session reads for this attempt.
    pub fn sessions(&self) -> AttemptSessionReads {
        self.sessions.clone()
    }
    /// Integrator class 3 controller-free process reads for this attempt.
    pub fn processes(&self) -> AttemptProcessReads {
        self.processes.clone()
    }
    /// Integrator class 3 read of the tool catalog this attempt was dispatched
    /// against: the tools its caller can call. A body that compiles a program
    /// links it against them (FIG-3116). `None` outside a runtime dispatch.
    pub fn tool_catalog(&self) -> Option<&Arc<crate::ToolCatalog>> {
        self.tool_catalog.as_ref()
    }
    /// Integrator class 3 cooperative cancellation token supplied by the attempt host.
    pub fn cancellation_token(&self) -> Option<&tokio_util::sync::CancellationToken> {
        self.cancellation_token.as_ref()
    }
    /// Integrator class 3 process this attempt executes inside, if any.
    pub fn enclosing_process(&self) -> Option<&ProcessId> {
        self.enclosing_process.as_ref()
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
            owner: self.owner.runtime_owner(),
            call_id: self.call_id.clone(),
            direct_completions: self.direct_completions.clone(),
            parent_invocation: self.parent_invocation.as_deref().cloned(),
        }
    }
    /// Integrator class 3 payload sealed by the provider's prepare phase.
    pub fn prepared_payload(&self) -> &serde_json::Value {
        &self.prepared_payload
    }
    /// Integrator class 3 protocol-owned execution binding for this tool call.
    pub fn tool_execution_binding(&self) -> &serde_json::Value {
        &self.tool_execution_binding
    }
    /// Lash's identity for this logical call (ADR 0117): the key a tool
    /// keys its idempotency on.
    ///
    /// Tools are at-least-once: a crash between a tool's effect and the
    /// durable record of its outcome runs the call again, and a reported
    /// failure is retried. Every run of one logical call — a crash replay, a
    /// retry after a reported failure — sees this same id; every other call
    /// sees a different one, even when the model's provider repeats its own
    /// call id. A service the tool calls deduplicates on it. A tool that
    /// wants a fresh key per attempt combines it with
    /// [`attempt_number`](Self::attempt_number) itself.
    pub fn call_id(&self) -> &lash_sansio::ToolCallId {
        &self.call_id
    }
    /// Integrator class 3 one-based retry attempt number.
    pub fn attempt_number(&self) -> u32 {
        self.attempt_number
    }
    /// Integrator class 3 retry ceiling sealed for this invocation.
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }
    /// The provenance a child declared by this body inherits.
    ///
    /// A process's children belong to the chain that started the process, not
    /// to the process's own runtime: they carry the chain's
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
    /// The immutable environment this attempt declares by reference.
    /// The runtime stores it under the declaring journal before recording the result.
    pub fn process_execution_env_ref(&self) -> Result<crate::ProcessExecutionEnvRef, PluginError> {
        if let Some(env_ref) = self.inherited_process_execution_env_ref.as_ref() {
            return Ok(env_ref.clone());
        }
        self.execution_env_spec.stable_ref().map_err(|error| {
            PluginError::Session(format!(
                "failed to encode process execution environment: {error}"
            ))
        })
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
    /// The host key of this call's completion wait: the tool completion wait
    /// its round pinned when it admitted the call, which a host resolves
    /// through [`resolve_host`](crate::runtime::actor::waits::resolve_host).
    /// A rerun of the call is handed the same key.
    ///
    /// # Errors
    ///
    /// A typed refusal when the capability is missing: the tool did not
    /// declare that its attempt may defer, or the call runs where no
    /// completion wait is pinned for it.
    pub fn completion_key(&self) -> Result<crate::PinnedKey, crate::RuntimeError> {
        self.completion.key()
    }
    /// The identity of the intent this attempt declares at `intent_index`,
    /// derived from its [`call_id`](Self::call_id).
    pub fn intent_identity(&self, intent_index: u32) -> crate::ToolIntentIdentity {
        crate::derive_tool_intent_identity_under(
            &self.owner.runtime_owner(),
            &self.execution_scope_id,
            &self.call_id,
            intent_index,
            self.parent_invocation.as_deref(),
        )
    }
}

#[derive(Clone, Default)]
pub(crate) struct ToolCompletionState {
    key: Arc<Mutex<Option<crate::PinnedKey>>>,
}

impl ToolCompletionState {
    pub(crate) fn store(&self, key: crate::PinnedKey) {
        let mut guard = self.key.lock_recover();
        if guard.is_none() {
            *guard = Some(key);
        }
    }

    pub(crate) fn take(&self) -> Option<crate::PinnedKey> {
        self.key.lock_recover().take()
    }

    pub(crate) fn load(&self) -> Option<crate::PinnedKey> {
        self.key.lock_recover().clone()
    }
}

/// The runtime's per-call dispatch state for one tool call. It is
/// crate-private: a provider body receives only the [`AttemptContext`]
/// projected from it, which reaches no dispatch, process administration or
/// effect controller.
#[derive(Clone)]
pub(crate) struct ToolContext<'run> {
    pub(crate) owner: crate::ExecutionOwner,
    pub(crate) sessions: Arc<dyn SessionStateService>,
    pub(crate) session_lifecycle: Arc<dyn SessionLifecycleService>,
    pub(crate) processes: Arc<dyn crate::ProcessService>,
    pub(crate) effect_controller: crate::ActorContext,
    pub(crate) runtime_dispatch: Option<Arc<crate::tool_dispatch::ToolDispatchContext<'run>>>,
    pub(crate) runtime_execution_context: Option<crate::RuntimeExecutionContext<'run>>,
    pub(crate) cancellation_token: Option<tokio_util::sync::CancellationToken>,
    /// The process this call executes inside.
    pub(crate) enclosing_process: Option<ProcessId>,
    pub(crate) attachment_store: Arc<crate::RuntimeAttachmentStore>,
    pub(crate) direct_completions: crate::DirectCompletionClient<'run>,
    pub(crate) prepared_payload: serde_json::Value,
    pub(crate) tool_execution_binding: serde_json::Value,
    tool_execution_route: ToolExecutionRoute,
    /// The identity of the admitted call this context runs.
    pub(crate) call_id: lash_sansio::ToolCallId,
    pub(crate) attempt_number: u32,
    pub(crate) max_attempts: u32,
    pub(crate) completion: ToolCompletionState,
    pub(crate) parent_invocation: Option<crate::RuntimeInvocation>,
    pub(crate) execution_env_spec: crate::ProcessExecutionEnvSpec,
    pub(crate) child_execution_trace_hook: Option<ToolChildExecutionTraceHook>,
}

#[derive(Clone)]
/// Notification emitted when a tool call's declared start realizes a child process.
pub struct ToolChildProcessStarted {
    /// The minted id of the child process that started.
    pub process_id: ProcessId,
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

pub(crate) struct ToolContextBuilder<'run> {
    owner: crate::ExecutionOwner,
    sessions: Arc<dyn SessionStateService>,
    session_lifecycle: Arc<dyn SessionLifecycleService>,
    processes: Arc<dyn crate::ProcessService>,
    effect_controller: crate::ActorContext,
    runtime_dispatch: Option<Arc<crate::tool_dispatch::ToolDispatchContext<'run>>>,
    runtime_execution_context: Option<crate::RuntimeExecutionContext<'run>>,
    cancellation_token: Option<tokio_util::sync::CancellationToken>,
    enclosing_process: Option<ProcessId>,
    attachment_store: Arc<crate::RuntimeAttachmentStore>,
    direct_completions: crate::DirectCompletionClient<'run>,
    prepared_payload: serde_json::Value,
    tool_execution_binding: serde_json::Value,
    tool_execution_route: ToolExecutionRoute,
    call_id: lash_sansio::ToolCallId,
    completion: ToolCompletionState,
    parent_invocation: Option<crate::RuntimeInvocation>,
    execution_env_spec: crate::ProcessExecutionEnvSpec,
    child_execution_trace_hook: Option<ToolChildExecutionTraceHook>,
}

impl<'run> ToolContextBuilder<'run> {
    /// The context of the admitted `call`, dispatched under `dispatch`.
    pub(crate) fn from_dispatch(
        dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
        call: &PreparedToolCall,
    ) -> Self {
        Self {
            owner: dispatch.owner.clone(),
            sessions: Arc::clone(&dispatch.sessions),
            session_lifecycle: Arc::clone(&dispatch.session_lifecycle),
            processes: Arc::clone(&dispatch.processes),
            effect_controller: dispatch.effect_controller.clone(),
            runtime_dispatch: Some(Arc::clone(&dispatch)),
            runtime_execution_context: None,
            cancellation_token: None,
            enclosing_process: None,
            attachment_store: Arc::clone(&dispatch.attachment_store),
            direct_completions: dispatch.direct_completions.clone(),
            prepared_payload: call.prepared_payload.clone(),
            tool_execution_binding: serde_json::Value::Null,
            tool_execution_route: ToolExecutionRoute::Catalog,
            call_id: call.call_id.clone(),
            completion: ToolCompletionState::default(),
            parent_invocation: dispatch.parent_invocation.clone(),
            execution_env_spec: dispatch.execution_env_spec.clone(),
            child_execution_trace_hook: None,
        }
    }

    pub(crate) fn cancellation_token(
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
    /// hosts write.
    pub(crate) fn enclosing_process(mut self, process_id: Option<ProcessId>) -> Self {
        self.enclosing_process = process_id;
        self
    }

    pub(crate) fn parent_invocation(mut self, metadata: Option<crate::RuntimeInvocation>) -> Self {
        self.parent_invocation = metadata;
        self
    }

    pub(crate) fn child_execution_trace_hook(
        mut self,
        hook: Option<ToolChildExecutionTraceHook>,
    ) -> Self {
        self.child_execution_trace_hook = hook;
        self
    }

    pub(crate) fn build(self) -> ToolContext<'run> {
        ToolContext {
            owner: self.owner,
            sessions: self.sessions,
            session_lifecycle: self.session_lifecycle,
            processes: self.processes,
            effect_controller: self.effect_controller,
            runtime_dispatch: self.runtime_dispatch,
            runtime_execution_context: self.runtime_execution_context,
            cancellation_token: self.cancellation_token,
            enclosing_process: self.enclosing_process,
            attachment_store: self.attachment_store,
            direct_completions: self.direct_completions,
            prepared_payload: self.prepared_payload,
            tool_execution_binding: self.tool_execution_binding,
            tool_execution_route: self.tool_execution_route,
            call_id: self.call_id,
            attempt_number: 1,
            max_attempts: 1,
            completion: self.completion,
            parent_invocation: self.parent_invocation,
            execution_env_spec: self.execution_env_spec,
            child_execution_trace_hook: self.child_execution_trace_hook,
        }
    }
}

impl<'run> ToolContext<'run> {
    /// The lineage of the process this tool call runs inside (FIG-3607 R1):
    /// the runtime context's when the call has one, else the dispatch's.
    pub(crate) fn process_lineage(&self) -> Option<crate::ProcessLineage> {
        self.runtime_execution_context
            .as_ref()
            .and_then(crate::RuntimeExecutionContext::process_lineage)
            .or_else(|| {
                self.runtime_dispatch
                    .as_ref()
                    .and_then(|dispatch| dispatch.process_lineage.clone())
            })
    }

    /// Hands the call the key of the completion wait its round pinned.
    pub(crate) fn install_completion_key(&self, key: Option<crate::PinnedKey>) {
        if let Some(key) = key {
            self.completion.store(key);
        }
    }
    pub(crate) fn replay_validation_trace(&self) -> Option<crate::RuntimeEffectReplayTrace> {
        self.runtime_execution_context
            .as_ref()
            .and_then(crate::RuntimeExecutionContext::replay_validation_trace)
    }

    pub(crate) fn to_static(&self) -> Option<ToolContext<'static>> {
        Some(ToolContext {
            owner: self.owner.clone(),
            sessions: Arc::clone(&self.sessions),
            session_lifecycle: Arc::clone(&self.session_lifecycle),
            processes: Arc::clone(&self.processes),
            effect_controller: self.effect_controller.clone(),
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
            attachment_store: Arc::clone(&self.attachment_store),
            direct_completions: self.direct_completions.to_static()?,
            prepared_payload: self.prepared_payload.clone(),
            tool_execution_binding: self.tool_execution_binding.clone(),
            tool_execution_route: self.tool_execution_route.clone(),
            call_id: self.call_id.clone(),
            attempt_number: self.attempt_number,
            max_attempts: self.max_attempts,
            completion: self.completion.clone(),
            parent_invocation: self.parent_invocation.clone(),
            execution_env_spec: self.execution_env_spec.clone(),
            child_execution_trace_hook: self.child_execution_trace_hook.clone(),
        })
    }

    #[cfg(any(test, feature = "testing"))]
    #[expect(
        clippy::expect_used,
        reason = "test-only builder: `FrameNodeId::new` rejects only the empty string, and the literal here is not"
    )]
    pub(crate) fn builder(
        session_id: SessionId,
        sessions: Arc<dyn SessionStateService>,
        session_lifecycle: Arc<dyn SessionLifecycleService>,
        processes: Arc<dyn crate::ProcessService>,
        effect_controller: crate::ActorContext,
        attachment_store: Arc<crate::RuntimeAttachmentStore>,
        direct_completions: crate::DirectCompletionClient<'run>,
    ) -> ToolContextBuilder<'run> {
        ToolContextBuilder {
            owner: crate::ExecutionOwner::SessionFrame {
                session_id,
                agent_frame_id: crate::FrameNodeId::new("test-frame")
                    .expect("test frame identity is non-empty"),
            },
            sessions,
            session_lifecycle,
            processes,
            effect_controller,
            runtime_dispatch: None,
            runtime_execution_context: None,
            cancellation_token: None,
            enclosing_process: None,
            attachment_store,
            direct_completions,
            prepared_payload: serde_json::Value::Null,
            tool_execution_binding: serde_json::Value::Null,
            tool_execution_route: ToolExecutionRoute::Catalog,
            call_id: lash_sansio::ToolCallId::fixture("tool-context"),
            completion: ToolCompletionState::default(),
            parent_invocation: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::AdmittedPluginConfig::default(),
                crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                ),
            ),
            child_execution_trace_hook: None,
        }
    }

    pub fn from_dispatch(
        dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
        call: &PreparedToolCall,
    ) -> ToolContextBuilder<'run> {
        ToolContextBuilder::from_dispatch(dispatch, call)
    }

    /// Exposes the owner to protocol and process-engine implementors while
    /// preparing or executing an authorized tool call.
    #[cfg(any(test, feature = "testing"))]
    pub fn owner(&self) -> &crate::ExecutionOwner {
        &self.owner
    }

    /// Exposes cooperative cancellation to tool implementors, returning `None` when the execution
    /// boundary supplied no cancellation scope.
    pub fn cancellation_token(&self) -> Option<&tokio_util::sync::CancellationToken> {
        self.cancellation_token.as_ref()
    }

    /// Exposes the process this call executes inside to protocol and process-engine
    /// implementors while preparing or executing an authorized tool call.
    #[cfg(any(test, feature = "testing"))]
    pub fn enclosing_process(&self) -> Option<&ProcessId> {
        self.enclosing_process.as_ref()
    }

    /// The identity of the admitted call this context runs.
    #[cfg(any(test, feature = "testing"))]
    pub fn call_id(&self) -> &lash_sansio::ToolCallId {
        &self.call_id
    }

    /// Lends the call the cancellation token of its recorded Run step body.
    pub(crate) fn with_step_stop(mut self, stop: tokio_util::sync::CancellationToken) -> Self {
        self.cancellation_token = Some(stop);
        self
    }

    pub(crate) fn take_completion_key(&self) -> Option<crate::PinnedKey> {
        self.completion.take()
    }

    pub(crate) fn with_attempt(mut self, attempt_number: u32, max_attempts: u32) -> Self {
        self.attempt_number = attempt_number.max(1);
        self.max_attempts = max_attempts.max(1);
        self
    }

    pub(crate) fn with_prepared_payload(mut self, payload: serde_json::Value) -> Self {
        self.prepared_payload = payload;
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
        self.runtime_execution_context = self.runtime_execution_context.map(|context| {
            context.for_tool_attempt(Arc::clone(&dispatch), parent_invocation.clone())
        });
        self.runtime_dispatch = Some(dispatch);
        self.parent_invocation = Some(parent_invocation);
        self
    }
}

/// Runtime-prepared executable tool call.
///
/// `call_id` is the call's admitted identity (ADR 0117): the prepare phase
/// cannot change it. The provider's own id stays beside it as correlation,
/// and any argument rewrites and provider-owned context projections are
/// frozen before the call crosses a runtime effect or process boundary.
// `PartialEq` but not `Eq`: `args` and `prepared_payload` are `serde_json::Value`.
// Comparison verifies retained logical call admission and replay.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreparedToolCall {
    pub call_id: lash_sansio::ToolCallId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_call_id: Option<String>,
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
            provider_call_id: call.provider_call_id,
            tool_id,
            tool_name: call.tool_name,
            args: call.args,
            replay: call.replay,
            prepared_payload: serde_json::Value::Null,
        }
    }

    /// Seals `payload` as the call's prepared payload.
    #[must_use]
    pub fn with_prepared_payload(mut self, payload: serde_json::Value) -> Self {
        self.prepared_payload = payload;
        self
    }
}

/// One ordered child inside a runtime-prepared tool batch. Its attempts are
/// keyed by the call's own id, under the batch's group.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedToolBatchCall {
    pub call: PreparedToolCall,
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
        Self::new_with_grants(
            batch_id,
            calls.into_iter().map(|call| (call, None)).collect(),
        )
    }

    pub(crate) fn new_with_grants(
        batch_id: impl Into<crate::BatchId>,
        calls: Vec<(PreparedToolCall, Option<ToolExecutionGrant>)>,
    ) -> Self {
        Self {
            batch_id: batch_id.into(),
            calls: calls
                .into_iter()
                .map(|(call, execution_grant)| PreparedToolBatchCall {
                    call,
                    execution_grant: execution_grant.map(Box::new),
                })
                .collect(),
        }
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
// float arm has no total equality. Comparison verifies that retained
// admission preserves the granted authority.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolExecutionGrant {
    /// The plugin whose declared code executes this grant.
    pub owner: crate::plugin::PluginRevision,
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
    pub fn from_definition(
        owner: crate::plugin::PluginRevision,
        definition: ToolDefinition,
    ) -> Self {
        Self {
            owner,
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
    fleet_format: crate::FleetFormat,
    owner: crate::RuntimeOwner,
    sessions: Arc<dyn SessionStateService>,
    /// The catalog the call is dispatched against: a process owner's catalog
    /// reads answer from it.
    tool_catalog: Option<Arc<crate::ToolCatalog>>,
    turn_context: crate::TurnContext,
    call_id: lash_sansio::ToolCallId,
    tool_execution_binding: serde_json::Value,
    tool_execution_route: ToolExecutionRoute,
    /// The originator of the process chain the call runs in, when it runs
    /// inside a process.
    process_originator: Option<crate::ProcessOriginator>,
}

impl ToolPrepareContext {
    pub fn fleet_format(&self) -> crate::FleetFormat {
        self.fleet_format
    }
    pub(crate) fn with_execution_binding(
        owner: crate::RuntimeOwner,
        sessions: Arc<dyn SessionStateService>,
        turn_context: crate::TurnContext,
        call_id: lash_sansio::ToolCallId,
        tool_execution_binding: serde_json::Value,
        fleet_format: crate::FleetFormat,
    ) -> Self {
        Self {
            fleet_format,
            owner,
            sessions,
            tool_catalog: None,
            turn_context,
            call_id,
            tool_execution_binding,
            tool_execution_route: ToolExecutionRoute::Catalog,
            process_originator: None,
        }
    }

    pub(crate) fn with_process_originator(
        mut self,
        process_originator: Option<crate::ProcessOriginator>,
    ) -> Self {
        self.process_originator = process_originator;
        self
    }

    /// A prepare context for tests of a provider's `prepare` step.
    #[cfg(any(test, feature = "testing"))]
    pub fn for_testing(
        owner: crate::RuntimeOwner,
        sessions: Arc<dyn SessionStateService>,
        process_originator: Option<crate::ProcessOriginator>,
    ) -> Self {
        Self::with_execution_binding(
            owner,
            sessions,
            crate::TurnContext::default(),
            lash_sansio::ToolCallId::fixture("prepare-for-testing"),
            serde_json::Value::Null,
            crate::FleetFormat::current(),
        )
        .with_process_originator(process_originator)
    }

    /// The originator of the process chain the call runs in, or `None`
    /// outside a process. A child session that a call inside a process
    /// creates parents under the originator's session: the process has no
    /// session of its own.
    pub fn process_originator(&self) -> Option<&crate::ProcessOriginator> {
        self.process_originator.as_ref()
    }

    pub(crate) fn with_dispatch_catalog(mut self, catalog: Arc<crate::ToolCatalog>) -> Self {
        self.tool_catalog = Some(catalog);
        self
    }

    pub(crate) fn with_granted_source_id(mut self, source_id: Option<String>) -> Self {
        self.tool_execution_route = ToolExecutionRoute::Granted { source_id };
        self
    }

    pub(crate) fn execution_route(&self) -> &ToolExecutionRoute {
        &self.tool_execution_route
    }

    /// Who the call being prepared runs for.
    pub fn owner(&self) -> &crate::RuntimeOwner {
        &self.owner
    }

    /// The session the call is prepared in, or
    /// [`PluginError::NotASessionRuntime`] inside a process.
    pub fn session_id(&self) -> Result<&SessionId, PluginError> {
        match &self.owner {
            crate::RuntimeOwner::Session(session_id) => Ok(session_id),
            crate::RuntimeOwner::Process(process_id) => Err(crate::runtime::not_a_session_runtime(
                "prepare_session_id",
                process_id,
            )),
        }
    }

    /// The admitted identity of the call being prepared.
    pub fn call_id(&self) -> &lash_sansio::ToolCallId {
        &self.call_id
    }

    pub fn tool_execution_binding(&self) -> &serde_json::Value {
        &self.tool_execution_binding
    }

    pub fn turn_context(&self) -> &crate::TurnContext {
        &self.turn_context
    }

    /// Snapshots the current session for protocol and tool implementors preparing an authorized
    /// call; failures preserve the plugin error contract.
    pub async fn session_snapshot(&self) -> Result<SessionSnapshot, PluginError> {
        self.sessions.snapshot_session(self.session_id()?).await
    }

    /// The snapshot of an explicitly named session, under either owner.
    pub async fn snapshot_session(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionSnapshot, PluginError> {
        self.sessions.snapshot_session(session_id).await
    }

    pub async fn tool_catalog(&self) -> Result<Vec<serde_json::Value>, PluginError> {
        match &self.owner {
            crate::RuntimeOwner::Session(session_id) => {
                self.sessions.tool_catalog(session_id).await
            }
            crate::RuntimeOwner::Process(_) => Ok(self.pinned_catalog()?.as_ref().clone()),
        }
    }

    /// Returns the shared canonical catalog snapshot for protocol and tool implementors that
    /// prepare multiple calls without rebuilding the projection.
    pub async fn shared_tool_catalog(
        &self,
    ) -> Result<std::sync::Arc<Vec<serde_json::Value>>, PluginError> {
        match &self.owner {
            crate::RuntimeOwner::Session(session_id) => {
                self.sessions.shared_tool_catalog(session_id).await
            }
            crate::RuntimeOwner::Process(_) => self.pinned_catalog(),
        }
    }

    fn pinned_catalog(&self) -> Result<Arc<Vec<serde_json::Value>>, PluginError> {
        let catalog = self.tool_catalog.as_ref().ok_or_else(|| {
            PluginError::Session(format!(
                "`{}` has no pinned tool catalog outside a runtime dispatch",
                self.owner
            ))
        })?;
        Ok(Arc::new(crate::tool_registry::project_tool_catalog(
            catalog.tools.iter().cloned(),
        )))
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

    /// The admitted manifest, including the provider's recorded execution binding.
    pub fn manifest(&self) -> &'a ToolManifest {
        self.manifest
    }
}

/// Trait for providing leaf tools to the sandbox.
///
/// Implementations supply cheap [`ToolManifest`]s, lazily resolved
/// [`ToolContract`]s, and a single required
/// [`execute`](Self::execute) method that handles every call. Tools that
/// need session state read it from `call.context`.
///
/// # Declaration
///
/// What a body may do beyond an inline Done result is declared on its
/// manifest as a [`ToolDeclaration`](crate::ToolDeclaration): `may_defer`
/// for a body that returns Deferred, `intents` for the Lash intent kinds a
/// Done result declares, and `isolated` for a call that runs as a process
/// from its start. Admission records the declaration with the manifest the
/// call was admitted under, and the runtime reads only that recorded answer:
/// a crash redelivery, a recovered group child or a replayed cell keeps the
/// capabilities it was admitted with, whatever the provider answers now. An
/// outcome the declaration does not admit — Deferred without `may_defer`, an
/// undeclared intent kind — is refused before anything it declares is
/// realized. An isolated call is bound by its provider's
/// [`isolated_process`](Self::isolated_process) to a registered process
/// engine; one the provider does not bind refuses at admission, before any
/// body runs.
///
/// Lash contains an `execute` panic as a typed call failure. Containment does
/// not establish that the host object's own interior-mutability state still
/// satisfies its invariants; hosts own replacement or repair before reuse.
///
/// # Delivery
///
/// A call is delivered at least once. A crash between a tool's effect and
/// the durable record of its outcome runs `execute` again, and a reported
/// failure the retry policy allows is retried. Lash does not deduplicate a
/// tool's effects for it.
///
/// Key idempotency on [`AttemptContext::call_id`]. It is the `ToolCallId`
/// lash mints when it admits the call (ADR 0117): every run of one logical
/// call — a crash replay, a retry after a reported failure, a redrive —
/// sees the same id, and every other call sees another one, even when the
/// model's provider repeats its own call id. The provider's id is never
/// handed to a tool. [`AttemptContext::attempt_number`] counts the runs
/// separately: a crash redelivers the same attempt number, and only a
/// reported failure the retry policy accepts advances it. A tool that wants
/// a fresh key per attempt combines the two itself.
///
/// The usual patterns: pass the call id as the idempotency key of an
/// external API that accepts one; record "call id done" in the same
/// transaction as the side effect; or check for an existing effect keyed by
/// the call id before making it. There is no idempotent capability and no
/// per-call timeout: a body that talks to a slow service bounds its own
/// wait and reports the failure.
///
/// Effects Lash owns — process starts, signals, cancels, events, triggers and
/// definitions — are not side effects of the body. A body returns them as
/// declared [`ToolIntents`](crate::ToolIntents), which the runtime realizes
/// once per intent identity after the attempt's outcome is durable, behind
/// a first-outcome fence keyed by the call id and the intent's index. A
/// redelivered attempt that declares the same intents realizes none twice.
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
    /// Bind an isolated call (its manifest declares `isolated`) to the
    /// registered process engine that runs it. Admission asks once, before
    /// any preparation, check or body, and records the answer; a replay or
    /// recovery never asks again. `None`, the default, refuses the call at
    /// admission: an isolated call never falls back to `execute`.
    fn isolated_process(
        &self,
        _call: IsolatedProcessRequest<'_>,
    ) -> Option<IsolatedProcessBinding> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // FIG-3417: the lifecycle parent a child start declares comes from ONE
    // shared derivation — the admitted execution scope, which for a process
    // names its minted id. Nothing on this path reads a registry.
    // -----------------------------------------------------------------------

    fn tool_context_under_scope(admitted: crate::AdmittedScope) -> ToolContext<'static> {
        let controller = crate::ActorContext::unavailable()
            .scoped(admitted)
            .expect("the test scope validates");
        ToolContext::builder(
            SessionId::from("session-1"),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::UnavailableProcessService),
            controller,
            Arc::new(crate::RuntimeAttachmentStore::unavailable()),
            crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
        )
        .build()
    }

    /// A recorded leaf attempt under a process scope names its start context
    /// from the admitted scope plus the process's recorded lineage. With no
    /// lineage the context refuses rather than inventing roots: a process
    /// body's ancestors are recorded facts, never a registry lookup.
    #[tokio::test]
    async fn an_attempt_under_a_process_scope_without_its_lineage_is_refused() {
        let context = tool_context_under_scope(crate::AdmittedScope::process(
            crate::process_id_for_test("worker"),
        ));
        let attempt = crate::testing::ToolCallFixture { context }.attempt("attempt-scope");
        assert!(
            attempt.start_cx().is_err(),
            "a process opener without its lineage has no start context"
        );
    }
}
