use super::*;
#[cfg(any(test, feature = "testing"))]
use lash_sansio::sync::MutexExt;
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
#[cfg(any(test, feature = "testing"))]
pub(crate) use usage::{
    PendingTokenLedgerEntry, StagedTokenLedger, record_token_usage_shared,
    stage_token_ledger_shared,
};
#[cfg(not(any(test, feature = "testing")))]
pub(in crate::runtime) use usage::{
    PendingTokenLedgerEntry, StagedTokenLedger, record_token_usage_shared,
    stage_token_ledger_shared,
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
    /// Identity of the registration attempt that created this entry. Only the
    /// lease carrying the same nonce may release it.
    pub(super) registration: u64,
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
    runtime_lease_executor_id: String,
    /// Explicit lane context for services scoped to a running parent turn.
    /// `None` identifies a lane-less host/service call and selects the fresh
    /// acquisition path at the persistence call site.
    held_session_execution_lease: Option<BorrowedLaneAuthority>,
    resident_graph_head_stale: Arc<AtomicBool>,
    turn_phase_probe: Option<Arc<dyn RuntimeTurnPhaseProbe>>,
}

#[derive(Clone)]
struct ManagedSessionCapability {
    registry: Arc<Mutex<HashMap<String, RuntimeHandle>>>,
    turns: Arc<StdMutex<HashMap<String, ManagedSessionTurn>>>,
    turn_concurrency_limit: std::num::NonZeroUsize,
}

#[derive(Clone)]
pub(in crate::runtime) struct UsageCapability {
    /// Session-scoped token cost ledger shared with the parent
    /// `LashRuntime`. All managers created from the same runtime
    /// write to the same Arc. Drained at turn-commit time.
    token_ledger: Arc<std::sync::Mutex<Vec<PendingTokenLedgerEntry>>>,
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
        matches!(
            (self, operation),
            (
                Self::ModelTool,
                ProcessVisibilityOperation::ListVisible
                    | ProcessVisibilityOperation::ListVisibleForAttempt
                    | ProcessVisibilityOperation::ValidateVisible
            )
        )
    }
}

impl CurrentSessionCapability {
    fn snapshot_meta_with_frame_root(runtime: &LashRuntime) -> RuntimeSessionState {
        let frame_root = runtime
            .state
            .current_frame_node_id
            .as_deref()
            .and_then(|node_id| runtime.state.session_graph.find_node(node_id))
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
            session_id: runtime.state.session_id.clone(),
            policy: runtime.state.effective_policy().clone(),
            agent_frames: runtime.state.agent_frames.clone(),
            current_frame_node_id: runtime.state.current_frame_node_id.clone(),
            session_graph,
            turn_index: runtime.state.turn_index,
            token_usage: runtime.state.token_usage.clone(),
            last_prompt_usage: runtime.state.last_prompt_usage.clone(),
            protocol_turn_options: runtime.state.effective_protocol_turn_options().clone(),
            checkpoint_components: runtime.state.checkpoint_components.clone(),
            plugin_snapshot_revision: runtime.state.plugin_snapshot_revision,
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
        held_session_execution_lease: Option<&SessionExecutionLeaseGuard>,
    ) -> Self {
        Self {
            session_id: runtime.state.session_id.clone(),
            snapshot: if persist_usage_to_store {
                CurrentSnapshot::Owned(runtime.export_persistence_state())
            } else {
                let read_model = runtime.state.read_model();
                CurrentSnapshot::ReadModel {
                    meta: Self::snapshot_meta_with_frame_root(runtime),
                    messages: read_model.messages,
                }
            },
            policy: runtime.state.effective_policy().clone(),
            host: runtime.host.clone(),
            plugins,
            store: runtime.services.store.clone(),
            runtime_lease_owner: runtime.runtime_lease_owner.clone(),
            runtime_lease_executor_id: runtime.runtime_lease_executor_id.clone(),
            held_session_execution_lease: held_session_execution_lease
                .map(SessionExecutionLeaseGuard::borrowed_authority),
            resident_graph_head_stale: Arc::clone(&runtime.resident_graph_head_stale),
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
            turn_concurrency_limit: runtime.host.core.control.managed_turn_concurrency_limit,
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
            visibility: ProcessVisibility::Full,
        })
    }

    pub(in crate::runtime) fn model_tool_process_service(
        self: &Arc<Self>,
    ) -> Arc<dyn crate::ProcessService> {
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

    pub(super) fn direct_completion_client<'run>(
        self: &Arc<Self>,
        effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
        turn_id: Option<String>,
    ) -> DirectCompletionClient<'run> {
        DirectCompletionClient::runtime(Arc::clone(self), effect_controller, turn_id)
    }

    pub(in crate::runtime) fn trigger_router(self: &Arc<Self>) -> Option<crate::TriggerRouter> {
        self.current.host.trigger_store.as_ref().and_then(|store| {
            self.current
                .host
                .work
                .process_wiring()
                .cloned()
                .map(|wiring| crate::TriggerRouter::new(Arc::clone(store), wiring))
        })
    }

    pub(super) fn new(
        runtime: &LashRuntime,
        persist_usage_to_store: bool,
        child_usage_event_relay: Option<ChildUsageEventRelay>,
        held_session_execution_lease: Option<&SessionExecutionLeaseGuard>,
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
                held_session_execution_lease,
            ),
            managed: ManagedSessionCapability::new(runtime),
            processes: ProcessCapability::new(runtime),
            usage: UsageCapability::new(runtime, persist_usage_to_store, child_usage_event_relay),
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

#[cfg(any(test, feature = "testing"))]
pub(crate) async fn append_receipt_mixed_usage_envelope_conformance(
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let policy = crate::SessionPolicy {
        provider_id: "mixed-envelope-provider".to_string(),
        model: crate::ModelSpec::builder("mixed-envelope-model")
            .context_window_tokens(200_000)
            .build()
            .expect("mixed-envelope model spec"),
        ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded)
    };
    let plugins = crate::PluginHost::new(crate::testing::test_standard_protocol_factories())
        .build_session("root")
        .expect("mixed-envelope plugin session");
    let mut runtime = crate::LashRuntime::from_persistent_embedded_state(
        policy.clone(),
        crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        )),
        crate::PersistentRuntimeServices::new(plugins, Arc::clone(&store)),
        crate::RuntimeSessionState {
            policy,
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        },
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("mixed-envelope runtime");
    super::state::append_session_nodes_to_state_with_clock(
        &mut runtime.state,
        &[crate::SessionAppendNode::plugin(
            "mixed-envelope-preexisting-pending",
            serde_json::json!({"pending": true}),
        )],
        "mixed-envelope-preexisting-pending",
        &crate::SystemClock,
    );
    let services = Arc::new(
        RuntimeSessionServices::new(&runtime, true, None, None)
            .expect("mixed-envelope session services"),
    );
    let graph = services.graph_service();
    let request = crate::AppendSessionNodesRequest {
        operation_id: "mixed-envelope-lost-response".to_string(),
        nodes: vec![crate::SessionAppendNode::plugin(
            "mixed-envelope",
            serde_json::json!({"attempt": 1}),
        )],
        requires_ancestor_node_id: None,
    };
    let first = graph
        .append_session_nodes("root", request.clone())
        .await
        .expect("first mixed-envelope append");

    let interleaved_usage = crate::TokenUsage {
        input_tokens: 17,
        output_tokens: 5,
        cache_read_input_tokens: 3,
        cache_write_input_tokens: 2,
        reasoning_output_tokens: 1,
    };
    services.usage.record_token_usage(
        "mixed-envelope-source",
        "mixed-envelope-model",
        &interleaved_usage,
    );
    runtime
        .await_background_work()
        .await
        .expect("refresh between lost response and retry");
    let retry_services = Arc::new(
        RuntimeSessionServices::new(&runtime, true, None, None)
            .expect("mixed-envelope retry session services"),
    );
    let replay = retry_services
        .graph_service()
        .append_session_nodes("root", request)
        .await
        .expect("lost-response retry replays");
    let (
        crate::AppendSessionNodesOutcome::Appended {
            node_ids: first_node_ids,
            leaf_node_id: first_leaf,
        },
        crate::AppendSessionNodesOutcome::Appended {
            node_ids: replay_node_ids,
            leaf_node_id: replay_leaf,
        },
    ) = (first, replay)
    else {
        panic!("both mixed-envelope attempts must append or replay")
    };
    let operation = super::state::boundary_operation(
        "root",
        "mixed-envelope-lost-response",
        "append-session-nodes",
    );
    let locally_rederived_retry_id =
        crate::store::derive_history_node_id("root", &operation, 0).expect("retry node derivation");
    assert_ne!(
        first_node_ids,
        vec![locally_rederived_retry_id],
        "the scenario must make retry-local node-id derivation differ from the stored result"
    );
    assert_eq!(replay_node_ids, first_node_ids);
    assert_eq!(replay_leaf, first_leaf);
    {
        let ledger = retry_services.usage.token_ledger.lock_recover();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].source, "mixed-envelope-source");
        assert_eq!(ledger[0].model, "mixed-envelope-model");
        assert_eq!(ledger[0].usage, interleaved_usage);
    }

    let changed_content_error = retry_services
        .graph_service()
        .append_session_nodes(
            "root",
            crate::AppendSessionNodesRequest {
                operation_id: "mixed-envelope-lost-response".to_string(),
                nodes: vec![crate::SessionAppendNode::plugin(
                    "mixed-envelope",
                    serde_json::json!({"attempt": "changed"}),
                )],
                requires_ancestor_node_id: None,
            },
        )
        .await
        .expect_err("changed content for an existing operation must be rejected");
    match changed_content_error {
        crate::PluginError::AppendOperationIdentityConflict {
            session_id,
            operation_key,
        } => {
            assert_eq!(session_id, "root");
            assert!(
                operation_key.contains("\"key\":\"append-session-nodes\""),
                "typed conflict must retain the durable append operation key: {operation_key}"
            );
        }
        other => panic!("expected a typed append identity conflict, got {other:?}"),
    }
    {
        let ledger = retry_services.usage.token_ledger.lock_recover();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].usage, interleaved_usage);
    }

    runtime
        .await_background_work()
        .await
        .expect("refresh after receipt replay");
    runtime
        .session_graph_service()
        .expect("fresh graph service")
        .append_session_nodes(
            "root",
            crate::AppendSessionNodesRequest {
                operation_id: "mixed-envelope-natural-commit".to_string(),
                nodes: vec![crate::SessionAppendNode::plugin(
                    "mixed-envelope",
                    serde_json::json!({"attempt": 2}),
                )],
                requires_ancestor_node_id: None,
            },
        )
        .await
        .expect("next natural commit persists restored usage");

    let read = store
        .load_session()
        .await
        .expect("load mixed-envelope session")
        .expect("mixed-envelope session exists");
    assert_eq!(read.token_ledger.len(), 1);
    assert_eq!(read.token_ledger[0].source, "mixed-envelope-source");
    assert_eq!(read.token_ledger[0].model, "mixed-envelope-model");
    assert_eq!(read.token_ledger[0].usage, interleaved_usage);

    // Pin ordinal reuse after successful confirmation. U1 is committed and
    // removed from the pending ledger under operation A at ordinal zero. U2
    // then reuses A/0 with different content; replaying A must confirm only
    // U1's full content-bound identity, leaving U2 staged for natural commit B.
    runtime
        .await_background_work()
        .await
        .expect("refresh before ordinal-reuse sequence");
    let ordinal_services = Arc::new(
        RuntimeSessionServices::new(&runtime, true, None, None)
            .expect("ordinal-reuse session services"),
    );
    let first_usage = crate::TokenUsage {
        input_tokens: 13,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 0,
    };
    ordinal_services.usage.record_token_usage(
        "ordinal-reuse-source",
        "ordinal-reuse-model",
        &first_usage,
    );
    let ordinal_request = crate::AppendSessionNodesRequest {
        operation_id: "mixed-envelope-ordinal-reuse-a".to_string(),
        nodes: vec![crate::SessionAppendNode::plugin(
            "mixed-envelope-ordinal-reuse",
            serde_json::json!({"append": "A"}),
        )],
        requires_ancestor_node_id: None,
    };
    ordinal_services
        .graph_service()
        .append_session_nodes("root", ordinal_request.clone())
        .await
        .expect("operation A commits U1");
    assert!(
        ordinal_services
            .usage
            .token_ledger
            .lock_recover()
            .is_empty(),
        "U1 must be removed after its full identity is confirmed"
    );

    let later_usage = crate::TokenUsage {
        input_tokens: 31,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 0,
    };
    ordinal_services.usage.record_token_usage(
        "ordinal-reuse-source",
        "ordinal-reuse-model",
        &later_usage,
    );
    let replay_error = ordinal_services
        .graph_service()
        .append_session_nodes("root", ordinal_request)
        .await
        .expect_err("operation A replay must refuse U1 confirmation against staged U2");
    assert!(matches!(
        replay_error,
        crate::PluginError::UnstagedUsageConfirmation {
            confirmed_count: 1,
            staged_count: 0,
        }
    ));
    {
        let ledger = ordinal_services.usage.token_ledger.lock_recover();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].usage, later_usage);
        let identity = ledger[0]
            .identity
            .as_ref()
            .expect("U2 remains staged under its full identity");
        assert_eq!(identity.entry_ordinal, 0);
        assert!(
            identity
                .operation_storage_key
                .contains("mixed-envelope-ordinal-reuse-a")
        );
    }

    ordinal_services
        .graph_service()
        .append_session_nodes(
            "root",
            crate::AppendSessionNodesRequest {
                operation_id: "mixed-envelope-ordinal-reuse-b".to_string(),
                nodes: vec![crate::SessionAppendNode::plugin(
                    "mixed-envelope-ordinal-reuse",
                    serde_json::json!({"append": "B"}),
                )],
                requires_ancestor_node_id: None,
            },
        )
        .await
        .expect("natural commit B persists U2");
    assert!(
        ordinal_services
            .usage
            .token_ledger
            .lock_recover()
            .is_empty(),
        "U2 must clear only after natural commit B confirms its full identity"
    );

    let read = store
        .load_session()
        .await
        .expect("load ordinal-reuse session")
        .expect("ordinal-reuse session exists");
    let durable = read
        .token_ledger
        .iter()
        .find(|entry| {
            entry.source == "ordinal-reuse-source" && entry.model == "ordinal-reuse-model"
        })
        .expect("U1 and U2 both remain durable");
    assert_eq!(durable.usage.input_tokens, 44);
}

#[cfg(any(test, feature = "testing"))]
pub(crate) async fn append_usage_cancellation_exactly_once_conformance<A, W, R>(
    store: Arc<dyn crate::RuntimePersistence>,
    arm_and_wait: A,
) where
    A: FnOnce() -> W,
    W: std::future::Future<Output = R>,
    R: FnOnce(),
{
    let policy = crate::SessionPolicy {
        provider_id: "cancelled-usage-provider".to_string(),
        model: crate::ModelSpec::builder("cancelled-usage-model")
            .context_window_tokens(200_000)
            .build()
            .expect("cancelled usage model spec"),
        ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded)
    };
    let plugins = crate::PluginHost::new(crate::testing::test_standard_protocol_factories())
        .build_session("root")
        .expect("cancelled usage plugin session");
    let mut runtime = crate::LashRuntime::from_persistent_embedded_state(
        policy.clone(),
        crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        )),
        crate::PersistentRuntimeServices::new(plugins, Arc::clone(&store)),
        crate::RuntimeSessionState {
            policy,
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        },
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("cancelled usage runtime");
    let services = Arc::new(
        RuntimeSessionServices::new(&runtime, true, None, None)
            .expect("cancelled usage session services"),
    );
    let usage = crate::TokenUsage {
        input_tokens: 19,
        output_tokens: 7,
        cache_read_input_tokens: 3,
        cache_write_input_tokens: 2,
        reasoning_output_tokens: 1,
    };
    services
        .usage
        .record_token_usage("cancelled-usage-source", "cancelled-usage-model", &usage);
    let request = crate::AppendSessionNodesRequest {
        operation_id: "cancelled-usage-append".to_string(),
        nodes: vec![crate::SessionAppendNode::plugin(
            "cancelled-usage",
            serde_json::json!({"attempt": 1}),
        )],
        requires_ancestor_node_id: None,
    };
    let wait_until_worker_queued = arm_and_wait();
    let graph = services.graph_service();
    let cancelled_request = request.clone();
    let append =
        crate::task::spawn(
            async move { graph.append_session_nodes("root", cancelled_request).await },
        );
    let release_worker = wait_until_worker_queued.await;
    append.abort();
    let cancelled = append.await;
    assert!(
        cancelled.is_err(),
        "append task must be cancelled post-send"
    );
    release_worker();

    store
        .load_session()
        .await
        .expect("flush queued SQLite commit")
        .expect("cancelled append committed on worker");
    services
        .graph_service()
        .append_session_nodes("root", request)
        .await
        .expect("cancelled append retry replays");
    runtime
        .await_background_work()
        .await
        .expect("refresh after cancelled append replay");
    runtime
        .session_graph_service()
        .expect("fresh graph service")
        .append_session_nodes(
            "root",
            crate::AppendSessionNodesRequest {
                operation_id: "cancelled-usage-natural-commit".to_string(),
                nodes: vec![crate::SessionAppendNode::plugin(
                    "cancelled-usage",
                    serde_json::json!({"attempt": 2}),
                )],
                requires_ancestor_node_id: None,
            },
        )
        .await
        .expect("next natural commit re-submits cancelled usage identity");

    let read = store
        .load_session()
        .await
        .expect("load cancelled usage session")
        .expect("cancelled usage session exists");
    let matching = read
        .token_ledger
        .iter()
        .filter(|entry| {
            entry.source == "cancelled-usage-source" && entry.model == "cancelled-usage-model"
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0].usage, usage);
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

#[cfg(test)]
mod process_visibility_tests {
    use super::{ProcessVisibility, RuntimeSessionProcessService};
    use crate::ProcessRegistry as _;
    use crate::runtime::tests::helpers::{named_turn_scope, standard_test_policy};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SESSION_ID: &str = "process-visibility-table-session";
    const VISIBLE_PROCESS_ID: &str = "visible-process";
    const HIDDEN_PROCESS_ID: &str = "hidden-process";

    #[derive(Clone, Copy, Debug)]
    enum Operation {
        ListVisible,
        ListVisibleForAttempt,
        ValidateVisible,
        SignalPossessed,
    }

    struct CountingFilter {
        invocations: AtomicUsize,
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
                .filter(|process_id| process_id.as_str() != HIDDEN_PROCESS_ID)
                .cloned()
                .collect()
        }
    }

    async fn test_service(
        visibility: ProcessVisibility,
    ) -> (RuntimeSessionProcessService, Arc<CountingFilter>) {
        let filter = Arc::new(CountingFilter {
            invocations: AtomicUsize::new(0),
        });
        let registry = Arc::new(crate::TestLocalProcessRegistry::default());
        let core = crate::RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        )
        .with_process_tool_visibility_filter(filter.clone());
        let env = crate::RuntimeEnvironment::builder(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        )
        .with_plugin_host(Arc::new(crate::PluginHost::new(
            crate::testing::test_standard_protocol_factories(),
        )))
        .with_runtime_host_config(core)
        .with_process_work(crate::testing::process_work_wiring_for_registry(
            registry.clone(),
        ))
        .with_queued_work(Arc::new(crate::NoQueuedWork::new()))
        .build();
        let policy = standard_test_policy();
        let runtime = crate::LashRuntime::from_environment(
            &env,
            policy.clone(),
            crate::RuntimeSessionState {
                session_id: SESSION_ID.to_string(),
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

        for process_id in [VISIBLE_PROCESS_ID, HIDDEN_PROCESS_ID] {
            registry
                .register_process_with_observers(
                    crate::ProcessRegistration::new(
                        process_id,
                        crate::ProcessInput::External {
                            metadata: serde_json::Value::Null,
                        },
                        crate::RecoveryContract::ExternallyOwned,
                        crate::ProcessProvenance::host(),
                    )
                    .with_extra_event_types([crate::ProcessEventType {
                        name: "signal.ready".to_string(),
                        payload_schema: crate::LashSchema::any(),
                        semantics: crate::ProcessEventSemanticsSpec::default(),
                    }]),
                    &[SESSION_ID.to_string()],
                )
                .await
                .expect("register observed process for visibility table");
        }

        let services = runtime
            .runtime_session_services()
            .expect("runtime session services");
        (
            RuntimeSessionProcessService {
                services,
                visibility,
            },
            filter,
        )
    }

    fn scope() -> crate::ProcessOpScope<'static> {
        crate::ProcessOpScope::new(named_turn_scope(
            SESSION_ID,
            &uuid::Uuid::new_v4().to_string(),
        ))
    }

    fn contains_hidden(records: &[crate::ProcessRecord]) -> bool {
        records.iter().any(|record| record.id == HIDDEN_PROCESS_ID)
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
                let (service, filter) = Box::pin(test_service(visibility)).await;
                filter.reset();
                let expected_invocations = match (visibility, operation) {
                    (ProcessVisibility::ModelTool, Operation::ListVisible)
                    | (ProcessVisibility::ModelTool, Operation::ListVisibleForAttempt) => 2,
                    (ProcessVisibility::ModelTool, Operation::ValidateVisible) => 1,
                    _ => 0,
                };

                match operation {
                    Operation::ListVisible => {
                        let records = crate::ProcessService::list_visible(
                            &service,
                            SESSION_ID,
                            crate::ProcessListMode::Live,
                            scope(),
                        )
                        .await
                        .expect("list visible process records");
                        assert_eq!(contains_hidden(&records), hidden_is_visible);
                    }
                    Operation::ListVisibleForAttempt => {
                        let records = crate::ProcessService::list_visible_for_attempt(
                            &service,
                            SESSION_ID,
                            crate::ProcessListMode::Live,
                        )
                        .await
                        .expect("list visible process records for attempt");
                        assert_eq!(contains_hidden(&records), hidden_is_visible);
                    }
                    Operation::ValidateVisible => {
                        let result = crate::ProcessService::validate_visible(
                            &service,
                            SESSION_ID,
                            &[HIDDEN_PROCESS_ID.to_string()],
                            scope(),
                        )
                        .await;
                        assert_eq!(result.is_ok(), hidden_is_visible);
                    }
                    Operation::SignalPossessed => {
                        // Callers own the visibility boundary through validate_visible;
                        // signal_possessed must not evaluate the filter a second time.
                        crate::ProcessService::signal_possessed(
                            &service,
                            SESSION_ID,
                            HIDDEN_PROCESS_ID,
                            "ready".to_string(),
                            uuid::Uuid::new_v4().to_string(),
                            serde_json::Value::Null,
                            scope(),
                        )
                        .await
                        .expect("signal an already-validated possessed process");
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
        let (service, filter) = Box::pin(test_service(ProcessVisibility::ModelTool)).await;
        filter.reset();

        let records = crate::plugin::ProcessReadService::list_visible(
            &service,
            SESSION_ID,
            crate::ProcessListMode::Live,
            scope(),
        )
        .await
        .expect("list process read records");

        assert!(!contains_hidden(&records));
        assert_eq!(filter.invocations(), 2);
    }
}
