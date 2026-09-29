//! History laws shared by the SQLite and PostgreSQL catalogs (ADR 0112 §14).

use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;

use crate::store::{
    ConformanceDeployment, GraphRowCorruption, HistoryAnchor, HistoryBudget, HistoryStop,
    WindowSelector,
};
use crate::{
    AgentFrameAssignment, AgentFrameReason, ForkSessionRequest, FrameKey, FrameNodeId, NodeId,
    OperationId, ProtocolTurnOptions, RuntimeCommit, RuntimeSessionState, SessionId, SessionPolicy,
    SessionRelation, StoreError, TurnBudget,
};

fn budget(nodes: u32, bytes: u64) -> HistoryBudget {
    HistoryBudget {
        max_nodes: NonZeroU32::new(nodes).expect("nonzero node budget"),
        max_bytes: NonZeroU64::new(bytes).expect("nonzero byte budget"),
    }
}

fn state(session_id: &str) -> RuntimeSessionState {
    RuntimeSessionState {
        session_id: SessionId::from(session_id),
        ..RuntimeSessionState::new(SessionPolicy::new(TurnBudget::Unbounded))
    }
}

async fn admit(store: &dyn ConformanceDeployment, session_id: &SessionId) {
    store
        .admit_session(&crate::testing::store_fixtures::root_session_request(
            session_id,
        ))
        .await
        .expect("admit history session");
}

async fn commit(store: &dyn ConformanceDeployment, state: &mut RuntimeSessionState) {
    let operation = OperationId::turn(
        &state.session_id,
        format!("history-{}", state.head_revision),
        "commit",
    );
    let (commit, new_ids) = RuntimeCommit::persisted_state_with_operation(state, &[], operation)
        .expect("prepare history commit");
    let receipt = store
        .commit_runtime_state(commit)
        .await
        .expect("commit history state");
    state.apply_persisted_commit_result(receipt);
    state.mark_node_ids_persisted(new_ids);
}

fn open_frame(state: &mut RuntimeSessionState, name: &str) -> FrameNodeId {
    let key = FrameKey::from_caller_material(name).expect("frame key");
    let frame = crate::session_graph::frame_node_id(&state.session_id, key.as_str());
    let opened = state.session_graph.append_frame_open_with_id_at(
        frame.clone(),
        key,
        AgentFrameReason::new("history-conformance"),
        AgentFrameAssignment::from_policy(state.policy.clone()),
        ProtocolTurnOptions::default(),
        "2026-09-29T00:00:00Z".to_string(),
    );
    assert!(opened, "fixture frame must have a fresh identity");
    state.current_frame_node_id = Some(frame.clone());
    state.agent_frames = state.session_graph.agent_frame_records(&state.session_id);
    frame
}

fn append_nodes(state: &mut RuntimeSessionState, count: usize) -> Vec<NodeId> {
    (0..count)
        .map(|ordinal| {
            state.session_graph.append_plugin(
                "history-conformance",
                serde_json::json!({"ordinal": ordinal, "revision": state.head_revision}),
            )
        })
        .collect()
}

async fn window(
    store: &dyn ConformanceDeployment,
    session_id: &SessionId,
) -> crate::store::SessionWindowRead {
    store
        .load_session_window(session_id, WindowSelector::Current)
        .await
        .expect("read current window")
        .expect("committed session has a window")
}

/// The current window decodes only its frame even after earlier history grows.
pub async fn history_window_is_frame_bounded(store: Arc<dyn ConformanceDeployment>) {
    let mut state = state("history-window-bounded");
    admit(store.as_ref(), &state.session_id).await;
    state.ensure_agent_frame_initialized();
    append_nodes(&mut state, 40);
    commit(store.as_ref(), &mut state).await;
    open_frame(&mut state, "history-middle-frame");
    append_nodes(&mut state, 30);
    commit(store.as_ref(), &mut state).await;
    let current_frame = open_frame(&mut state, "history-current-frame");
    append_nodes(&mut state, 11);
    commit(store.as_ref(), &mut state).await;

    let before = store.decoded_row_counts_for_testing();
    let read = window(store.as_ref(), &state.session_id).await;
    let after = store.decoded_row_counts_for_testing();
    assert_eq!(read.current_frame_node_id, Some(current_frame));
    assert_eq!(read.window.nodes.len(), 12);
    assert_eq!(after.graph_node_bodies - before.graph_node_bodies, 12);
    assert_eq!(after.usage_rows - before.usage_rows, 0);
    assert_eq!(after.turn_receipt_bodies - before.turn_receipt_bodies, 0);
    assert_eq!(
        read.window.anchor().expect("anchored window").generation,
        72
    );
}

/// Node and byte limits take exact prefixes, and a cursor remains pinned after an append.
pub async fn history_pages_are_bounded_and_pinned(store: Arc<dyn ConformanceDeployment>) {
    let mut state = state("history-pages");
    admit(store.as_ref(), &state.session_id).await;
    state.ensure_agent_frame_initialized();
    append_nodes(&mut state, 5);
    commit(store.as_ref(), &mut state).await;
    open_frame(&mut state, "history-pages-second-frame");
    append_nodes(&mut state, 3);
    commit(store.as_ref(), &mut state).await;

    let first = store
        .load_ancestors(&state.session_id, HistoryAnchor::Head, budget(2, u64::MAX))
        .await
        .expect("first history page");
    assert_eq!(first.nodes.len(), 2);
    assert_eq!(first.stop, HistoryStop::NodeBudget);
    let pinned_leaf = first.pinned_leaf.clone().expect("head has leaf");
    let cursor = first.next.clone().expect("more history remains");
    let foreign = store
        .load_ancestors(
            &SessionId::from("another-session"),
            HistoryAnchor::Cursor(cursor.clone()),
            budget(2, u64::MAX),
        )
        .await
        .expect_err("cursor cannot cross sessions");
    assert!(matches!(foreign, StoreError::CursorForeignSession { .. }));

    append_nodes(&mut state, 2);
    commit(store.as_ref(), &mut state).await;
    let mut seen = first
        .nodes
        .iter()
        .map(|node| node.record.node_id.clone())
        .collect::<Vec<_>>();
    let mut next = Some(cursor);
    while let Some(cursor) = next {
        let page = store
            .load_ancestors(
                &state.session_id,
                HistoryAnchor::Cursor(cursor),
                budget(2, u64::MAX),
            )
            .await
            .expect("continue pinned history");
        assert!(
            !page.nodes.is_empty(),
            "a resumable page must make progress"
        );
        assert_eq!(page.pinned_leaf.as_ref(), Some(&pinned_leaf));
        seen.extend(page.nodes.iter().map(|node| node.record.node_id.clone()));
        next = page.next;
        if next.is_none() {
            assert_eq!(page.stop, HistoryStop::Root);
        }
    }
    assert_eq!(
        seen.len(),
        10,
        "initial frame, five nodes, second frame, three nodes"
    );
    assert_eq!(
        seen.last(),
        state.session_graph.nodes.first().map(|node| &node.node_id)
    );

    let too_small = store
        .load_ancestors(
            &state.session_id,
            HistoryAnchor::Node(pinned_leaf.clone()),
            budget(2, 1),
        )
        .await
        .expect_err("first node exceeds one byte");
    let required = match too_small {
        StoreError::HistoryNodeTooLarge {
            node_id,
            required_bytes,
            max_bytes,
        } => {
            assert_eq!(node_id, pinned_leaf);
            assert_eq!(max_bytes, 1);
            required_bytes
        }
        other => panic!("expected HistoryNodeTooLarge, got {other:?}"),
    };
    let exact = store
        .load_ancestors(
            &state.session_id,
            HistoryAnchor::Node(pinned_leaf),
            budget(2, required),
        )
        .await
        .expect("exact first-node byte budget succeeds");
    assert_eq!(exact.nodes.len(), 1);
    assert_eq!(exact.stop, HistoryStop::ByteBudget);
}

/// A corrupt pointer or row never turns a window read into a shorter answer.
pub async fn history_window_rejects_corrupt_anchors<Make, Fut>(make: Make)
where
    Make: Fn(&'static str) -> Fut,
    Fut: std::future::Future<Output = Arc<dyn ConformanceDeployment>>,
{
    for case in [
        "foreign-pointer",
        "middle-row",
        "missing-parent",
        "bad-size",
        "head-pointer",
    ] {
        let store = make(case).await;
        let mut state = state(&format!("history-corrupt-{case}"));
        admit(store.as_ref(), &state.session_id).await;
        state.ensure_agent_frame_initialized();
        let ids = append_nodes(&mut state, 3);
        commit(store.as_ref(), &mut state).await;
        let frame = state.current_frame_node_id.clone().expect("initial frame");
        match case {
            "foreign-pointer" => {
                store
                    .corrupt_graph_row_for_testing(
                        &ids[1],
                        GraphRowCorruption::SetFramePointer(ids[0].clone()),
                    )
                    .await
            }
            "middle-row" => {
                store
                    .corrupt_graph_row_for_testing(&ids[1], GraphRowCorruption::DeleteRow)
                    .await
            }
            "missing-parent" => {
                store
                    .corrupt_graph_row_for_testing(
                        &frame.clone().into_inner().into(),
                        GraphRowCorruption::SetParent(Some(NodeId::from("missing-parent"))),
                    )
                    .await
            }
            "bad-size" => {
                store
                    .corrupt_graph_row_for_testing(&ids[1], GraphRowCorruption::SetBodyBytes(1))
                    .await
            }
            "head-pointer" => {
                store
                    .set_head_current_frame_for_testing(
                        &state.session_id,
                        Some(FrameNodeId::new(ids[0].clone()).expect("node id")),
                    )
                    .await
            }
            _ => unreachable!(),
        }
        .expect("inject one corrupt row");
        let error = store
            .load_session_window(&state.session_id, WindowSelector::Current)
            .await
            .expect_err("corrupt window must fail");
        match case {
            "head-pointer" => assert!(matches!(error, StoreError::CurrentFrameNodeMismatch { .. })),
            "foreign-pointer" | "missing-parent" => assert!(matches!(
                error,
                StoreError::InvalidWindowAnchor { .. } | StoreError::StoredDataCorrupt { .. }
            )),
            _ => assert!(matches!(
                error,
                StoreError::StoredDataCorrupt { .. } | StoreError::InvalidWindowAnchor { .. }
            )),
        }
    }
}

/// A fork sees inherited rows through its own ceiling, and the predicate agrees.
pub async fn history_fork_respects_ceiling(store: Arc<dyn ConformanceDeployment>) {
    let mut source = state("history-fork-source");
    admit(store.as_ref(), &source.session_id).await;
    source.ensure_agent_frame_initialized();
    append_nodes(&mut source, 2);
    commit(store.as_ref(), &mut source).await;
    let frame = open_frame(&mut source, "history-fork-middle");
    let middle = append_nodes(&mut source, 2);
    commit(store.as_ref(), &mut source).await;
    store.pin(&middle[0]).await.expect("retain fork point");
    store
        .fork_session(&ForkSessionRequest {
            session_id: SessionId::from("history-fork-child"),
            node_id: middle[0].clone(),
            relation: SessionRelation::Root,
            pending_observer_intents: Vec::new(),
            policy: SessionPolicy::new(TurnBudget::Unbounded),
        })
        .await
        .expect("fork at retained node");
    let child = SessionId::from("history-fork-child");
    append_nodes(&mut source, 2);
    commit(store.as_ref(), &mut source).await;

    let read = window(store.as_ref(), &child).await;
    assert_eq!(read.current_frame_node_id, Some(frame));
    assert_eq!(read.window.nodes.len(), 2);
    assert_eq!(read.window.leaf_node_id.as_ref(), Some(&middle[0]));
    assert!(
        store
            .contains_active_ancestor(&child, &middle[0])
            .await
            .expect("inherited node")
    );
    assert!(
        !store
            .contains_active_ancestor(&child, &middle[1])
            .await
            .expect("row above ceiling")
    );
}
