//! Session services behind the runtime's session-facing plugin surface.
//!
//! What lives here (ADR 0089): session initialisation (`session_init` — the
//! `SessionCreateRequest` pipeline and the process-origin port that executes a
//! recorded child's first turn), the process runners, direct completions, and
//! current-session services (`current`, `graph`, `api`) that resolve only the
//! runtime's own session id — a foreign id is an unknown-session error, never
//! a registry lookup. There is no second session model: every executing
//! session is an ordinary session, and a process-spawned child runtime is
//! owned by its process run, not held here.

use super::*;
use lash_protocol_types::sync::MutexExt as _;
use lash_vocabulary::identity::ProcessId;
use lash_vocabulary::identity::SessionId;
use lash_vocabulary::identity::TurnId;
use std::sync::atomic::AtomicBool;

mod api;
mod current;
mod direct;
mod direct_outcome;
mod graph;
mod process_runners;
mod session_init;

pub use crate::direct_completion_client::DirectCompletionClient;

#[derive(Clone)]
enum CurrentSnapshot {
    /// Host-scoped services read a persistence snapshot. Their graph writes
    /// enqueue commands for the session owner.
    Owned(RuntimeSessionState),
    /// Turn-scoped services see a read projection of the running turn's base
    /// state. It is never a durable base: graph appends made through these
    /// services ride the turn's commit draft and are overlaid on this
    /// projection for in-turn readers.
    ReadModel {
        meta: RuntimeSessionState,
        messages: lash_protocol_types::AppendVec<Message>,
        graph_appends: TurnGraphAppendDraft,
    },
}

impl CurrentSnapshot {
    fn to_runtime_state(&self) -> RuntimeSessionState {
        match self {
            Self::Owned(snapshot) => snapshot.clone(),
            Self::ReadModel {
                meta,
                messages,
                graph_appends,
            } => {
                let mut snapshot = meta.clone();
                snapshot.replace_active_read_state(messages.as_slice());
                graph_appends.overlay_on_read_state(&mut snapshot);
                snapshot
            }
        }
    }
}

/// The session a session runtime's services resolve: its id, the state
/// they read, and the store and lane their writes go through.
#[derive(Clone)]
pub(in crate::runtime) struct CurrentSession {
    pub(in crate::runtime) session_id: SessionId,
    snapshot: CurrentSnapshot,
    store: Option<crate::store::SessionStore>,
}

/// Who the services run for. A process runtime is keyed by its minted id: it
/// has no session state, no session store and no frame, and every
/// session-only service refuses it with `PluginError::NotASessionRuntime`.
/// An engine process carries the composition its environment's components
/// make (FIG-6034); a session-turn process carries none (D5).
#[derive(Clone)]
pub(in crate::runtime) enum CurrentOwner {
    Session(Box<CurrentSession>),
    Process {
        process_id: lash_vocabulary::identity::ProcessId,
        engine: Option<Arc<EngineProcessParts>>,
    },
}

/// What an engine process runs under: the environment its start stated, the
/// composition only that environment's components make, and the state sink
/// of its step state.
pub(in crate::runtime) struct EngineProcessParts {
    pub(in crate::runtime) resolutions: Arc<
        std::sync::Mutex<
            std::collections::BTreeMap<
                lash_vocabulary::names::OperationUse,
                lash_core_store::resolutions::ResolutionEntry,
            >,
        >,
    >,
    pub(in crate::runtime) env: Arc<crate::ProcessEnv>,
    pub(in crate::runtime) composition: crate::plugin::ProcessComposition,
    pub(in crate::runtime) state: Arc<crate::plugin::ComponentState>,
}

/// The plugins an owner's services run under: a session's own plugin
/// session, or, for a process, the deployment's session plugin host (a
/// session the process creates resolves against it) and the plugin
/// admission its segment adopted.
#[derive(Clone)]
pub(in crate::runtime) enum OwnerPlugins {
    Session(Arc<crate::PluginSession>),
    Process {
        host: Arc<crate::PluginHost>,
        admission: Arc<std::sync::Mutex<Option<crate::store::plugin_writers::PluginAdmission>>>,
    },
}

impl OwnerPlugins {
    /// The deployment's session plugin host.
    pub(in crate::runtime) fn plugin_host(&self) -> &crate::PluginHost {
        match self {
            Self::Session(plugins) => plugins.host(),
            Self::Process { host, .. } => host,
        }
    }

    /// The plugin admission the owner writes under.
    pub(in crate::runtime) fn plugin_admission(
        &self,
    ) -> Option<crate::store::plugin_writers::PluginAdmission> {
        match self {
            Self::Session(plugins) => plugins.plugin_admission(),
            Self::Process { admission, .. } => admission.lock_recover().clone(),
        }
    }

    fn adopt_plugin_admission(&self, adopted: crate::store::plugin_writers::PluginAdmission) {
        match self {
            Self::Session(plugins) => plugins.adopt_plugin_admission(adopted),
            Self::Process { admission, .. } => *admission.lock_recover() = Some(adopted),
        }
    }
}

#[derive(Clone)]
pub(in crate::runtime) struct CurrentOwnerCapability {
    pub(in crate::runtime) owner: CurrentOwner,
    /// The policy a session's direct calls run under; a process's is its
    /// environment's (`None` for a session-turn process, which makes none).
    policy: Option<SessionPolicy>,
    pub(in crate::runtime) host: RuntimeHost,
    plugins: OwnerPlugins,
    runtime_lease_owner: crate::LeaseOwnerIdentity,
    runtime_lease_executor_id: String,
    turn_phase_probe: Option<Arc<dyn RuntimeTurnPhaseProbe>>,
}

impl CurrentOwnerCapability {
    pub(in crate::runtime) fn runtime_owner(&self) -> lash_vocabulary::identity::RuntimeOwner {
        match &self.owner {
            CurrentOwner::Session(session) => {
                lash_vocabulary::identity::RuntimeOwner::Session(session.session_id.clone())
            }
            CurrentOwner::Process { process_id, .. } => {
                lash_vocabulary::identity::RuntimeOwner::Process(process_id.clone())
            }
        }
    }

    /// The session these services resolve, when a session owns them.
    pub(in crate::runtime) fn session(&self) -> Option<&CurrentSession> {
        match &self.owner {
            CurrentOwner::Session(session) => Some(session.as_ref()),
            CurrentOwner::Process { .. } => None,
        }
    }

    /// The session these services resolve, or `NotASessionRuntime` naming
    /// `operation` for a process runtime.
    pub(in crate::runtime) fn require_session(
        &self,
        operation: &'static str,
    ) -> Result<&CurrentSession, lash_execution_contracts::plugin_error::PluginError> {
        match &self.owner {
            CurrentOwner::Session(session) => Ok(session.as_ref()),
            CurrentOwner::Process { process_id, .. } => Err(
                lash_execution_contracts::plugin_error::PluginError::NotASessionRuntime {
                    operation: operation.to_string(),
                    process_id: process_id.clone(),
                },
            ),
        }
    }

    /// Whether `session_id` is the session these services resolve.
    pub(in crate::runtime) fn is_current_session(&self, session_id: &SessionId) -> bool {
        self.session()
            .is_some_and(|session| session.session_id == *session_id)
    }

    /// Who a dispatch built from these services runs for: the session on its
    /// current agent frame, or the process.
    pub(in crate::runtime) fn execution_owner(
        &self,
    ) -> Result<crate::ExecutionOwner, lash_execution_contracts::plugin_error::PluginError> {
        match &self.owner {
            CurrentOwner::Session(session) => {
                let agent_frame_id = session
                    .snapshot
                    .to_runtime_state()
                    .current_frame_node_id
                    .ok_or_else(|| {
                        lash_execution_contracts::plugin_error::PluginError::Session(format!(
                            "session `{}` has no initialized agent frame",
                            session.session_id
                        ))
                    })?;
                Ok(crate::ExecutionOwner::SessionFrame {
                    session_id: session.session_id.clone(),
                    agent_frame_id,
                })
            }
            CurrentOwner::Process { process_id, .. } => Ok(crate::ExecutionOwner::Process {
                process_id: process_id.clone(),
            }),
        }
    }

    /// The session's own plugin session, or `NotASessionRuntime` naming
    /// `operation` for a process runtime.
    pub(in crate::runtime) fn session_plugins(
        &self,
        operation: &'static str,
    ) -> Result<&Arc<crate::PluginSession>, lash_execution_contracts::plugin_error::PluginError>
    {
        match (&self.plugins, &self.owner) {
            (OwnerPlugins::Session(plugins), _) => Ok(plugins),
            (OwnerPlugins::Process { .. }, CurrentOwner::Process { process_id, .. }) => Err(
                lash_execution_contracts::plugin_error::PluginError::NotASessionRuntime {
                    operation: operation.to_string(),
                    process_id: process_id.clone(),
                },
            ),
            (OwnerPlugins::Process { .. }, CurrentOwner::Session(session)) => Err(
                lash_execution_contracts::plugin_error::PluginError::Session(format!(
                    "session `{}` runs without its plugin session",
                    session.session_id
                )),
            ),
        }
    }

    /// The engine process these services run for, with its composition.
    pub(in crate::runtime) fn engine_process(
        &self,
    ) -> Result<
        (
            &lash_vocabulary::identity::ProcessId,
            &Arc<EngineProcessParts>,
        ),
        lash_execution_contracts::plugin_error::PluginError,
    > {
        match &self.owner {
            CurrentOwner::Process {
                process_id,
                engine: Some(engine),
            } => Ok((process_id, engine)),
            CurrentOwner::Process { process_id, .. } => Err(
                lash_execution_contracts::plugin_error::PluginError::Session(format!(
                    "process `{process_id}` runs a session turn, which has no composition"
                )),
            ),
            CurrentOwner::Session(session) => Err(
                lash_execution_contracts::plugin_error::PluginError::Session(format!(
                    "session `{}` is not a process",
                    session.session_id
                )),
            ),
        }
    }

    /// The policy direct calls run under.
    fn direct_policy(
        &self,
    ) -> Result<&SessionPolicy, lash_execution_contracts::plugin_error::PluginError> {
        self.policy.as_ref().ok_or_else(|| {
            lash_execution_contracts::plugin_error::PluginError::Session(format!(
                "{} makes no model call",
                match &self.owner {
                    CurrentOwner::Process { process_id, .. } =>
                        format!("session-turn process `{process_id}`"),
                    CurrentOwner::Session(session) => format!("session `{}`", session.session_id),
                }
            ))
        })
    }
}

#[derive(Clone)]
struct ProcessCapability {
    sync_needed: Arc<AtomicBool>,
}

#[derive(Clone, Default)]
struct DirectCompletionCapability;

/// What a process runtime's services are built from.
pub(in crate::runtime) struct ProcessServicesPorts {
    pub(in crate::runtime) process_id: lash_vocabulary::identity::ProcessId,
    /// An engine process's environment and composition; `None` for a
    /// session-turn process.
    pub(in crate::runtime) engine: Option<Arc<EngineProcessParts>>,
    /// The policy direct calls run under: the environment's, for an engine
    /// process.
    pub(in crate::runtime) policy: Option<SessionPolicy>,
    pub(in crate::runtime) host: RuntimeHost,
    /// The deployment's session plugin host.
    pub(in crate::runtime) plugin_host: Arc<crate::PluginHost>,
    pub(in crate::runtime) runtime_lease_owner: crate::LeaseOwnerIdentity,
    pub(in crate::runtime) turn_phase_probe: Option<Arc<dyn RuntimeTurnPhaseProbe>>,
}

#[derive(Clone)]
pub struct RuntimeSessionServices {
    current: CurrentOwnerCapability,
    processes: ProcessCapability,
    direct: DirectCompletionCapability,
    direct_replay_ordinals: Arc<std::sync::Mutex<std::collections::BTreeMap<String, u64>>>,
    direct_unkeyed_in_flight: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
}

#[derive(Clone)]
pub(in crate::runtime) struct RuntimeSessionStateService {
    services: Arc<RuntimeSessionServices>,
}

#[derive(Clone)]
pub(in crate::runtime) struct RuntimeSessionLifecycleService {
    services: Arc<RuntimeSessionServices>,
}

#[derive(Clone)]
pub(in crate::runtime) struct RuntimeSessionGraphService {
    services: Arc<RuntimeSessionServices>,
}

#[derive(Clone)]
pub(in crate::runtime) struct RuntimeSessionProcessService {
    services: Arc<RuntimeSessionServices>,
    visibility: ProcessVisibility,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessVisibility {
    Full,
    ModelTool,
}

#[derive(Clone, Copy, Debug)]
enum ProcessVisibilityOperation {
    ListVisible,
    ListVisibleForAttempt,
    ValidateVisible,
}

impl ProcessVisibility {
    fn consults_filter(self, operation: ProcessVisibilityOperation) -> bool {
        match (self, operation) {
            (_, ProcessVisibilityOperation::ListVisible) => self != Self::Full,
            (_, ProcessVisibilityOperation::ListVisibleForAttempt) => self != Self::Full,
            (_, ProcessVisibilityOperation::ValidateVisible) => self != Self::Full,
        }
    }
}

impl CurrentOwnerCapability {
    fn snapshot_meta_with_frame_root(state: &RuntimeSessionState) -> RuntimeSessionState {
        let frame_root = state
            .current_frame_node_id
            .as_deref()
            .and_then(|node_id| state.session_graph.find_node(node_id))
            .cloned()
            .map(|mut node| {
                node.parent_node_id = None;
                node
            });
        // This is either empty or one detached frame root selected from an already-validated
        // resident graph, so identity, parent, and leaf integrity hold by construction.
        let frame_root_durable = frame_root
            .as_ref()
            .is_some_and(|node| state.is_node_persisted(&node.node_id));
        let mut session_graph = crate::SessionGraph::from_validated_nodes(
            frame_root.iter().cloned().collect(),
            frame_root.map(|node| node.node_id),
        );
        if frame_root_durable {
            session_graph.mark_all_durable();
        }
        RuntimeSessionState {
            session_id: state.session_id.clone(),
            policy: state.effective_policy().clone(),
            agent_frames: state.agent_frames.clone(),
            current_frame_node_id: state.current_frame_node_id.clone(),
            session_graph,
            turn_index: state.turn_index,
            token_usage: state.token_usage.clone(),
            last_prompt_usage: state.last_prompt_usage.clone(),
            authority: state.authority.clone(),
            checkpoint_components: state.checkpoint_components.clone(),
            checkpoint_ref: state.checkpoint_ref.clone(),
            head_revision: state.head_revision,
            config_revision: state.config_revision,
            undelivered_config_change: state.undelivered_config_change.clone(),
        }
    }

    fn new(
        runtime: &LashRuntime,
        plugins: Arc<crate::PluginSession>,
        turn_graph_appends: Option<&TurnGraphAppendDraft>,
    ) -> Self {
        Self {
            owner: CurrentOwner::Session(Box::new(CurrentSession {
                session_id: runtime.state.session_id.clone(),
                snapshot: match turn_graph_appends {
                    None => CurrentSnapshot::Owned(runtime.export_persistence_state()),
                    Some(graph_appends) => {
                        let read_model = runtime.state.read_model();
                        CurrentSnapshot::ReadModel {
                            meta: Self::snapshot_meta_with_frame_root(&runtime.state),
                            messages: read_model.messages,
                            graph_appends: graph_appends.clone(),
                        }
                    }
                },
                store: runtime.services.store.clone(),
            })),
            policy: Some(runtime.state.effective_policy().clone()),
            host: runtime.host.clone(),
            plugins: OwnerPlugins::Session(plugins),
            runtime_lease_owner: runtime.runtime_lease_owner.clone(),
            runtime_lease_executor_id: runtime.runtime_lease_executor_id.clone(),
            turn_phase_probe: runtime.turn_phase_probe.clone(),
        }
    }

    /// This runtime's recorded policy bound to the transport that executes
    /// it. A recorded model this worker cannot bind is the typed, retryable
    /// `LlmProfileUnavailable`.
    fn resolve_policy(
        &self,
    ) -> Result<RuntimeSessionPolicy, lash_execution_contracts::plugin_error::PluginError> {
        self.host
            .resolve_owner_policy(&self.runtime_owner(), self.direct_policy()?.clone())
    }
}

impl ProcessCapability {
    fn new(runtime: &LashRuntime) -> Self {
        Self {
            sync_needed: Arc::clone(&runtime.process_sync_needed),
        }
    }
}

impl RuntimeSessionServices {
    pub(in crate::runtime) fn bindings(self: &Arc<Self>) -> Arc<crate::plugin::ServiceBindings> {
        let mut bindings = crate::plugin::ServiceBindings::default();
        if self.current.session().is_some() {
            bindings = bindings
                .with_session_state(self.state_service())
                .with_session_lifecycle(self.lifecycle_service())
                .with_session_graph(self.graph_service());
        }
        if self.current.host.process_registry().is_some() {
            bindings = bindings.with_processes(self.process_service());
        }
        Arc::new(bindings)
    }
    /// Adopt `admission` as the plugin admission the owner's plugins write
    /// under (FIG-4747).
    pub(in crate::runtime) fn adopt_plugin_admission(
        &self,
        admission: crate::store::plugin_writers::PluginAdmission,
    ) {
        self.current.plugins.adopt_plugin_admission(admission);
    }

    pub(in crate::runtime) fn state_service(
        self: &Arc<Self>,
    ) -> Arc<dyn crate::plugin::SessionStateService> {
        Arc::new(RuntimeSessionStateService {
            services: Arc::clone(self),
        })
    }

    pub(in crate::runtime) fn read_service(
        self: &Arc<Self>,
    ) -> Arc<dyn crate::plugin::SessionReadService> {
        Arc::new(RuntimeSessionStateService {
            services: Arc::clone(self),
        })
    }

    pub(in crate::runtime) fn lifecycle_service(
        self: &Arc<Self>,
    ) -> Arc<dyn crate::plugin::SessionLifecycleService> {
        Arc::new(RuntimeSessionLifecycleService {
            services: Arc::clone(self),
        })
    }

    pub(in crate::runtime) fn graph_service(
        self: &Arc<Self>,
    ) -> Arc<dyn crate::plugin::SessionGraphService> {
        Arc::new(RuntimeSessionGraphService {
            services: Arc::clone(self),
        })
    }

    /// The trace-only emitter context hooks hold in place of a session
    /// service: transforms, compactors and context-pressure hooks write
    /// nothing durable.
    pub(in crate::runtime) fn trace_emitter(self: &Arc<Self>) -> crate::plugin::PluginTraceEmitter {
        let services = Arc::clone(self);
        crate::plugin::PluginTraceEmitter::new(move |context, event| {
            services.current.emit_trace(context, event);
        })
    }

    pub(in crate::runtime) fn process_service(self: &Arc<Self>) -> Arc<dyn crate::ProcessService> {
        Arc::new(RuntimeSessionProcessService {
            services: Arc::clone(self),
            visibility: ProcessVisibility::Full,
        })
    }

    pub fn model_tool_process_service(self: &Arc<Self>) -> Arc<dyn crate::ProcessService> {
        Arc::new(RuntimeSessionProcessService {
            services: Arc::clone(self),
            visibility: ProcessVisibility::ModelTool,
        })
    }

    #[cfg(test)]
    pub(in crate::runtime) fn process_read_service(
        self: &Arc<Self>,
    ) -> Arc<dyn crate::plugin::ProcessReadService> {
        Arc::new(RuntimeSessionProcessService {
            services: Arc::clone(self),
            visibility: ProcessVisibility::Full,
        })
    }

    pub fn direct_completion_client<'run>(
        self: &Arc<Self>,
        effect_controller: crate::ActorContext,
        turn_id: Option<TurnId>,
    ) -> DirectCompletionClient<'run> {
        lash_core_execution::core_internal::runtime_direct_completion_client(
            self.clone(),
            effect_controller,
            turn_id,
        )
    }

    pub(in crate::runtime) fn process_engines(&self) -> &crate::ProcessEngineRegistry {
        &self.current.host.core.process_engines
    }

    /// Host-scoped services: they own a persistence snapshot and commit graph
    /// writes against the store themselves; turn-scoped services come from
    /// [`Self::for_turn`].
    pub(super) fn new(runtime: &LashRuntime) -> Result<Self, PluginOperationInvokeError> {
        Self::with_scope(runtime, None)
    }

    /// The services a process runtime runs its body through, keyed by the
    /// process's minted id: built from the host and, for an engine process,
    /// the composition its environment's components make, with no session
    /// state. Its direct calls account under the process's own owner
    /// (ADR 0127).
    pub(in crate::runtime) fn for_process(ports: ProcessServicesPorts) -> Self {
        let ProcessServicesPorts {
            process_id,
            engine,
            policy,
            host,
            plugin_host,
            runtime_lease_owner,
            turn_phase_probe,
        } = ports;
        Self {
            current: CurrentOwnerCapability {
                policy,
                owner: CurrentOwner::Process { process_id, engine },
                host,
                plugins: OwnerPlugins::Process {
                    host: plugin_host,
                    admission: Arc::default(),
                },
                runtime_lease_owner,
                runtime_lease_executor_id: uuid::Uuid::new_v4().to_string(),
                turn_phase_probe,
            },
            processes: ProcessCapability {
                sync_needed: Arc::new(AtomicBool::new(false)),
            },
            direct: DirectCompletionCapability,
            direct_replay_ordinals: Arc::new(std::sync::Mutex::new(
                std::collections::BTreeMap::new(),
            )),
            direct_unkeyed_in_flight: Arc::new(std::sync::Mutex::new(
                std::collections::BTreeSet::new(),
            )),
        }
    }

    /// Turn-scoped services: graph appends ride `turn_graph_appends`,
    /// committed once by the turn.
    pub(super) fn for_turn(
        runtime: &LashRuntime,
        turn_graph_appends: &TurnGraphAppendDraft,
    ) -> Result<Self, PluginOperationInvokeError> {
        Self::with_scope(runtime, Some(turn_graph_appends))
    }

    fn with_scope(
        runtime: &LashRuntime,
        turn_graph_appends: Option<&TurnGraphAppendDraft>,
    ) -> Result<Self, PluginOperationInvokeError> {
        Ok(Self {
            current: CurrentOwnerCapability::new(
                runtime,
                Arc::clone(&runtime.services.plugins),
                turn_graph_appends,
            ),
            processes: ProcessCapability::new(runtime),
            direct: DirectCompletionCapability,
            direct_replay_ordinals: Arc::new(std::sync::Mutex::new(
                std::collections::BTreeMap::new(),
            )),
            direct_unkeyed_in_flight: Arc::new(std::sync::Mutex::new(
                std::collections::BTreeSet::new(),
            )),
        })
    }
}

pub(super) async fn emit_session_event_to_sink(events: &dyn EventSink, event: SessionStreamEvent) {
    if !events.is_noop() {
        events.emit(event).await;
    }
}

pub(super) fn emit_session_events(event_tx: &TurnObserver, plugin_events: Vec<SessionStreamEvent>) {
    for event in plugin_events {
        event_tx.publish(RuntimeStreamEvent::Session(event));
    }
}

#[cfg(test)]
mod process_visibility_tests {
    use super::{ProcessVisibility, RuntimeSessionProcessService};
    use lash_vocabulary::identity::SessionId;
    use lash_vocabulary::identity::TurnId;

    use crate::runtime::tests::helpers::standard_test_policy;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SESSION_ID: &str = "process-visibility-table-session";

    #[derive(Clone, Copy, Debug)]
    enum Operation {
        ListVisible,
        ListVisibleForAttempt,
        ValidateVisible,
    }

    struct CountingFilter {
        invocations: AtomicUsize,
        hidden: std::sync::OnceLock<lash_vocabulary::identity::ProcessId>,
    }

    impl CountingFilter {
        fn reset(&self) {
            self.invocations.store(0, Ordering::SeqCst);
        }

        fn invocations(&self) -> usize {
            self.invocations.load(Ordering::SeqCst)
        }
    }

    impl crate::ProcessToolVisibilityFilter for CountingFilter {
        fn narrow(
            &self,
            _session: &lash_vocabulary::identity::SessionId,
            candidates: &[lash_vocabulary::identity::ProcessId],
        ) -> Vec<lash_vocabulary::identity::ProcessId> {
            self.invocations.fetch_add(1, Ordering::SeqCst);
            candidates
                .iter()
                .filter(|process_id| Some(*process_id) != self.hidden.get())
                .cloned()
                .collect()
        }
    }

    async fn test_service(
        visibility: ProcessVisibility,
    ) -> (
        RuntimeSessionProcessService,
        Arc<CountingFilter>,
        lash_execution_contracts::backend::Backend,
        lash_vocabulary::identity::ProcessId,
    ) {
        let filter = Arc::new(CountingFilter {
            invocations: AtomicUsize::new(0),
            hidden: std::sync::OnceLock::new(),
        });
        let backend = crate::testing::sqlite_memory_store_backend().await;
        let registry = backend.process_registry();
        let core = crate::RuntimeHostConfig::new(
            backend.clone(),
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
            crate::ExecutionBudgets::recommended(),
            crate::runtime::DeltaCoalescing::recommended(),
            crate::DataRetentionConfig::standard(),
        )
        .with_process_tool_visibility_filter(filter.clone());
        let env = crate::RuntimeEnvironment::builder(core)
            .with_plugin_host(Arc::new(crate::PluginHost::new(
                crate::testing::test_standard_protocol_factories(),
                crate::ExecutionBudgets::recommended(),
                crate::trace::TraceRuntime::new(std::sync::Arc::new(crate::SystemClock)),
            )))
            .with_process_work(crate::testing::process_work_wiring_for_registry(
                registry.clone(),
            ))
            .build();
        let policy = standard_test_policy();
        let runtime = crate::LashRuntime::from_environment(
            &env,
            policy.clone(),
            crate::RuntimeSessionState {
                session_id: SessionId::fixture(SESSION_ID.to_string()),
                policy,
                ..crate::RuntimeSessionState::fixture(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                    crate::NoProgressBudget::bounded(12),
                ))
            },
            None,
            crate::testing::runtime_lease_owner(),
        )
        .await
        .expect("runtime with counting process visibility filter");

        let mut registered = Vec::new();
        for _ in ["visible", "hidden"] {
            let process_id = registry
                .register_process_with_observers(
                    lash_execution_contracts::testing::held_engine_registration(
                        serde_json::Value::Null,
                        lash_execution_contracts::process::ProcessProvenance::host(),
                        lash_execution_contracts::process::Lifetime::Detached,
                    ),
                    &[SessionId::fixture(SESSION_ID.to_string())],
                )
                .await
                .expect("register observed process for visibility table")
                .id;
            registered.push(process_id);
        }
        let hidden_process_id = registered.pop().expect("the hidden process");
        filter
            .hidden
            .set(hidden_process_id.clone())
            .expect("the hidden process is registered once");

        let services = runtime
            .runtime_session_services()
            .expect("runtime session services");
        (
            RuntimeSessionProcessService {
                services,
                visibility,
            },
            filter,
            backend,
            hidden_process_id,
        )
    }

    /// One turn's execution context lends each operation its scope, so the
    /// filter observation is identical across operations.
    fn operation_scope(
        backend: &lash_execution_contracts::backend::Backend,
    ) -> crate::ActorContext {
        crate::ActorContext::detached(backend.clone())
            .scoped(crate::AdmittedScope::turn(
                SessionId::from(SESSION_ID),
                TurnId::fixture(uuid::Uuid::new_v4().to_string()),
            ))
            .expect("a turn's scope")
    }

    fn contains_hidden(
        records: &[lash_execution_contracts::process::ObservedProcess],
        hidden: &lash_vocabulary::identity::ProcessId,
    ) -> bool {
        records.iter().any(|record| record.process_id == *hidden)
    }

    #[tokio::test]
    async fn process_service_filter_policy_is_enforced_by_every_production_operation() {
        let cases = [
            (ProcessVisibility::Full, true),
            (ProcessVisibility::ModelTool, false),
        ];
        let operations = [
            Operation::ListVisible,
            Operation::ListVisibleForAttempt,
            Operation::ValidateVisible,
        ];

        for (visibility, hidden_is_visible) in cases {
            for operation in operations {
                let (service, filter, backend, hidden_process_id) =
                    Box::pin(test_service(visibility)).await;
                filter.reset();
                let expected_invocations = match (visibility, operation) {
                    (ProcessVisibility::ModelTool, Operation::ListVisible)
                    | (ProcessVisibility::ModelTool, Operation::ListVisibleForAttempt) => 2,
                    (ProcessVisibility::ModelTool, Operation::ValidateVisible) => 1,
                    _ => 0,
                };

                match operation {
                    Operation::ListVisible => {
                        let scope = operation_scope(&backend);
                        let records = crate::ProcessService::list_visible(
                            &service,
                            &SessionId::from(SESSION_ID),
                            lash_execution_contracts::process::ProcessListMode::Live,
                            crate::ProcessOpScope::new(scope),
                        )
                        .await
                        .expect("list visible process records");
                        assert_eq!(
                            contains_hidden(&records, &hidden_process_id),
                            hidden_is_visible
                        );
                    }
                    Operation::ListVisibleForAttempt => {
                        let records = crate::ProcessService::list_visible_for_attempt(
                            &service,
                            &lash_vocabulary::identity::RuntimeOwner::Session(SessionId::from(
                                SESSION_ID,
                            )),
                            lash_execution_contracts::process::ProcessListMode::Live,
                        )
                        .await
                        .expect("list visible process records for attempt");
                        assert_eq!(
                            contains_hidden(&records, &hidden_process_id),
                            hidden_is_visible
                        );
                    }
                    Operation::ValidateVisible => {
                        let scope = operation_scope(&backend);
                        let result = crate::ProcessService::validate_visible(
                            &service,
                            &lash_vocabulary::identity::RuntimeOwner::Session(SessionId::from(
                                SESSION_ID,
                            )),
                            std::slice::from_ref(&hidden_process_id),
                            crate::ProcessOpScope::new(scope),
                        )
                        .await;
                        assert_eq!(result.is_ok(), hidden_is_visible);
                    }
                }

                assert_eq!(
                    filter.invocations(),
                    expected_invocations,
                    "unexpected filter calls for {visibility:?} {operation:?}"
                );
            }
        }
    }
}
