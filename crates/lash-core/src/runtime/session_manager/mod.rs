use super::*;
use std::sync::atomic::AtomicBool;

mod api;
mod create_plan;
mod current;
mod direct;
mod graph;
mod managed;
mod materialize;
mod process_runners;
mod turns;
mod usage;

pub use direct::DirectCompletionClient;
pub(in crate::runtime) use usage::ChildUsageEventRelay;
pub(in crate::runtime::session_manager) use usage::{
    ChannelEventSink, LiveChildUsageForwarder, subtract_usage,
};

#[derive(Clone)]
enum CurrentSnapshot {
    Owned(RuntimeSessionState),
    ReadModel {
        meta: RuntimeSessionState,
        messages: Arc<Vec<Message>>,
    },
}

impl CurrentSnapshot {
    fn to_runtime_state(&self) -> RuntimeSessionState {
        match self {
            Self::Owned(snapshot) => snapshot.clone(),
            Self::ReadModel { meta, messages } => {
                let mut snapshot = meta.clone();
                snapshot.replace_active_read_state(messages.as_slice());
                snapshot
            }
        }
    }
}

pub(super) struct ManagedSessionTurn {
    pub(super) session_id: String,
}

#[derive(Clone)]
pub(in crate::runtime) struct CurrentSessionCapability {
    pub(in crate::runtime) session_id: String,
    snapshot: CurrentSnapshot,
    policy: SessionPolicy,
    pub(in crate::runtime) host: RuntimeHost,
    plugins: Arc<crate::PluginSession>,
    store: Option<Arc<dyn crate::store::RuntimePersistence>>,
    runtime_lease_owner: crate::LeaseOwnerIdentity,
    turn_phase_probe: Option<Arc<dyn RuntimeTurnPhaseProbe>>,
}

#[derive(Clone)]
struct ManagedSessionCapability {
    registry: Arc<Mutex<HashMap<String, RuntimeHandle>>>,
    turns: Arc<Mutex<HashMap<String, ManagedSessionTurn>>>,
}

#[derive(Clone)]
pub(in crate::runtime) struct UsageCapability {
    /// Session-scoped token cost ledger shared with the parent
    /// `LashRuntime`. All managers created from the same runtime
    /// write to the same Arc. Drained at turn-commit time.
    token_ledger: Arc<std::sync::Mutex<Vec<TokenLedgerEntry>>>,
    /// Maps child session_id → usage_source label.
    child_sources: Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// Tracks live child-turn usage already bubbled into the shared
    /// token ledger so child turn completion can reconcile final usage
    /// without double counting.
    child_turn_live_usage: Arc<std::sync::Mutex<HashMap<String, TokenUsage>>>,
    /// Optional relay for bubbling child-session token usage into the
    /// parent turn's live event stream.
    child_usage_event_relay: Option<ChildUsageEventRelay>,
    /// Out-of-turn managers persist drained usage back into the
    /// current session graph. Turn-time managers leave the shared
    /// ledger alone so the parent turn can commit it once.
    persist_to_store: bool,
}

#[derive(Clone)]
struct ProcessCapability {
    sync_needed: Arc<AtomicBool>,
}

#[derive(Clone, Default)]
struct DirectCompletionCapability;

#[derive(Clone)]
pub(super) struct RuntimeSessionServices {
    current: CurrentSessionCapability,
    managed: ManagedSessionCapability,
    processes: ProcessCapability,
    usage: UsageCapability,
    direct: DirectCompletionCapability,
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
}

impl CurrentSessionCapability {
    fn snapshot_meta_with_frame_root(runtime: &LashRuntime) -> RuntimeSessionState {
        let frame_root =
            runtime
                .state
                .current_agent_frame()
                .map(|frame| crate::SessionNodeRecord {
                    node_id: frame.frame_node_id.clone(),
                    parent_node_id: None,
                    timestamp: frame.created_at.clone(),
                    payload: crate::SessionNodePayload::FrameOpen {
                        reason: frame.reason.clone(),
                        assignment: frame.assignment.clone(),
                        protocol_turn_options: frame.protocol_turn_options.clone(),
                    },
                });
        let session_graph = crate::SessionGraph::from_nodes(
            frame_root.iter().cloned().collect(),
            frame_root.map(|node| node.node_id),
        );
        RuntimeSessionState {
            session_id: runtime.state.session_id.clone(),
            policy: runtime.state.policy.clone(),
            agent_frames: runtime.state.agent_frames.clone(),
            current_frame_node_id: runtime.state.current_frame_node_id.clone(),
            session_graph,
            turn_index: runtime.state.turn_index,
            token_usage: runtime.state.token_usage.clone(),
            last_prompt_usage: runtime.state.last_prompt_usage.clone(),
            protocol_turn_options: runtime.protocol_turn_options.clone(),
            tool_state_ref: runtime.state.tool_state_ref.clone(),
            tool_state_generation: runtime.state.tool_state_generation,
            tool_state_snapshot: None,
            plugin_snapshot_ref: runtime.state.plugin_snapshot_ref.clone(),
            plugin_snapshot_revision: runtime.state.plugin_snapshot_revision,
            plugin_snapshot: None,
            execution_state_ref: runtime.state.execution_state_ref.clone(),
            execution_state_snapshot: None,
            token_ledger: runtime.state.token_ledger.clone(),
            checkpoint_ref: runtime.state.checkpoint_ref.clone(),
            head_revision: runtime.state.head_revision,
            persisted_node_ids: runtime.state.persisted_node_ids.clone(),
        }
    }

    fn new(
        runtime: &LashRuntime,
        plugins: Arc<crate::PluginSession>,
        persist_usage_to_store: bool,
    ) -> Self {
        Self {
            session_id: runtime.state.session_id.clone(),
            snapshot: if persist_usage_to_store {
                CurrentSnapshot::Owned(runtime.export_graph_first_state())
            } else {
                let read_model = runtime.state.read_model();
                CurrentSnapshot::ReadModel {
                    meta: Self::snapshot_meta_with_frame_root(runtime),
                    messages: read_model.messages,
                }
            },
            policy: runtime.policy.clone(),
            host: runtime.host.clone(),
            plugins,
            store: runtime.services.store.clone(),
            runtime_lease_owner: runtime.runtime_lease_owner.clone(),
            turn_phase_probe: runtime.turn_phase_probe.clone(),
        }
    }

    fn resolve_policy(&self) -> Result<RuntimeSessionPolicy, crate::PluginError> {
        self.host
            .resolve_session_policy(&self.session_id, self.policy.clone())
            .map_err(|err| crate::PluginError::Session(err.to_string()))
    }
}

impl ManagedSessionCapability {
    fn new(runtime: &LashRuntime) -> Self {
        Self {
            registry: Arc::clone(&runtime.managed_sessions),
            turns: Arc::clone(&runtime.managed_turns),
        }
    }
}

impl ProcessCapability {
    fn new(runtime: &LashRuntime) -> Self {
        Self {
            sync_needed: Arc::clone(&runtime.process_sync_needed),
        }
    }
}

impl UsageCapability {
    fn new(
        runtime: &LashRuntime,
        persist_to_store: bool,
        child_usage_event_relay: Option<ChildUsageEventRelay>,
    ) -> Self {
        Self {
            token_ledger: Arc::clone(&runtime.shared_token_ledger),
            child_sources: Arc::new(std::sync::Mutex::new(HashMap::new())),
            child_turn_live_usage: Arc::new(std::sync::Mutex::new(HashMap::new())),
            child_usage_event_relay,
            persist_to_store,
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

    pub(in crate::runtime) fn process_service(self: &Arc<Self>) -> Arc<dyn crate::ProcessService> {
        Arc::new(RuntimeSessionProcessService {
            services: Arc::clone(self),
        })
    }

    pub(in crate::runtime) fn process_read_service(
        self: &Arc<Self>,
    ) -> Arc<dyn crate::plugin::ProcessReadService> {
        Arc::new(RuntimeSessionProcessService {
            services: Arc::clone(self),
        })
    }

    pub(in crate::runtime) fn process_cancel_ability(
        self: &Arc<Self>,
    ) -> Arc<dyn crate::ProcessCancelAbility> {
        Arc::clone(&self.current.host.core.control.process_cancel_ability)
    }

    pub(super) fn direct_completion_client<'run>(
        self: &Arc<Self>,
        effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
        turn_id: Option<String>,
    ) -> DirectCompletionClient<'run> {
        DirectCompletionClient::runtime(Arc::clone(self), effect_controller, turn_id)
    }

    pub(in crate::runtime) fn trigger_router(self: &Arc<Self>) -> Option<crate::TriggerRouter> {
        self.current.host.trigger_store.as_ref().map(|store| {
            crate::TriggerRouter::new(
                Arc::clone(store),
                self.current.host.process_registry.clone(),
                self.current.host.process_work_driver.clone(),
            )
        })
    }

    pub(super) fn new(
        runtime: &LashRuntime,
        persist_usage_to_store: bool,
        child_usage_event_relay: Option<ChildUsageEventRelay>,
    ) -> Result<Self, PluginOperationInvokeError> {
        let Some(session) = runtime.session.as_ref() else {
            return Err(PluginOperationInvokeError::Unknown(
                "session_manager".to_string(),
            ));
        };
        Ok(Self {
            current: CurrentSessionCapability::new(
                runtime,
                Arc::clone(session.plugins()),
                persist_usage_to_store,
            ),
            managed: ManagedSessionCapability::new(runtime),
            processes: ProcessCapability::new(runtime),
            usage: UsageCapability::new(runtime, persist_usage_to_store, child_usage_event_relay),
            direct: DirectCompletionCapability,
        })
    }
}

pub(super) async fn emit_session_events_to_sink(
    events: &dyn EventSink,
    plugin_events: Vec<SessionStreamEvent>,
) {
    if events.is_noop() {
        return;
    }
    for event in plugin_events {
        events.emit(event).await;
    }
}

pub(super) async fn emit_session_event_to_sink(events: &dyn EventSink, event: SessionStreamEvent) {
    if !events.is_noop() {
        events.emit(event).await;
    }
}

pub(super) async fn emit_session_events(
    event_tx: &mpsc::Sender<RuntimeStreamEvent>,
    plugin_events: Vec<SessionStreamEvent>,
) {
    for event in plugin_events {
        if !event_tx.is_closed() {
            let _ = event_tx.send(RuntimeStreamEvent::Session(event)).await;
        }
    }
}
