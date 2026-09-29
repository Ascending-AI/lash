use super::*;
use pretty_assertions::assert_eq;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn commit_increments_head_and_round_trips_agent_frames(store: Arc<dyn RuntimeStore>) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        policy: SessionPolicy {
            model: ModelSpec::builder("gpt-5.4-mini")
                .context_window_tokens(200_000)
                .build()
                .expect("valid model spec"),
            ..SessionPolicy::new(crate::TurnBudget::Unbounded)
        },
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    let initial_frame = state.current_agent_frame().expect("initial frame");
    let assignment = initial_frame.assignment.clone();
    let initial_frame_node_id = initial_frame.frame_node_id.clone();
    let custom_reason = AgentFrameReason::new("plan_mode");
    let second_frame_key =
        crate::FrameKey::from_caller_material("frame-2").expect("non-empty frame material");
    let second_frame_node_id =
        crate::session_graph::frame_node_id(&state.session_id, second_frame_key.as_str());
    assert!(state.session_graph.append_frame_open_with_id_at(
        second_frame_node_id.clone(),
        second_frame_key,
        custom_reason.clone(),
        assignment,
        ProtocolTurnOptions::default(),
        "2026-07-27T00:00:00Z".to_string(),
    ));
    state.current_frame_node_id = Some(second_frame_node_id.clone());
    state.agent_frames = state
        .session_graph
        .agent_frame_records(&SessionId::from("root"));
    state.set_execution_state_snapshot(Some(b"frame-vm".to_vec().into()));

    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, &[]),
        "commit-round-trip",
    )
    .await
    .expect("commit runtime state");
    let read = store
        .load_session_window(
            &SessionId::from("root"),
            crate::store::WindowSelector::Current,
        )
        .await
        .expect("load session")
        .expect("session read");

    assert_eq!(
        read.current_frame_node_id.as_deref(),
        Some(second_frame_node_id.as_str())
    );
    // The window is the current frame (ADR 0112 §5): it holds the second
    // frame's record alone, and its anchor names the first frame below it.
    let frames = read.window.agent_frame_records(&SessionId::from("root"));
    assert_eq!(frames.len(), 1);
    let current = &frames[0];
    assert_eq!(current.frame_node_id, second_frame_node_id);
    assert_eq!(current.reason, custom_reason);
    let anchor = read.window.anchor().expect("a stored window is anchored");
    assert_eq!(anchor.frame_node_id, second_frame_node_id);
    assert_eq!(
        anchor.previous_frame_node_id.as_ref(),
        Some(&initial_frame_node_id),
        "the anchor names the frame the window's base continues"
    );
    assert_eq!(
        read.checkpoint.as_ref().and_then(|checkpoint| {
            checkpoint.component_body(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
        }),
        Some(&b"frame-vm"[..])
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn concurrent_head_revision_cas_applies_exactly_once(store: Arc<dyn RuntimeStore>) {
    let session_id = "concurrent-head-cas";
    let _lease = seal_drive_fence_for_test(&store, &SessionId::from(session_id), "cas-owner").await;
    let make_commit = |node_id: &str| {
        let state = RuntimeSessionState {
            session_id: SessionId::from(session_id.to_string()),
            ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
        };
        let node = sample_session_node(&SessionId::from(session_id), node_id, None);
        let derived_node_id = node.node_id.clone();
        let commit = RuntimeCommit {
            expected_head_revision: 0,
            current_frame_node_id: Some(
                crate::FrameNodeId::new(derived_node_id.clone())
                    .expect("derived test frame identity is non-empty"),
            ),
            graph: crate::GraphAppend::Extend { nodes: vec![node] },
            ..RuntimeCommit::persisted_state_for_test(&state, &[])
        };
        commit
            .with_operation(crate::OperationId::new(
                crate::ExecutionScope::runtime_operation(format!("head-cas:{node_id}")),
                "commit",
            ))
            .expect("build distinct head-CAS operation")
            .0
    };

    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let left_store = Arc::clone(&store);
    let right_store = Arc::clone(&store);
    let left_barrier = Arc::clone(&barrier);
    let right_barrier = Arc::clone(&barrier);
    let left_commit = make_commit("cas-left");
    let right_commit = make_commit("cas-right");
    let left = crate::task::spawn(async move {
        left_barrier.wait().await;
        left_store.commit_runtime_state(left_commit).await
    });
    let right = crate::task::spawn(async move {
        right_barrier.wait().await;
        right_store.commit_runtime_state(right_commit).await
    });

    barrier.wait().await;
    let left = left.await.expect("join left head-CAS writer");
    let right = right.await.expect("join right head-CAS writer");
    let winners = [&left, &right]
        .into_iter()
        .filter(|result| result.is_ok())
        .count();
    let conflicts = [&left, &right]
        .into_iter()
        .filter(|result| matches!(result, Err(StoreError::HeadRevisionConflict { .. })))
        .count();
    assert_eq!(
        winners, 1,
        "exactly one concurrent writer must win head CAS, got left={left:?} right={right:?}"
    );
    assert_eq!(
        conflicts, 1,
        "the losing writer must receive HeadRevisionConflict, got left={left:?} right={right:?}"
    );

    let persisted = store
        .load_session_window(
            &SessionId::from("concurrent-head-cas"),
            crate::store::WindowSelector::Current,
        )
        .await
        .expect("load state after concurrent head CAS")
        .expect("concurrent head-CAS winner persisted a session");
    assert_eq!(persisted.head_revision, 1, "exactly one commit applied");
    assert_eq!(persisted.window.nodes.len(), 1, "exactly one graph applied");
    let left_node_id = caller_frame_node_id(&SessionId::from(session_id), "cas-left");
    let right_node_id = caller_frame_node_id(&SessionId::from(session_id), "cas-right");
    assert!(
        persisted.window.nodes[0].node_id == left_node_id.as_str()
            || persisted.window.nodes[0].node_id == right_node_id.as_str(),
        "the persisted graph must come from one of the two writers"
    );
}

/// The store is multi-session (ADR 0112): a session the catalog never admitted
/// is `Absent` and both commits and history reads refuse it as
/// `SessionNotFound`. A second admitted session is served beside the first
/// without either seeing the other's head.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn serves_each_admitted_session_and_refuses_an_unknown_one(store: Arc<dyn RuntimeStore>) {
    let state_for = |session_id: &str| {
        let mut state = RuntimeSessionState {
            session_id: SessionId::from(session_id),
            ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
        };
        state.ensure_agent_frame_initialized();
        state
    };
    let window = |session_id: &'static str| {
        let store = Arc::clone(&store);
        async move {
            store
                .load_session_window(
                    &SessionId::from(session_id),
                    crate::store::WindowSelector::Current,
                )
                .await
                .unwrap_or_else(|error| panic!("load {session_id}'s window: {error}"))
                .unwrap_or_else(|| panic!("{session_id} has a committed head"))
        }
    };

    store
        .admit_session(&lash_core::testing::store_fixtures::root_session_request(
            &SessionId::from("alpha"),
        ))
        .await
        .expect("admit the first session");
    let alpha = state_for("alpha");
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&alpha, &[]),
        "commit-alpha",
    )
    .await
    .expect("the admitted session commits");

    let unknown = SessionId::from("never-admitted");
    let refused_commit = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(
            &state_for(unknown.as_str()),
            &[],
        ))
        .await
        .expect_err("a session the catalog never admitted cannot commit");
    assert!(
        matches!(
            refused_commit,
            crate::StoreError::SessionNotFound { ref session_id } if *session_id == unknown
        ),
        "an unknown session commit is refused as SessionNotFound, got {refused_commit:?}"
    );
    assert!(
        matches!(
            store
                .lookup_session(&unknown)
                .await
                .expect("look up a session the catalog never admitted"),
            crate::SessionLookup::Absent
        ),
        "a session the catalog never admitted is Absent"
    );
    let refused = store
        .load_ancestors(
            &unknown,
            crate::store::HistoryAnchor::Head,
            crate::store::HistoryBudget {
                max_nodes: std::num::NonZeroU32::MIN,
                max_bytes: std::num::NonZeroU64::new(1024).expect("nonzero byte budget"),
            },
        )
        .await
        .expect_err("a session with no head row has no history to read");
    assert!(
        matches!(
            refused,
            crate::StoreError::SessionNotFound { ref session_id } if *session_id == unknown
        ),
        "an unknown session is refused as SessionNotFound, got {refused:?}"
    );

    store
        .admit_session(&lash_core::testing::store_fixtures::root_session_request(
            &SessionId::from("beta"),
        ))
        .await
        .expect("admit a second session beside the first");
    let beta = state_for("beta");
    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&beta, &[]),
        "commit-beta",
    )
    .await
    .expect("the second admitted session commits");

    let alpha_read = window("alpha").await;
    let beta_read = window("beta").await;
    assert_eq!(alpha_read.session_id, "alpha");
    assert_eq!(beta_read.session_id, "beta");
    assert_eq!(
        alpha_read.head_revision, 1,
        "beta's commit leaves alpha's head alone"
    );
    assert_eq!(
        beta_read.head_revision, 1,
        "beta's head starts at its own first commit"
    );
    let node_ids = |read: &crate::store::SessionWindowRead| {
        read.window
            .nodes
            .iter()
            .map(|node| node.node_id.to_string())
            .collect::<Vec<_>>()
    };
    let alpha_nodes = node_ids(&alpha_read);
    let beta_nodes = node_ids(&beta_read);
    assert!(!alpha_nodes.is_empty() && !beta_nodes.is_empty());
    assert!(
        alpha_nodes.iter().all(|node| !beta_nodes.contains(node)),
        "each session's window holds only its own rows: alpha={alpha_nodes:?} beta={beta_nodes:?}"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn load_hydrates_checkpoint_and_usage(store: Arc<dyn RuntimeStore>) {
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("hydrated"),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.set_tool_state_snapshot(Some(
        ToolState::default().with_generation_for_conformance(9),
    ));
    state.set_plugin_state(Some(PluginState {
        plugins: Default::default(),
    }));
    let usage = TokenLedgerEntry {
        source: "turn".to_string(),
        model: "mock-model".to_string(),
        usage: TokenUsage {
            input_tokens: 11,
            output_tokens: 7,
            cache_read_input_tokens: 3,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 5,
        },
        usage_disposition: Default::default(),
    };

    commit_runtime_state_for_test(
        &store,
        RuntimeCommit::persisted_state_for_test(&state, &[usage]),
        "hydrate",
    )
    .await
    .expect("commit");

    let read = store
        .load_session_window(
            &SessionId::from("hydrated"),
            crate::store::WindowSelector::Current,
        )
        .await
        .expect("load")
        .expect("session");
    let checkpoint = read.checkpoint.expect("checkpoint");
    assert_eq!(read.session_id, "hydrated");
    assert_eq!(
        checkpoint
            .decode_component::<ToolState>(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT)
            .expect("decode dynamic snapshot")
            .expect("dynamic snapshot")
            .generation(),
        9
    );
    assert_eq!(read.usage.rows.len(), 1);
    assert_eq!(read.usage.rows[0].usage.input_tokens, 11);
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_read_loads_persisted_history(store: Arc<dyn RuntimeStore>) {
    let root = sample_session_node(&SessionId::from("branchy"), "root-node", None);
    let root_node_id = root.node_id.clone();
    let graph = crate::SessionGraph::from_nodes(
        vec![
            root,
            sample_session_node(
                &SessionId::from("branchy"),
                "left-node",
                Some(&root_node_id),
            ),
            sample_session_node(&SessionId::from("branchy"), "left-leaf", Some("left-node")),
        ],
        Some("left-leaf".into()),
    )
    .expect("branch fixture graph is valid");
    let state = RuntimeSessionState {
        session_id: SessionId::from("branchy"),
        current_frame_node_id: Some(
            crate::FrameNodeId::new(root_node_id.clone())
                .expect("derived test frame identity is non-empty"),
        ),
        session_graph: graph,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    let expected_node_ids = commit
        .graph
        .nodes()
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    let expected_leaf_node_id = commit.graph.leaf_node_id().cloned();
    commit_runtime_state_for_test(&store, commit, "active-path")
        .await
        .expect("commit linear graph");

    let read = store
        .load_session_window(
            &SessionId::from("branchy"),
            crate::store::WindowSelector::Current,
        )
        .await
        .expect("load session history")
        .expect("session history exists");
    assert_eq!(
        read.window
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>(),
        expected_node_ids
            .iter()
            .map(lash_core::NodeId::as_str)
            .collect::<Vec<_>>(),
        "session reads must return the persisted leaf-to-root history"
    );
    assert_eq!(read.window.leaf_node_id, expected_leaf_node_id);
}
