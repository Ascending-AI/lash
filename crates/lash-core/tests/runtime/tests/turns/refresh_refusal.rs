//! A refused refresh from the store leaves the resident session whole at
//! its old head (FIG-3684). Ported in FIG-5310 from lash-core's
//! `runtime/tests/persistence.rs` onto a SQLite memory store set.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn rejected_refresh_does_not_retain_stale_checkpoint_components() {
    let backend = sqlite_memory_store_backend().await;
    struct BrokenRead {
        inner: Arc<RecordingStore>,
        read: Mutex<Option<lash_core::store::SessionWindowRead>>,
        loads: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl lash_core::store::RuntimeStoreDecorator for BrokenRead {
        type Inner = dyn lash_core::RuntimeStore;

        fn inner(&self) -> &Self::Inner {
            self.inner.as_ref()
        }
        async fn load_session_window(
            &self,
            session_id: &SessionId,
            selector: lash_core::store::WindowSelector,
        ) -> Result<Option<lash_core::store::SessionWindowRead>, lash_core::StoreError> {
            if let Some(read) = self.read.lock_recover().clone() {
                self.loads.fetch_add(1, Ordering::SeqCst);
                return Ok(Some(read));
            }
            lash_core::store::SessionHistoryStore::load_session_window(
                self.inner.as_ref(),
                session_id,
                selector,
            )
            .await
        }
        async fn load_session_head_meta(
            &self,
            session_id: &SessionId,
        ) -> Result<Option<lash_core::store::SessionHeadMeta>, lash_core::StoreError> {
            if let Some(read) = self.read.lock_recover().as_ref() {
                return Ok(Some(lash_core::store::SessionHeadMeta::assemble(
                    &read.session_id,
                    lash_core::store::SessionHeadPayload {
                        schema_version: lash_core::CURRENT_SESSION_STATE_VERSION,
                        session_id: read.session_id.clone(),
                        config: read.config.clone(),
                    },
                    read.head_revision,
                    read.checkpoint_ref.clone(),
                    read.window.leaf_node_id.clone(),
                    read.current_frame_node_id.clone(),
                )?));
            }
            lash_core::SessionCommitStore::load_session_head_meta(self.inner.as_ref(), session_id)
                .await
        }
    }
    let store = Arc::new(BrokenRead {
        inner: crate::runtime_support::recording_unbound_store_on(&backend).await,
        read: Mutex::new(None),
        loads: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(&backend),
        store.clone(),
    )
    .await;
    // The old head is a recorded one: a writer committed the session's
    // initial frame, and the runtime adopted it.
    lash_core::testing::runtime_helpers::advance_session_head(store.inner.as_ref(), |_| {}).await;
    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("adopt the recorded initial frame");
    runtime.edit_resident_state_for_test(|state| {
        state.set_execution_state_snapshot(Some(b"old-frame-root".to_vec().into()));
    });
    let old_frame = runtime.state().current_frame_node_id.clone();
    assert!(old_frame.is_some(), "the old head has a recorded frame");
    let mut replacement = runtime.state().clone();
    lash_core::runtime::state::open_agent_frame_in_state_with_clock(
        &mut replacement,
        lash_core::testing::runtime_internals::OpenAgentFrameRequest::new(
            lash_core::FrameKey::from_caller_material("review-new-frame").unwrap(),
            lash_core::AgentFrameReason::new("review"),
        ),
        &lash_core::testing::TestClock::new(1000),
    )
    .expect("open a fresh review frame");
    assert_ne!(replacement.current_frame_node_id, old_frame);
    replacement
        .session_graph
        .validate_resident_integrity()
        .unwrap();
    let config = lash_core::RuntimeCommit::persisted_state_for_test(&replacement).config;
    let mut checkpoint = lash_core::HydratedSessionCheckpoint::default();
    checkpoint.turn_state.turn_index = usize::MAX;
    // The durable read a store answers for the switched head: the window
    // of the new frame, anchored above the old one.
    let new_frame = replacement
        .current_frame_node_id
        .clone()
        .expect("the replacement has a current frame");
    let base = replacement
        .session_graph
        .nodes
        .iter()
        .position(|node| node.node_id.as_str() == new_frame.as_str())
        .expect("the new frame's FrameOpen is resident");
    let window_nodes = replacement.session_graph.nodes[base..]
        .iter()
        .map(|node| node.as_ref().clone())
        .collect::<Vec<_>>();
    let window = lash_core::SessionGraph::from_window(
        window_nodes,
        replacement
            .session_graph
            .leaf_node_id
            .clone()
            .expect("the replacement has a leaf"),
        lash_core::session_graph::WindowAnchor {
            frame_node_id: new_frame,
            generation: base as u64,
            external_parent: replacement.session_graph.nodes[base].parent_node_id.clone(),
            previous_frame_node_id: old_frame.clone(),
        },
    )
    .expect("the new frame's window is well anchored");
    *store.read.lock_recover() = Some(
        lash_core::store::SessionWindowRead::new(
            replacement.session_id.clone(),
            runtime.state().head_revision + 1,
            config,
            window,
            Some("new-checkpoint".to_string().into()),
            Some(checkpoint),
        )
        .expect("a well-formed window read"),
    );
    let old_head_revision = runtime.state().head_revision;
    // A refused adoption leaves the resident session whole at its old head,
    // so no new head is ever paired with the previous frame's execution, and
    // a retry reads the durable head again rather than trusting a half-adopted
    // resident one (FIG-3684).
    for attempt in 1..=2 {
        let refused = runtime.refresh_session_graph_from_store().await;
        assert!(
            matches!(
                refused,
                Err(SessionError::Store {
                    source: lash_core::StoreError::CheckpointTurnIndexOutOfRange { .. },
                    ..
                })
            ),
            "attempt {attempt}: {refused:?}"
        );
        assert_eq!(store.loads.load(Ordering::SeqCst), attempt);
        assert_eq!(runtime.state().head_revision, old_head_revision);
        assert_eq!(runtime.state().current_frame_node_id, old_frame);
        assert_eq!(
            runtime
                .state()
                .execution_state_hydration()
                .unwrap()
                .map(|state| state.root.to_vec()),
            Some(b"old-frame-root".to_vec()),
            "the old head keeps its own execution state"
        );
    }
}
