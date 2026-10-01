//! Session services behind the runtime's session-facing plugin surface.
//!
//! What lives here (ADR 0089): session initialisation (`session_init` — the
//! `SessionCreateRequest` pipeline and the process-origin port that drives a
//! recorded child's first turn), the process runners, direct completions, and
//! current-session services (`current`, `graph`, `api`) that resolve only the
//! runtime's own session id — a foreign id is an unknown-session error, never
//! a registry lookup. There is no second session model: every executing
//! session is an ordinary session, and a process-spawned child runtime is
//! owned by its process run, not held here.

use super::*;
use crate::ProcessId;
use crate::SessionId;
use crate::TurnId;
use std::sync::atomic::AtomicBool;

mod api;
mod current;
mod direct;
mod direct_outcome;
mod graph;
mod process_runners;
mod session_init;
mod tool_child_context;
#[cfg(any(test, feature = "testing"))]
pub use session_init::take_spawned_child_runtimes;
mod event_sink;

pub use crate::direct_completion_client::DirectCompletionClient;
pub(in crate::runtime::session_manager) use event_sink::ChannelEventSink;

#[derive(Clone)]
enum CurrentSnapshot {
    /// Host-scoped services own a full persistence snapshot and commit
    /// against the store themselves.
    Owned(RuntimeSessionState),
    /// Turn-scoped services see a read projection of the running turn's base
    /// state. It is never a durable base: graph appends made through these
    /// services ride the turn's commit draft and are overlaid on this
    /// projection for in-turn readers.
    ReadModel {
        meta: RuntimeSessionState,
        messages: lash_sansio::AppendVec<Message>,
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
    /// Explicit lane context for services scoped to a running parent turn.
    /// `None` identifies a lane-less host/service call and selects the fresh
    /// acquisition path at the persistence call site.
    held_drive_fence: Option<DriveFence>,
    resident_graph_head_stale: Arc<AtomicBool>,
}

/// Who the services run for. A process runtime is keyed by its minted id and
/// carries its captured environment: it has no session state, no session
/// store and no frame, and every session-only service refuses it with
/// `PluginError::NotASessionRuntime`.
#[derive(Clone)]
pub(in crate::runtime) enum CurrentOwner {
    Session(Box<CurrentSession>),
    Process {
        process_id: crate::ProcessId,
        /// The environment the process captured at its start: its starter's
        /// recorded policy and plugin config (FIG-4396).
        environment: Box<crate::ProcessExecutionEnvSpec>,
    },
}

#[derive(Clone)]
pub(in crate::runtime) struct CurrentOwnerCapability {
    pub(in crate::runtime) owner: CurrentOwner,
    policy: SessionPolicy,
    pub(in crate::runtime) host: RuntimeHost,
    plugins: Arc<crate::PluginSession>,
    runtime_lease_owner: crate::LeaseOwnerIdentity,
    runtime_lease_executor_id: String,
    turn_phase_probe: Option<Arc<dyn RuntimeTurnPhaseProbe>>,
}

impl CurrentOwnerCapability {
    /// The fleet-format generation this capability's durable writers emit:
    /// the `F` the bound session's store recorded (FIG-3796), or, for a
    /// process owner, the `F` its process registry recorded, since a process
    /// writes its effect occurrences and terminal through that registry
    /// (FIG-3805). A capability holding neither writes nothing durable, so the
    /// build's own generation is the only honest answer it can give.
    pub(in crate::runtime) fn fleet_format(&self) -> crate::FleetFormat {
        match &self.owner {
            CurrentOwner::Session(session) => session
                .store
                .as_ref()
                .map(|store| store.fleet_format())
                .unwrap_or_else(crate::FleetFormat::current),
            CurrentOwner::Process { .. } => self
                .host
                .process_registry()
                .map(|registry| registry.fleet_format())
                .unwrap_or_else(crate::FleetFormat::current),
        }
    }

    pub(in crate::runtime) fn runtime_owner(&self) -> crate::RuntimeOwner {
        match &self.owner {
            CurrentOwner::Session(session) => {
                crate::RuntimeOwner::Session(session.session_id.clone())
            }
            CurrentOwner::Process { process_id, .. } => {
                crate::RuntimeOwner::Process(process_id.clone())
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
    ) -> Result<&CurrentSession, crate::PluginError> {
        match &self.owner {
            CurrentOwner::Session(session) => Ok(session.as_ref()),
            CurrentOwner::Process { process_id, .. } => {
                Err(crate::PluginError::NotASessionRuntime {
                    operation: operation.to_string(),
                    process_id: process_id.clone(),
                })
            }
        }
    }

    /// Whether `session_id` is the session these services resolve.
    pub(in crate::runtime) fn is_current_session(&self, session_id: &SessionId) -> bool {
        self.session()
            .is_some_and(|session| session.session_id == *session_id)
    }

    /// The runtime store of the session these services resolve; a process
    /// runtime has none.
    pub(in crate::runtime) fn session_runtime_store(&self) -> Option<Arc<dyn crate::RuntimeStore>> {
        self.session()
            .and_then(|session| session.store.as_ref())
            .map(|store| Arc::clone(store.store()))
    }

    /// Who a dispatch built from these services runs for: the session on its
    /// current agent frame, or the process.
    pub(in crate::runtime) fn execution_owner(
        &self,
    ) -> Result<crate::ExecutionOwner, crate::PluginError> {
        match &self.owner {
            CurrentOwner::Session(session) => {
                let agent_frame_id = session
                    .snapshot
                    .to_runtime_state()
                    .current_frame_node_id
                    .ok_or_else(|| {
                        crate::PluginError::Session(format!(
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

    /// The execution environment a start captures from these services: the
    /// session's current one, or the process's captured one.
    pub(in crate::runtime) fn execution_env_spec(
        &self,
    ) -> Result<crate::ProcessExecutionEnvSpec, crate::PluginError> {
        match &self.owner {
            CurrentOwner::Session(session) => Ok(
                crate::facade_support::RuntimeSessionStateFacadeOps::process_execution_env_spec(
                    &session.snapshot.to_runtime_state(),
                    &self.policy,
                ),
            ),
            CurrentOwner::Process { environment, .. } => Ok(environment.as_ref().clone()),
        }
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
    pub(in crate::runtime) process_id: crate::ProcessId,
    /// The environment the process captured at its start; its policy is
    /// the process runtime's policy.
    pub(in crate::runtime) environment: crate::ProcessExecutionEnvSpec,
    pub(in crate::runtime) host: RuntimeHost,
    pub(in crate::runtime) plugins: Arc<crate::PluginSession>,
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
        let session_graph = crate::SessionGraph::from_validated_nodes(
            frame_root.iter().cloned().collect(),
            frame_root.map(|node| node.node_id),
        );
        RuntimeSessionState {
            session_id: state.session_id.clone(),
            policy: state.effective_policy().clone(),
            agent_frames: state.agent_frames.clone(),
            current_frame_node_id: state.current_frame_node_id.clone(),
            pending_follow_on: state.pending_follow_on.clone(),
            session_graph,
            turn_index: state.turn_index,
            token_usage: state.token_usage.clone(),
            last_prompt_usage: state.last_prompt_usage.clone(),
            authority: state.authority.clone(),
            checkpoint_components: state.checkpoint_components.clone(),
            checkpoint_ref: state.checkpoint_ref.clone(),
            head_revision: state.head_revision,
            config_revision: state.config_revision,
            persisted_node_ids: state.persisted_node_ids.clone(),
            preserve_tool_state_snapshot: state.preserve_tool_state_snapshot,
        }
    }

    fn new(
        runtime: &LashRuntime,
        plugins: Arc<crate::PluginSession>,
        turn_graph_appends: Option<&TurnGraphAppendDraft>,
        held_drive_fence: Option<&DriveFence>,
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
                held_drive_fence: held_drive_fence.cloned(),
                resident_graph_head_stale: Arc::clone(
                    runtime.resident_session.graph_head_stale_flag(),
                ),
            })),
            policy: runtime.state.effective_policy().clone(),
            host: runtime.host.clone(),
            plugins,
            runtime_lease_owner: runtime.runtime_lease_owner.clone(),
            runtime_lease_executor_id: runtime.runtime_lease_executor_id.clone(),
            turn_phase_probe: runtime.turn_phase_probe.clone(),
        }
    }

    // The provider pin is settled typed at open and at child-create
    // (ADR 0066, `SessionPolicy::settle_provider_pin`), so a recorded/requested
    // mismatch can no longer arrive here; the remaining resolution failures are
    // host wiring faults and stay as plugin errors.
    fn resolve_policy(&self) -> Result<RuntimeSessionPolicy, crate::PluginError> {
        self.host
            .resolve_owner_policy(&self.runtime_owner(), self.policy.clone())
            .map_err(|err| crate::PluginError::Session(err.to_string()))
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
        effect_controller: crate::runtime::ScopedEffectController<'run>,
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

    pub(in crate::runtime) fn trigger_router(self: &Arc<Self>) -> Option<crate::TriggerRouter> {
        self.current
            .host
            .work
            .process_wiring()
            .cloned()
            .map(|wiring| {
                crate::TriggerRouter::new(self.current.host.core.trigger_store(), wiring)
                    .with_process_artifacts(
                        Arc::clone(&self.current.host.core.durability.process_env_store),
                        self.current.host.core.process_engines.clone(),
                    )
                    .with_process_starts(
                        self.current
                            .host
                            .core
                            .backend()
                            .obligation_ledger(crate::store::ObligationKind::ProcessStart),
                        Arc::clone(&self.current.host.core.clock),
                        self.current.host.core.control.relay_policy(),
                    )
            })
    }

    /// Host-scoped services: they own a persistence snapshot and commit graph
    /// writes against the store themselves; turn-scoped services come from
    /// [`Self::for_turn`].
    pub(super) fn new(
        runtime: &LashRuntime,
        held_drive_fence: Option<&DriveFence>,
    ) -> Result<Self, PluginOperationInvokeError> {
        Self::with_scope(runtime, None, held_drive_fence)
    }

    /// The services a process runtime runs its body through, keyed by the
    /// process's minted id: built from the host, the process's own plugin
    /// session and its captured environment, with no session state. Its
    /// direct calls account under the process's own owner (ADR 0125).
    pub(in crate::runtime) fn for_process(ports: ProcessServicesPorts) -> Self {
        let ProcessServicesPorts {
            process_id,
            environment,
            host,
            plugins,
            runtime_lease_owner,
            turn_phase_probe,
        } = ports;
        Self {
            current: CurrentOwnerCapability {
                policy: environment.policy.clone(),
                owner: CurrentOwner::Process {
                    process_id,
                    environment: Box::new(environment),
                },
                host,
                plugins,
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
        held_drive_fence: Option<&DriveFence>,
        turn_graph_appends: &TurnGraphAppendDraft,
    ) -> Result<Self, PluginOperationInvokeError> {
        Self::with_scope(runtime, Some(turn_graph_appends), held_drive_fence)
    }

    fn with_scope(
        runtime: &LashRuntime,
        turn_graph_appends: Option<&TurnGraphAppendDraft>,
        held_drive_fence: Option<&DriveFence>,
    ) -> Result<Self, PluginOperationInvokeError> {
        let Some(session) = runtime.session.as_ref() else {
            return Err(PluginOperationInvokeError::Unknown(
                "session_manager".to_string(),
            ));
        };
        Ok(Self {
            current: CurrentOwnerCapability::new(
                runtime,
                Arc::clone(session.plugins()),
                turn_graph_appends,
                held_drive_fence,
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
    use crate::SessionId;
    use crate::TurnId;

    use crate::runtime::tests::helpers::standard_test_policy;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SEED: u64 = 0xf6_0005;
    const SESSION_ID: &str = "process-visibility-table-session";

    #[derive(Clone, Copy, Debug)]
    enum Operation {
        ListVisible,
        ListVisibleForAttempt,
        ValidateVisible,
        SignalPossessed,
    }

    struct CountingFilter {
        invocations: AtomicUsize,
        hidden: std::sync::OnceLock<crate::ProcessId>,
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
            _session: &crate::SessionId,
            candidates: &[crate::ProcessId],
        ) -> Vec<crate::ProcessId> {
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
        lash_restate_test::RestateTestBackend,
        crate::ProcessId,
    ) {
        let filter = Arc::new(CountingFilter {
            invocations: AtomicUsize::new(0),
            hidden: std::sync::OnceLock::new(),
        });
        let double =
            crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let backend = double.lash_backend();
        let registry = backend.process_registry();
        let core = crate::RuntimeHostConfig::new(
            backend,
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        )
        .with_process_tool_visibility_filter(filter.clone());
        let env = crate::RuntimeEnvironment::builder(core)
            .with_plugin_host(Arc::new(crate::PluginHost::new(
                crate::testing::test_standard_protocol_factories(),
            )))
            .with_process_work(crate::testing::process_work_wiring_for_registry(
                registry.clone(),
            ))
            .with_queued_work(Arc::new(crate::NoSessionWork::new()))
            .build();
        let policy = standard_test_policy();
        let runtime = crate::LashRuntime::from_environment(
            &env,
            policy.clone(),
            crate::RuntimeSessionState {
                session_id: SessionId::from(SESSION_ID.to_string()),
                policy,
                ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
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
                    crate::ProcessRegistration::new(
                        crate::ProcessInput::External {
                            metadata: serde_json::Value::Null,
                        },
                        crate::ProcessProvenance::host(),
                        crate::Lifetime::Detached,
                    )
                    .with_extra_event_types([crate::ProcessEventType {
                        name: "signal.ready".to_string(),
                        payload_schema: crate::LashSchema::any(),
                        semantics: crate::ProcessEventSemanticsSpec::default(),
                    }]),
                    &[SessionId::from(SESSION_ID.to_string())],
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
            double,
            hidden_process_id,
        )
    }

    /// One opened turn handler lends each operation its scope: `signal`
    /// executes its command in-handler, and the read operations share the
    /// same shape so the filter observation is identical.
    async fn operation_scope(
        double: &lash_restate_test::RestateTestBackend,
    ) -> lash_restate_test::OpenHandler {
        double
            .open_handler(crate::AdmittedScope::turn(
                SessionId::from(SESSION_ID),
                TurnId::from(uuid::Uuid::new_v4().to_string()),
            ))
            .await
            .expect("open the turn's handler")
    }

    fn contains_hidden(records: &[crate::ProcessRecord], hidden: &crate::ProcessId) -> bool {
        records.iter().any(|record| record.id == *hidden)
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
            Operation::SignalPossessed,
        ];

        for (visibility, hidden_is_visible) in cases {
            for operation in operations {
                let (service, filter, double, hidden_process_id) =
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
                        let handler = operation_scope(&double).await;
                        let records = crate::ProcessService::list_visible(
                            &service,
                            &SessionId::from(SESSION_ID),
                            crate::ProcessListMode::Live,
                            crate::ProcessOpScope::new(handler.scoped()),
                        )
                        .await
                        .expect("list visible process records");
                        handler.close().await.expect("close the turn's handler");
                        assert_eq!(
                            contains_hidden(&records, &hidden_process_id),
                            hidden_is_visible
                        );
                    }
                    Operation::ListVisibleForAttempt => {
                        let records = crate::ProcessService::list_visible_for_attempt(
                            &service,
                            &crate::RuntimeOwner::Session(SessionId::from(SESSION_ID)),
                            crate::ProcessListMode::Live,
                        )
                        .await
                        .expect("list visible process records for attempt");
                        assert_eq!(
                            contains_hidden(&records, &hidden_process_id),
                            hidden_is_visible
                        );
                    }
                    Operation::ValidateVisible => {
                        let handler = operation_scope(&double).await;
                        let result = crate::ProcessService::validate_visible(
                            &service,
                            &crate::RuntimeOwner::Session(SessionId::from(SESSION_ID)),
                            std::slice::from_ref(&hidden_process_id),
                            crate::ProcessOpScope::new(handler.scoped()),
                        )
                        .await;
                        handler.close().await.expect("close the turn's handler");
                        assert_eq!(result.is_ok(), hidden_is_visible);
                    }
                    Operation::SignalPossessed => {
                        // Callers own the visibility boundary through validate_visible;
                        // signal_possessed must not evaluate the filter a second time.
                        let handler = operation_scope(&double).await;
                        crate::ProcessService::signal_possessed(
                            &service,
                            &crate::RuntimeOwner::Session(SessionId::from(SESSION_ID)),
                            &hidden_process_id,
                            "ready".to_string(),
                            uuid::Uuid::new_v4().to_string(),
                            serde_json::Value::Null,
                            crate::ProcessOpScope::new(handler.scoped()),
                        )
                        .await
                        .expect("signal an already-validated possessed process");
                        handler.close().await.expect("close the turn's handler");
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

    #[tokio::test]
    async fn process_read_service_honors_model_tool_visibility_if_wired_that_way() {
        let (service, filter, double, hidden) =
            Box::pin(test_service(ProcessVisibility::ModelTool)).await;
        filter.reset();

        let handler = operation_scope(&double).await;
        let records = crate::plugin::ProcessReadService::list_visible(
            &service,
            &SessionId::from(SESSION_ID),
            crate::ProcessListMode::Live,
            crate::ProcessOpScope::new(handler.scoped()),
        )
        .await
        .expect("list process read records");
        handler.close().await.expect("close the turn's handler");

        assert!(!contains_hidden(&records, &hidden));
        assert_eq!(filter.invocations(), 2);
    }
}
