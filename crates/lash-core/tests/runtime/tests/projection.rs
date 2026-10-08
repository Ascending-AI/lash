// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

//! A runtime binds, parks and appends against its session store without a
//! turn, on SQLite memory stores (FIG-5307; the store-only laws FIG-5190
//! deleted with the engine double). The turn laws of this file run through a
//! host's `send()` in `crates/lash/src/tests/projection.rs`.

use super::*;
use lash_core::plugin::PluginSessionRequest;

struct AppendRollbackProtocolFactory {
    store: Arc<RecordingStore>,
    protocol_dirty: Arc<AtomicBool>,
    restore_called: Arc<AtomicBool>,
    fail_restore: Arc<AtomicBool>,
    advance_store_head: bool,
}

impl lash_core::facade_support::PluginFactory for AppendRollbackProtocolFactory {
    fn id(&self) -> &'static str {
        "test_protocol"
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::facade_support::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(AppendRollbackProtocolPlugin {
            store: Arc::clone(&self.store),
            protocol_dirty: Arc::clone(&self.protocol_dirty),
            restore_called: Arc::clone(&self.restore_called),
            fail_restore: Arc::clone(&self.fail_restore),
            advance_store_head: self.advance_store_head,
        }))
    }
}

impl lash_core::plugin::PluginDefinition for AppendRollbackProtocolFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("test_protocol")
    }
}

struct AppendRollbackProtocolPlugin {
    store: Arc<RecordingStore>,
    protocol_dirty: Arc<AtomicBool>,
    restore_called: Arc<AtomicBool>,
    fail_restore: Arc<AtomicBool>,
    advance_store_head: bool,
}

impl lash_core::facade_support::SessionPlugin for AppendRollbackProtocolPlugin {
    fn id(&self) -> &'static str {
        "test_protocol"
    }

    fn register(
        &self,
        reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        reg.protocol()
            .session(Arc::new(AppendRollbackProtocolSession {
                store: Arc::clone(&self.store),
                protocol_dirty: Arc::clone(&self.protocol_dirty),
                restore_called: Arc::clone(&self.restore_called),
                fail_restore: Arc::clone(&self.fail_restore),
                advance_store_head: self.advance_store_head,
            }))?;
        reg.protocol()
            .protocol_driver(Arc::new(UnusedAppendRollbackProtocolDriver))?;
        Ok(())
    }
}

struct AppendRollbackProtocolSession {
    store: Arc<RecordingStore>,
    protocol_dirty: Arc<AtomicBool>,
    restore_called: Arc<AtomicBool>,
    fail_restore: Arc<AtomicBool>,
    advance_store_head: bool,
}

#[async_trait::async_trait]
impl lash_core::plugin::ProtocolSessionPlugin for AppendRollbackProtocolSession {
    async fn append_session_nodes(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
        _nodes: &[lash_core::SessionAppendNode],
    ) -> Result<(), lash_core::SessionError> {
        self.protocol_dirty.store(true, Ordering::SeqCst);
        if self.advance_store_head {
            // Another writer lands a commit while the append is in flight.
            lash_core::testing::runtime_helpers::advance_session_head(self.store.as_ref(), |_| {})
                .await;
        }
        Ok(())
    }

    async fn restore_session(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
        _state: lash_core::plugin::ProtocolSessionRestoreView,
    ) -> Result<(), lash_core::SessionError> {
        self.protocol_dirty.store(false, Ordering::SeqCst);
        self.restore_called.store(true, Ordering::SeqCst);
        if self.fail_restore.load(Ordering::SeqCst) {
            return Err(lash_core::SessionError::Protocol(
                "injected protocol restore failure".to_string(),
            ));
        }
        Ok(())
    }
}

struct UnusedAppendRollbackProtocolDriver;

impl lash_core::plugin::ProtocolDriverPlugin for UnusedAppendRollbackProtocolDriver {
    fn build_preamble(
        &self,
        _input: lash_core::ProtocolBuildInput,
    ) -> lash_core::TurnDriverPreamble {
        panic!("append rollback test never builds a turn")
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn preopened_store_binds_without_remapping_initial_frame() {
    let backend = sqlite_recording_backend().await;
    let store = unbound_recording_store(&backend).await;
    let policy = standard_test_policy();
    lash_core::store::SessionCatalogStore::admit_session(
        store.as_ref(),
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("preopened-session"),
            relation: lash_core::SessionRelation::Root,
            config: lash_core::PersistedSessionConfig::from_policy(
                &policy.clone(),
                lash_core::SessionToolAccess::ambient(),
            ),
            head: lash_core::SessionCreationHead::Config,
        },
    )
    .await
    .expect("preopen store binding");
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("preopened-session"),
        policy: policy.clone(),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ))
    };
    state.ensure_agent_frame_initialized();
    let provisional_frame = state
        .current_frame_node_id
        .clone()
        .expect("provisional initial frame");
    let runtime_host = test_host_config(&backend);
    let runtime_services = lash_core::facade_support::PersistentRuntimeServices::new(
        plugin_session_with_tools(&SessionId::from("preopened-session"), Arc::new(EmptyTools)),
        session_view(store, "preopened-session"),
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let runtime = LashRuntime::from_persistent_embedded_state(
        policy,
        runtime_host,
        runtime_services,
        state,
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("preopened persistent runtime");
    let bound = runtime.export_persistence_state();
    let frame = bound.current_agent_frame().expect("bound initial frame");
    let lash_core::SessionNodePayload::FrameOpen { frame_key, .. } = &bound
        .session_graph
        .find_node(&frame.frame_node_id)
        .expect("bound frame node")
        .payload
    else {
        panic!("current agent frame must resolve to FrameOpen");
    };
    assert_eq!(frame.frame_node_id, provisional_frame);
    assert_eq!(
        frame.frame_node_id,
        lash_core::facade_support::frame_node_id(
            &SessionId::from("preopened-session"),
            frame_key.as_str()
        ),
        "frame identity is stable before and after store binding"
    );
    assert!(matches!(
        bound.turn_scope("first-turn"),
        lash_core::ExecutionScope::Turn {
            ref session_id,
            ref turn_id,
        } if session_id == "preopened-session" && turn_id == "first-turn"
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn park_returns_error_when_final_commit_fails() {
    let backend = sqlite_recording_backend().await;
    let store = unbound_recording_store(&backend).await;
    lash_core::testing::runtime_helpers::create_runtime_fixture_session(
        store.as_ref(),
        &SessionId::from("park-session"),
        &standard_test_policy(),
    )
    .await
    .expect("create the runtime fixture session");
    let plugins = plugin_session_with_tools(&SessionId::from("park-session"), Arc::new(EmptyTools));
    let runtime_host = test_host_config(&backend);
    let runtime_services = lash_core::facade_support::PersistentRuntimeServices::new(
        plugins,
        session_view(store.clone(), "park-session"),
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let runtime = LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        runtime_host,
        runtime_services,
        RuntimeSessionState {
            session_id: SessionId::from("park-session"),
            policy: standard_test_policy(),
            ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ))
        },
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("runtime");

    store.fail_next_runtime_commit(lash_core::StoreError::Backend(
        "park-session final commit refused".to_string(),
    ));

    let err = match Box::pin(runtime.park()).await {
        Ok(_) => panic!("park should fail when final persistence fails"),
        Err(refused) => *refused.error,
    };

    let message = err.to_string();
    assert!(message.contains("failed to persist runtime state"));
    assert!(message.contains("park-session final commit refused"));
}

#[tokio::test(flavor = "multi_thread")]
async fn storeless_append_rejects_inactive_ancestor_before_mutation() {
    let backend = sqlite_recording_backend().await;
    let store = unbound_recording_store(&backend).await;
    let protocol_dirty = Arc::new(AtomicBool::new(false));
    let restore_called = Arc::new(AtomicBool::new(false));
    let plugin_host =
        lash_core::testing::test_plugin_host(vec![Arc::new(AppendRollbackProtocolFactory {
            store,
            protocol_dirty: Arc::clone(&protocol_dirty),
            restore_called: Arc::clone(&restore_called),
            fail_restore: Arc::new(AtomicBool::new(false)),
            advance_store_head: false,
        })]);
    let plugins = plugin_host
        .build_session(PluginSessionRequest::creation("root", Default::default()))
        .expect("plugins");
    let runtime_host = test_host_config(&backend);
    let runtime_services = lash_core::testing::runtime_internals::RuntimeServices::new(
        plugins,
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let mut runtime = LashRuntime::from_embedded_state(
        standard_test_policy(),
        runtime_host,
        runtime_services,
        RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        )),
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("storeless runtime");
    protocol_dirty.store(false, Ordering::SeqCst);
    restore_called.store(false, Ordering::SeqCst);
    let before = runtime.state().session_graph.clone();

    let result = Box::pin(runtime.append_storeless_session_nodes(
        lash_core::AppendSessionNodesRequest {
            operation_id: "storeless-stale-ancestor".to_string(),
            nodes: vec![lash_core::SessionAppendNode::plugin(
                "storeless-stale-ancestor",
                serde_json::json!({"value": 1}),
            )],
            requires_ancestor_node_id: Some("not-on-active-path".into()),
        },
    ))
    .await
    .expect("storeless ancestor fence is a typed result");

    assert!(matches!(
        result,
        lash_core::AppendSessionNodesOutcome::StaleBranch { ref required_node_id }
            if required_node_id == "not-on-active-path"
    ));
    assert!(!protocol_dirty.load(Ordering::SeqCst));
    assert!(!restore_called.load(Ordering::SeqCst));
    assert_eq!(
        serde_json::to_value(&runtime.state().session_graph).expect("encode storeless graph"),
        serde_json::to_value(&before).expect("encode original storeless graph")
    );
}
