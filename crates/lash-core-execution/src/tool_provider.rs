pub(crate) use completion_support::AttemptCompletionSupport;
use lash_vocabulary::identity::ProcessId;
use lash_vocabulary::identity::SessionId;
use std::sync::{Arc, Mutex};

use lash_protocol_types::llm::types::ProviderReplayMeta;
use lash_protocol_types::sync::MutexExt;
use serde::{Deserialize, Serialize};

use crate::ToolOutcome;
use crate::plugin::OwnerConfigView;
use crate::plugin::SessionLifecycleService;
use crate::plugin::SessionSnapshot;
use crate::plugin::SessionStateService;
use lash_execution_contracts::plugin_error::PluginError;
use lash_vocabulary::tool_contract::ToolContract;
use lash_vocabulary::tool_contract::ToolId;
use lash_vocabulary::tool_contract::ToolManifest;

mod attachments;
mod completion_support;
mod direct_completion;
mod grant;
mod isolation;
mod live;
mod session;

pub use attachments::ToolAttachmentClient;
pub use direct_completion::ToolDirectCompletionClient;
pub use grant::ToolExecutionGrant;
pub use isolation::{IsolatedProcessRequest, IsolatedStart};

/// Why a start's definition target did not resolve.
#[derive(Debug, thiserror::Error)]
pub enum ResolveTargetError {
    /// The definition is not stored, or refutes the claim made of it: the
    /// start's tool error.
    #[error("{0}")]
    Refused(String),
    /// A store fault: retryable, never recorded as a refusal.
    #[error(transparent)]
    Fault(PluginError),
}
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
    owner: lash_vocabulary::identity::RuntimeOwner,
    sessions: Arc<dyn SessionStateService>,
    /// The policy of the environment the attempt runs under: a process
    /// owner's model reads answer from it.
    facts: crate::plugin::ExecutionFacts,
    /// The catalog the attempt was dispatched against: a process owner's
    /// catalog reads answer from it. `None` outside a runtime dispatch.
    tool_catalog: Option<Arc<crate::ToolCatalog>>,
}

impl AttemptSessionReads {
    /// Integrator class 3 read of the attempt owner's effective model policy.
    pub async fn model(&self) -> Result<session::ToolSessionLlmProfile, PluginError> {
        let (model, attachment_acceptance, generation) = match &self.owner {
            lash_vocabulary::identity::RuntimeOwner::Session(session_id) => {
                let policy = self.sessions.snapshot_session(session_id).await?.policy;
                (
                    policy.model,
                    policy.attachment_acceptance,
                    policy.generation,
                )
            }
            lash_vocabulary::identity::RuntimeOwner::Process(_) => (
                self.facts.model.clone(),
                Arc::clone(&self.facts.attachment_acceptance),
                self.facts.generation.clone(),
            ),
        };
        let Some(config) = model else {
            return Err(PluginError::Session(
                "the attempt owner has selected no model".to_string(),
            ));
        };
        Ok(session::ToolSessionLlmProfile {
            model: config,
            attachment_acceptance,
            generation,
        })
    }

    /// Integrator class 3 snapshot of the bound session without an effect
    /// controller. A process owner has no session and is refused.
    pub async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
        match &self.owner {
            lash_vocabulary::identity::RuntimeOwner::Session(session_id) => {
                self.sessions.snapshot_session(session_id).await
            }
            lash_vocabulary::identity::RuntimeOwner::Process(process_id) => Err(
                crate::runtime::not_a_session_runtime("snapshot_current", process_id),
            ),
        }
    }

    /// Integrator class 3 snapshot of a named session through controller-free reads.
    pub async fn snapshot(&self, session_id: &SessionId) -> Result<SessionSnapshot, PluginError> {
        self.sessions.snapshot_session(session_id).await
    }

    /// Integrator class 3 read of the owner's serialized tool catalog.
    pub async fn tool_catalog(&self) -> Result<Vec<serde_json::Value>, PluginError> {
        match &self.owner {
            lash_vocabulary::identity::RuntimeOwner::Session(session_id) => {
                self.sessions.tool_catalog(session_id).await
            }
            lash_vocabulary::identity::RuntimeOwner::Process(_) => {
                Ok(self.pinned_catalog()?.as_ref().clone())
            }
        }
    }

    /// Integrator class 3 shared read of the immutable serialized tool catalog.
    pub async fn shared_tool_catalog(&self) -> Result<Arc<Vec<serde_json::Value>>, PluginError> {
        match &self.owner {
            lash_vocabulary::identity::RuntimeOwner::Session(session_id) => {
                self.sessions.shared_tool_catalog(session_id).await
            }
            lash_vocabulary::identity::RuntimeOwner::Process(_) => self.pinned_catalog(),
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
            catalog.entries().iter().cloned(),
        )))
    }
}

/// Integrator class 3 controller-free process reads for a recorded leaf attempt.
#[derive(Clone)]
pub struct AttemptProcessReads {
    owner: lash_vocabulary::identity::RuntimeOwner,
    processes: Arc<dyn crate::ProcessService>,
}

impl AttemptProcessReads {
    /// Integrator class 3 listing through the attempt-safe process filter.
    pub async fn list_handles_filtered(
        &self,
        filter: &lash_execution_contracts::process::ProcessListFilter,
    ) -> Result<Vec<lash_execution_contracts::process::ProcessHandleView>, PluginError> {
        Ok(self
            .processes
            .list_visible_for_attempt(&self.owner, filter.list_mode())
            .await?
            .into_iter()
            .filter(|record| filter.matches_observed(record))
            .map(lash_execution_contracts::process::ProcessHandleView::from_observed)
            .collect())
    }
}

/// Runtime-only route selected by the dispatcher for one authorized call.
///
/// The route is deliberately private to the core so provider authors observe
/// only the ordinary prepare/execute APIs and their existing execution
/// binding. Registry-backed granted calls still need their provider's
/// identity, however, because that provider is intentionally outside the
/// pinned catalog.
#[derive(Clone, Default)]
pub(crate) enum ToolExecutionRoute {
    #[default]
    Catalog,
    Granted {
        provider: lash_vocabulary::providers::ToolProviderHandle,
    },
}

/// Integrator class 3 sealed, controller-free environment for a recorded leaf attempt.
#[derive(Clone)]
pub struct AttemptContext<'run> {
    history: Option<Arc<crate::ReadHistory>>,
    admission_now: lash_durable::DurableInstant,
    live: crate::ToolLivePublisher,
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
    tool_execution_binding: Option<lash_vocabulary::providers::ExecutionBinding>,
    /// The run's resolved config the attempt executes under.
    config: crate::plugin::OwnerConfigView,
    call_id: lash_protocol_types::ToolCallId,
    attempt_number: u32,
    max_attempts: u32,
    /// The enclosing process's full recorded provenance, also supplied to
    /// child starts. `None` when the attempt runs outside a process.
    process_spawn_provenance: Option<lash_execution_contracts::process::ProcessProvenance>,
    process_lineage: Option<lash_execution_contracts::process::ProcessLineage>,
    /// The process this attempt runs inside, with its recorded environment
    /// and settings: what a nested start's policy may inherit. `None`
    /// outside a process execution.
    enclosing_env: Option<Arc<crate::EnclosingProcess>>,
    /// The execution scope the attempt runs under: what a launch it
    /// declares is held under.
    execution_scope: crate::ExecutionScope,
    completion: AttemptCompletionSupport,
    phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
    tool_execution_route: ToolExecutionRoute,
    /// The catalog the attempt was dispatched against. `None` outside a
    /// runtime dispatch.
    tool_catalog: Option<Arc<crate::ToolCatalog>>,
    /// The routes of the surface the attempt was admitted under: what a
    /// start's capture links from. `None` outside a runtime dispatch.
    routes: Option<crate::SurfaceTools>,
    linked_execution: Option<crate::LinkedExecution>,
    resolution_pins: std::collections::BTreeMap<
        lash_vocabulary::names::OperationUse,
        lash_core_store::resolutions::ResolutionEntry,
    >,
    definition_engines: crate::ProcessEngineRegistry,
    pub(crate) resolved_run: Option<Arc<crate::ResolvedRun>>,
    /// The environments [`declare_launch`](Self::declare_launch) held under
    /// the attempt's execution claim: the only ones a launch the attempt
    /// declares may name (D10).
    held_launches: Arc<Mutex<Vec<crate::ProcessEnvRef>>>,
    /// Where the body stands in the trace: under the attempt's tool span,
    /// bound from the provider the call routes to and revoked when the body
    /// exits.
    trace: crate::trace::ExtensionTrace,
}

mod attempt;

#[derive(Clone, Default)]
pub(crate) struct ToolCompletionState {
    key: Arc<Mutex<Option<lash_execution_contracts::waits::PinnedKey>>>,
}

impl ToolCompletionState {
    pub(crate) fn store(&self, key: lash_execution_contracts::waits::PinnedKey) {
        let mut guard = self.key.lock_recover();
        if guard.is_none() {
            *guard = Some(key);
        }
    }

    pub(crate) fn take(&self) -> Option<lash_execution_contracts::waits::PinnedKey> {
        self.key.lock_recover().take()
    }

    pub(crate) fn load(&self) -> Option<lash_execution_contracts::waits::PinnedKey> {
        self.key.lock_recover().clone()
    }
}

/// The runtime's per-call dispatch state for one tool call. It is
/// crate-private: a provider body receives only the [`AttemptContext`]
/// projected from it, which reaches no dispatch, process administration or
/// effect controller.
#[derive(Clone)]
pub(crate) struct ToolContext<'run> {
    pub(crate) admission_now: Option<lash_durable::DurableInstant>,
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
    pub(crate) tool_execution_binding: Option<lash_vocabulary::providers::ExecutionBinding>,
    tool_execution_route: ToolExecutionRoute,
    /// The identity of the admitted call this context runs.
    pub(crate) call_id: lash_protocol_types::ToolCallId,
    pub(crate) attempt_number: u32,
    pub(crate) max_attempts: u32,
    pub(crate) completion: ToolCompletionState,
    pub(crate) parent_invocation: Option<crate::RuntimeInvocation>,
    pub(crate) facts: crate::plugin::ExecutionFacts,
    pub(crate) child_execution_trace_hook: Option<ToolChildExecutionTraceHook>,
    /// The live step of the recorded attempt this context's body runs in:
    /// bound by the engine's step wrapper when the body really runs, never
    /// when the journal serves it.
    pub(crate) live: Option<Arc<crate::trace::LiveStep>>,
    /// The registered provider the call's admitted binding resolved to:
    /// what its body's capability is bound from.
    pub(crate) executable: Option<lash_vocabulary::providers::PluginCallbackIdentity>,
}

#[derive(Clone)]
/// Notification emitted when a tool call's declared start realizes a child process.
pub struct ToolChildProcessStarted {
    /// The minted id of the child process that started.
    pub process_id: ProcessId,
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
    tool_execution_binding: Option<lash_vocabulary::providers::ExecutionBinding>,
    tool_execution_route: ToolExecutionRoute,
    call_id: lash_protocol_types::ToolCallId,
    completion: ToolCompletionState,
    parent_invocation: Option<crate::RuntimeInvocation>,
    facts: crate::plugin::ExecutionFacts,
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
            tool_execution_binding: None,
            tool_execution_route: ToolExecutionRoute::Catalog,
            call_id: call.call_id.clone(),
            completion: ToolCompletionState::default(),
            parent_invocation: dispatch.parent_invocation.clone(),
            facts: dispatch.facts.clone(),
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
            admission_now: None,
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
            facts: self.facts,
            child_execution_trace_hook: self.child_execution_trace_hook,
            live: None,
            executable: None,
        }
    }
}

impl<'run> ToolContext<'run> {
    /// The run's resolved config the call executes under: its dispatch's,
    /// or none outside a runtime dispatch.
    pub(crate) fn config(&self) -> crate::plugin::OwnerConfigView {
        self.runtime_dispatch.as_ref().map_or_else(
            || crate::plugin::OwnerConfigView::session(crate::AdmittedPluginConfig::default()),
            |dispatch| dispatch.config.clone(),
        )
    }

    /// The lineage of the process this tool call runs inside (FIG-3607 R1):
    /// the runtime context's when the call has one, else the dispatch's.
    pub(crate) fn process_lineage(
        &self,
    ) -> Option<lash_execution_contracts::process::ProcessLineage> {
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
    pub(crate) fn install_completion_key(
        &self,
        key: Option<lash_execution_contracts::waits::PinnedKey>,
    ) {
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
            admission_now: self.admission_now,
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
            facts: self.facts.clone(),
            child_execution_trace_hook: self.child_execution_trace_hook.clone(),
            live: self.live.clone(),
            executable: self.executable.clone(),
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
        let policy = crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        );
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
            tool_execution_binding: None,
            tool_execution_route: ToolExecutionRoute::Catalog,
            call_id: lash_protocol_types::ToolCallId::fixture("tool-context"),
            completion: ToolCompletionState::default(),
            parent_invocation: None,
            facts: crate::plugin::ExecutionFacts::of_policy(&policy, None),
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
    pub fn call_id(&self) -> &lash_protocol_types::ToolCallId {
        &self.call_id
    }

    /// Lends the call the cancellation token of its recorded Run step body.
    pub(crate) fn with_step_stop(mut self, stop: tokio_util::sync::CancellationToken) -> Self {
        self.cancellation_token = Some(stop);
        self
    }

    pub(crate) fn take_completion_key(&self) -> Option<lash_execution_contracts::waits::PinnedKey> {
        self.completion.take()
    }

    pub(crate) fn with_attempt(mut self, attempt_number: u32, max_attempts: u32) -> Self {
        self.attempt_number = attempt_number.max(1);
        self.max_attempts = max_attempts.max(1);
        self
    }

    /// Names the registered provider the call's admitted binding resolved
    /// to.
    pub(crate) fn with_executable(
        mut self,
        identity: lash_vocabulary::providers::PluginCallbackIdentity,
    ) -> Self {
        self.executable = Some(identity);
        self
    }

    pub(crate) fn with_prepared_payload(mut self, payload: serde_json::Value) -> Self {
        self.prepared_payload = payload;
        self
    }

    pub(crate) fn with_tool_execution_binding(
        mut self,
        binding: Option<lash_vocabulary::providers::ExecutionBinding>,
    ) -> Self {
        self.tool_execution_binding = binding;
        self
    }

    pub(crate) fn with_granted_provider(
        mut self,
        provider: lash_vocabulary::providers::ToolProviderHandle,
    ) -> Self {
        self.tool_execution_route = ToolExecutionRoute::Granted { provider };
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
    pub call_id: lash_protocol_types::ToolCallId,
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
    pub batch_id: lash_vocabulary::identity::BatchId,
    pub calls: Vec<PreparedToolBatchCall>,
}

impl PreparedToolBatch {
    /// Freezes source-order prepared calls for protocol and process-engine implementors; execution
    /// may be concurrent, but launch and completion projection retain this order.
    pub fn new(
        batch_id: impl Into<lash_vocabulary::identity::BatchId>,
        calls: Vec<PreparedToolCall>,
    ) -> Self {
        Self::new_with_grants(
            batch_id,
            calls.into_iter().map(|call| (call, None)).collect(),
        )
    }

    pub(crate) fn new_with_grants(
        batch_id: impl Into<lash_vocabulary::identity::BatchId>,
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

#[derive(Clone)]
pub struct ToolPrepareContext {
    fleet_format: crate::FleetFormat,
    owner: lash_vocabulary::identity::RuntimeOwner,
    sessions: Arc<dyn SessionStateService>,
    /// The catalog the call is dispatched against: a process owner's catalog
    /// reads answer from it.
    tool_catalog: Option<Arc<crate::ToolCatalog>>,
    turn_context: crate::TurnContext,
    call_id: lash_protocol_types::ToolCallId,
    tool_execution_binding: Option<lash_vocabulary::providers::ExecutionBinding>,
    tool_execution_route: ToolExecutionRoute,
    /// The originator of the process chain the call runs in, when it runs
    /// inside a process.
    process_originator: Option<lash_execution_contracts::process::ProcessOriginator>,
    /// The run's resolved config the call is prepared under.
    config: crate::plugin::OwnerConfigView,
}

impl ToolPrepareContext {
    pub fn fleet_format(&self) -> crate::FleetFormat {
        self.fleet_format
    }
    pub(crate) fn with_execution_binding(
        owner: lash_vocabulary::identity::RuntimeOwner,
        sessions: Arc<dyn SessionStateService>,
        turn_context: crate::TurnContext,
        call_id: lash_protocol_types::ToolCallId,
        tool_execution_binding: Option<lash_vocabulary::providers::ExecutionBinding>,
        fleet_format: crate::FleetFormat,
        config: crate::plugin::OwnerConfigView,
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
            config,
        }
    }

    pub(crate) fn with_process_originator(
        mut self,
        process_originator: Option<lash_execution_contracts::process::ProcessOriginator>,
    ) -> Self {
        self.process_originator = process_originator;
        self
    }

    /// A prepare context for tests of a provider's `prepare` step.
    #[cfg(any(test, feature = "testing"))]
    pub fn for_testing(
        owner: lash_vocabulary::identity::RuntimeOwner,
        sessions: Arc<dyn SessionStateService>,
        process_originator: Option<lash_execution_contracts::process::ProcessOriginator>,
    ) -> Self {
        Self::with_execution_binding(
            owner,
            sessions,
            crate::TurnContext::default(),
            lash_protocol_types::ToolCallId::fixture("prepare-for-testing"),
            None,
            crate::FleetFormat::current(),
            crate::plugin::OwnerConfigView::session(crate::AdmittedPluginConfig::default()),
        )
        .with_process_originator(process_originator)
    }

    /// The originator of the process chain the call runs in, or `None`
    /// outside a process. A child session that a call inside a process
    /// creates parents under the originator's session: the process has no
    /// session of its own.
    pub fn process_originator(
        &self,
    ) -> Option<&lash_execution_contracts::process::ProcessOriginator> {
        self.process_originator.as_ref()
    }

    pub(crate) fn with_dispatch_catalog(mut self, catalog: Arc<crate::ToolCatalog>) -> Self {
        self.tool_catalog = Some(catalog);
        self
    }

    pub(crate) fn with_granted_provider(
        mut self,
        provider: lash_vocabulary::providers::ToolProviderHandle,
    ) -> Self {
        self.tool_execution_route = ToolExecutionRoute::Granted { provider };
        self
    }

    pub(crate) fn execution_route(&self) -> &ToolExecutionRoute {
        &self.tool_execution_route
    }

    /// Who the call being prepared runs for.
    pub fn owner(&self) -> &lash_vocabulary::identity::RuntimeOwner {
        &self.owner
    }

    /// The session the call is prepared in, or
    /// [`PluginError::NotASessionRuntime`] inside a process.
    pub fn session_id(&self) -> Result<&SessionId, PluginError> {
        match &self.owner {
            lash_vocabulary::identity::RuntimeOwner::Session(session_id) => Ok(session_id),
            lash_vocabulary::identity::RuntimeOwner::Process(process_id) => Err(
                crate::runtime::not_a_session_runtime("prepare_session_id", process_id),
            ),
        }
    }

    /// The admitted identity of the call being prepared.
    pub fn call_id(&self) -> &lash_protocol_types::ToolCallId {
        &self.call_id
    }

    /// The execution binding of the deferred match this call runs on, its
    /// readability already checked; `None` for a call that carries none.
    pub fn tool_execution_binding(&self) -> Option<&lash_vocabulary::providers::ExecutionBinding> {
        self.tool_execution_binding.as_ref()
    }

    /// The run's resolved config, which a provider prepares the call under.
    pub fn config(&self) -> &crate::plugin::OwnerConfigView {
        &self.config
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
            lash_vocabulary::identity::RuntimeOwner::Session(session_id) => {
                self.sessions.tool_catalog(session_id).await
            }
            lash_vocabulary::identity::RuntimeOwner::Process(_) => {
                Ok(self.pinned_catalog()?.as_ref().clone())
            }
        }
    }

    /// Returns the shared canonical catalog snapshot for protocol and tool implementors that
    /// prepare multiple calls without rebuilding the projection.
    pub async fn shared_tool_catalog(
        &self,
    ) -> Result<std::sync::Arc<Vec<serde_json::Value>>, PluginError> {
        match &self.owner {
            lash_vocabulary::identity::RuntimeOwner::Session(session_id) => {
                self.sessions.shared_tool_catalog(session_id).await
            }
            lash_vocabulary::identity::RuntimeOwner::Process(_) => self.pinned_catalog(),
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
            catalog.entries().iter().cloned(),
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
    contract: &'a ToolContract,
    pub args: &'a serde_json::Value,
    pub context: &'a AttemptContext<'a>,
}

impl<'a> ToolCall<'a> {
    /// Only the runtime dispatcher builds these; the manifest is the coupling between stable
    /// ID and provider-facing name, and the contract is the one the call was prepared under.
    pub fn new(
        manifest: &'a ToolManifest,
        contract: &'a ToolContract,
        args: &'a serde_json::Value,
        context: &'a AttemptContext<'a>,
    ) -> Self {
        Self {
            manifest,
            contract,
            args,
            context,
        }
    }

    /// The contract the call was prepared under: its catalog entry's, or
    /// for a deferred-resolution call its grant's, never re-resolved from
    /// the current catalog. Its output schema is what the call's success
    /// value is admitted against when it settles.
    pub fn contract(&self) -> &'a ToolContract {
        self.contract
    }

    /// The stable tool ID carried by the pinned manifest.
    pub fn tool_id(&self) -> &'a ToolId {
        &self.manifest.id
    }

    /// The tool's operation, by its effect name, carried by the pinned
    /// manifest.
    pub fn name(&self) -> String {
        self.manifest.tool()
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
/// manifest as a [`ToolDeclaration`](lash_vocabulary::tool_declaration::ToolDeclaration): `may_defer`
/// for a body that returns Deferred, `intents` for the Lash intent kinds a
/// Done result declares, and `isolated` for a call that runs as a process
/// from its start. Admission records the declaration with the manifest the
/// call was admitted under, and the runtime reads only that recorded answer:
/// a crash redelivery, a recovered group child or a replayed cell keeps the
/// capabilities it was admitted with, whatever the provider answers now. An
/// outcome the declaration does not admit — Deferred without `may_defer`, an
/// undeclared intent kind — is refused before anything it declares is
/// realized. An isolated call runs in the registered process engine its
/// manifest names, started with the payload its provider's
/// [`isolated_start`](Self::isolated_start) answers. A
/// manifest holds only a valid declaration and its host-set bounds, and an
/// isolated tool registers only where its engine is registered, so no call
/// is refused for its tool's configuration.
///
/// Lash contains an `execute` panic as a typed call failure. Containment does
/// not establish that the host object's own interior-mutability state still
/// satisfies its invariants; hosts own replacement or repair before reuse.
///
/// # Delivery
///
/// Delivery follows the recorded execution policy (ADR 0132 §7 and the
/// [facade tool policy](https://github.com/Ascending-AI/lash/blob/main/crates/lash/src/lib.rs#L386)).
/// `Once` never restarts a started attempt whose outcome is missing and never
/// retries a reported failure; missing-outcome ambiguity settles as
/// `Interrupted`. `Repeatable` may rerun an unrecorded attempt or retry a
/// reported failure only within its bounds, while both the recorded and
/// current policies permit repetition. Lash makes no exactly-once claim for
/// external effects: an effect may have happened before its outcome committed.
/// Lash does not deduplicate those effects for the tool.
///
/// Key idempotency on [`AttemptContext::call_id`]. It is the `ToolCallId`
/// lash mints when it admits the call (ADR 0117): every run of one logical
/// call — a permitted crash rerun, retry after a reported failure or redrive —
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
/// the call id before making it. There is no idempotent capability: execution
/// policy and bounds are separate from the capability declaration. The host
/// supplies a mandatory [execution bound](https://github.com/Ascending-AI/lash/blob/main/crates/lash/src/lib.rs#L450)
/// for each body and a separate [park bound](https://github.com/Ascending-AI/lash/blob/main/crates/lash/src/lib.rs#L450)
/// for a tool that may defer, with no Lash-selected default or ceiling. Lash
/// records and enforces these limits; transport timeouts remain useful but
/// do not replace this contract. Cancellation or a limit can end Lash's wait
/// and drop a body without proving that its external work stopped.
///
/// Effects Lash owns — process starts, cancels and definitions — are not
/// side effects of the body. A body returns them as
/// declared [`ToolIntents`](lash_execution_contracts::tool_intent::ToolIntents), which the runtime realizes
/// once per intent identity after the attempt's outcome is durable, behind
/// a first-outcome fence keyed by the call id and the intent's index. A
/// redelivered attempt that declares the same intents realizes none twice.
#[async_trait::async_trait]
pub trait ToolProvider: Send + Sync + 'static {
    fn refresh_environment(&self) {}
    fn environment_mode(&self) -> lash_core_store::environment::SourceMode {
        lash_core_store::environment::SourceMode::RefreshOnly
    }
    fn environment_config_dependencies(&self) -> std::collections::BTreeSet<String> {
        Default::default()
    }
    fn environment_namespace_dependencies(&self) -> std::collections::BTreeSet<String> {
        Default::default()
    }
    fn subscribe_environment(
        &self,
        _source: &lash_core_store::environment::SourceId,
        _sink: &crate::plugin::environment_sources::EnvironmentChangeSink,
    ) {
    }
    /// Capture definitions and bindings once. A mutable provider overrides this
    /// method under its own synchronization owner to return an atomic revision.
    fn environment_snapshot(
        &self,
        key: &crate::ProviderKey,
        source: &lash_core_store::environment::SourceId,
        revision: lash_core_store::environment::SourceRevision,
        config: &OwnerConfigView,
        _namespace: Option<(
            &lash_vocabulary::package::PublicationDomain,
            &lash_vocabulary::package::PublicationScope,
        )>,
    ) -> Result<lash_core_store::environment::SourceSnapshot, lash_execution_contracts::PluginError>
    {
        let mut tools = Vec::new();
        for manifest in self.tool_manifests(config)? {
            let contract = self
                .resolve_contract(&manifest.tool(), config)
                .ok_or_else(|| {
                    lash_execution_contracts::PluginError::ResidentToolContractUnavailable {
                        tool_id: manifest.id.clone(),
                        operation: manifest.tool(),
                    }
                })?;
            let payload = self
                .link_payload(&manifest, config)
                .map(
                    |(format, payload)| lash_vocabulary::providers::ExecutionBinding {
                        codec: key.clone(),
                        format,
                        payload,
                    },
                );
            tools.push(crate::SurfaceEntry::advertised(
                key.clone(),
                lash_vocabulary::ToolDefinition {
                    manifest,
                    contract: (*contract).clone(),
                },
                payload,
            ));
        }
        Ok(lash_core_store::environment::SourceSnapshot {
            source: source.clone(),
            revision,
            fragment: lash_core_store::environment::EnvironmentFragment {
                tools,
                ..Default::default()
            },
        })
    }

    /// The tools this provider advertises under `config`, the run's resolved
    /// config: which tools exist is the provider's own configuration
    /// (FIG-6074).
    fn tool_manifests(
        &self,
        config: &OwnerConfigView,
    ) -> Result<Vec<ToolManifest>, lash_execution_contracts::plugin_error::PluginError>;
    fn resolve_manifest(&self, name: &str, config: &OwnerConfigView) -> Option<ToolManifest> {
        self.tool_manifests(config)
            .ok()?
            .into_iter()
            .find(|manifest| manifest.tool() == name)
    }
    fn resolve_manifest_by_id(
        &self,
        id: &ToolId,
        config: &OwnerConfigView,
    ) -> Option<ToolManifest> {
        self.tool_manifests(config)
            .ok()?
            .into_iter()
            .find(|manifest| manifest.id == *id)
    }
    fn resolve_contract(&self, name: &str, config: &OwnerConfigView) -> Option<Arc<ToolContract>>;
    fn resolve_contract_by_id(
        &self,
        id: &ToolId,
        config: &OwnerConfigView,
    ) -> Option<Arc<ToolContract>> {
        let manifest = self.resolve_manifest_by_id(id, config)?;
        self.resolve_contract(&manifest.tool(), config)
    }
    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        Ok(PreparedToolCall::identity(call.tool_id, call.pending))
    }
    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome;
    /// The execution binding formats this provider reads, declared with its
    /// key (S8). A recorded binding whose codec is this provider and whose
    /// format is not listed parks, typed, before the call is prepared: the
    /// provider's own reader never sees it.
    fn readable_binding_formats(&self) -> &'static [lash_vocabulary::providers::FormatVersion] {
        &[]
    }
    /// The codec payload a link to the advertised tool `manifest` records:
    /// its format, one of [`Self::readable_binding_formats`], and the
    /// payload, which every call through the link reads back from its
    /// context ([`AttemptContext::tool_execution_binding`]) and nothing
    /// compares. `None`, the default, records none.
    fn link_payload(
        &self,
        _manifest: &ToolManifest,
        _config: &OwnerConfigView,
    ) -> Option<(lash_vocabulary::providers::FormatVersion, serde_json::Value)> {
        None
    }
    /// The start of the process an isolated call (its manifest names an
    /// isolation engine) runs: the engine's payload, the environment and the
    /// engine settings (D6). Admission asks once and records the answer; a
    /// replay or recovery never asks again. An isolated call never falls
    /// back to `execute`, and a provider that states no start refuses it.
    ///
    /// # Errors
    ///
    /// The call's typed refusal, its tool error.
    fn isolated_start(
        &self,
        _call: IsolatedProcessRequest<'_>,
    ) -> Result<IsolatedStart, crate::ProcessEnvRefusal> {
        Err(crate::ProcessEnvRefusal::Policy {
            reason: "this provider states no isolated start".into(),
        })
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
            Arc::new(crate::plugin::ServiceBindings::default()),
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
        let attempt = crate::testing::ToolCallFixture {
            context,
            resolved_run: None,
        }
        .attempt("attempt-scope");
        assert!(
            attempt.start_cx().is_err(),
            "a process opener without its lineage has no start context"
        );
    }
}
