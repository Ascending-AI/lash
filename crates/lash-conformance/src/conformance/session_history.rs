//! History laws shared by the SQLite and PostgreSQL catalogs (ADR 0112 §14).
#![expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]

use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;

use crate::facade_support::SessionGraphFacadeOps as _;

use crate::store::{
    ConformanceDeployment, GraphRowCorruption, HistoryAnchor, HistoryBudget, HistoryStop,
    WindowSelector,
};
use crate::{
    AgentFrameAssignment, AgentFrameReason, ForkSessionRequest, FrameKey, FrameNodeId, NodeId,
    OperationId, ProtocolTurnOptions, RuntimeCommit, RuntimeSessionState, SessionId, SessionPolicy,
    SessionRelation, StoreError, TokenLedgerEntry, TokenUsage, TurnBudget,
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
    commit_entries(store, state, &[]).await;
}

async fn commit_entries(
    store: &dyn ConformanceDeployment,
    state: &mut RuntimeSessionState,
    entries: &[TokenLedgerEntry],
) {
    commit_with_evidence(store, state, entries, Vec::new()).await;
}

async fn commit_with_evidence(
    store: &dyn ConformanceDeployment,
    state: &mut RuntimeSessionState,
    entries: &[TokenLedgerEntry],
    failure_evidence: Vec<crate::TurnFailureEvidence>,
) {
    let operation = OperationId::turn(
        &state.session_id,
        format!("history-{}", state.head_revision),
        "commit",
    );
    let (mut commit, new_ids) =
        RuntimeCommit::persisted_state_with_operation(state, entries, operation)
            .expect("prepare history commit");
    commit.failure_evidence = failure_evidence;
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

/// The ids of the last `count` nodes on the active path. Read after a
/// commit, these are the finalized ids the store persisted: a commit remaps
/// the draft ids `append_nodes` returned.
fn active_tail(state: &RuntimeSessionState, count: usize) -> Vec<NodeId> {
    let path = state.session_graph.active_path_nodes();
    path[path.len() - count..]
        .iter()
        .map(|node| node.node_id.clone())
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

async fn seed_window_fixture(
    store: &dyn ConformanceDeployment,
    extra_earlier_nodes: usize,
) -> (RuntimeSessionState, FrameNodeId) {
    let mut state = state(&format!("history-window-bounded-{extra_earlier_nodes}"));
    admit(store, &state.session_id).await;
    state.ensure_agent_frame_initialized();
    append_nodes(&mut state, 40);
    commit(store, &mut state).await;
    let mut remaining = extra_earlier_nodes;
    while remaining > 0 {
        let count = remaining.min(250);
        append_nodes(&mut state, count);
        commit(store, &mut state).await;
        remaining -= count;
    }
    let reported = (0..500)
        .map(|_| {
            TokenLedgerEntry::reported(
                "turn",
                "history-model",
                TokenUsage {
                    input_tokens: 1,
                    ..TokenUsage::default()
                },
            )
        })
        .collect::<Vec<_>>();
    for entries in reported.chunks(100) {
        commit_entries(store, &mut state, entries).await;
    }
    let hole = TokenLedgerEntry {
        source: "turn".to_string(),
        model: "history-model".to_string(),
        usage: TokenUsage::default(),
        usage_disposition: crate::LedgerUsageOutcome::unreported((0..3).map(|ordinal| {
            crate::UnreportedLedgerAttempt {
                call_id: format!("history-call-{ordinal}"),
                attempt_ordinal: 0,
                generation_id: Some(format!("history-generation-{ordinal}")),
            }
        })),
    };
    commit_entries(store, &mut state, &[hole]).await;
    for ordinal in 0..50 {
        commit_with_evidence(
            store,
            &mut state,
            &[],
            vec![crate::TurnFailureEvidence {
                partial_output: Some(crate::TurnFailurePartialOutput::Complete {
                    text: format!("failed generation {ordinal}"),
                }),
                billed_usage: crate::llm::types::LlmUsage::default(),
                refusal: crate::ChargeSafetyRefusalEvidence {
                    code: "history-fixture".to_string(),
                    denial_reason: crate::ChargeSafetyDenialReason::GuaranteeRequired,
                    protocol_position: crate::ProtocolPosition::OutputStarted,
                    attempt_number: 1,
                    attempt_count: 1,
                },
            }],
        )
        .await;
    }
    open_frame(&mut state, "history-middle-frame");
    append_nodes(&mut state, 30);
    commit(store, &mut state).await;
    let current_frame = open_frame(&mut state, "history-current-frame");
    append_nodes(&mut state, 11);
    commit(store, &mut state).await;

    (state, current_frame)
}

/// The current window decodes only its frame even after earlier history grows.
pub async fn history_window_is_frame_bounded(store: Arc<dyn ConformanceDeployment>) {
    for extra_earlier_nodes in [0, 1_000] {
        let (state, current_frame) = seed_window_fixture(store.as_ref(), extra_earlier_nodes).await;
        let before = store.decoded_row_counts_for_testing();
        let read = window(store.as_ref(), &state.session_id).await;
        let after = store.decoded_row_counts_for_testing();
        assert_eq!(read.current_frame_node_id, Some(current_frame));
        assert_eq!(read.window.nodes.len(), 12);
        assert_eq!(after.graph_node_bodies - before.graph_node_bodies, 12);
        assert_eq!(after.usage_rows - before.usage_rows, 0);
        assert_eq!(after.usage_holes - before.usage_holes, 3);
        assert_eq!(after.turn_receipt_bodies - before.turn_receipt_bodies, 0);
        assert_eq!(
            read.window.anchor().expect("anchored window").generation,
            72 + extra_earlier_nodes as u64
        );
        assert_eq!(read.usage.outstanding.len(), 3);
        assert_eq!(read.usage.rows.len(), 1);
        assert_eq!(read.usage.rows[0].usage.input_tokens, 500);

        let ledger = store
            .load_usage_ledger_page(&state.session_id, None, NonZeroU32::new(7).expect("limit"))
            .await
            .expect("first usage ledger page");
        assert_eq!(ledger.rows.len(), 7);
        assert!(ledger.next.is_some());
        let failures = store
            .load_failure_evidence_page(&state.session_id, None, NonZeroU32::new(7).expect("limit"))
            .await
            .expect("first failure evidence page");
        assert_eq!(failures.settlements.len(), 7);
        assert!(failures.next.is_some());
    }
}

/// Node and byte limits take exact prefixes, and a cursor remains pinned after an append.
pub async fn history_pages_are_bounded_and_pinned(store: Arc<dyn ConformanceDeployment>) {
    let mut state = state("history-pages");
    admit(store.as_ref(), &state.session_id).await;
    state.ensure_agent_frame_initialized();
    append_nodes(&mut state, 5);
    commit(store.as_ref(), &mut state).await;
    let initial_frame = state
        .current_frame_node_id
        .clone()
        .expect("the initial frame is current");
    open_frame(&mut state, "history-pages-second-frame");
    append_nodes(&mut state, 3);
    commit(store.as_ref(), &mut state).await;

    let first = store
        .load_ancestors(&state.session_id, HistoryAnchor::Head, budget(2, 1_048_576))
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
            budget(2, 1_048_576),
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
                budget(2, 1_048_576),
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
        seen.last().map(NodeId::as_str),
        Some(initial_frame.as_str()),
        "the last page ends at the initial frame's generation-0 row"
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

    let one = store
        .load_ancestors(
            &state.session_id,
            HistoryAnchor::Node(exact.nodes[0].record.node_id.clone()),
            budget(1, required),
        )
        .await
        .expect("one-node history lookup");
    assert_eq!(one.nodes.len(), 1);
    assert_eq!(one.nodes[0].record.node_id, exact.nodes[0].record.node_id);

    let headless = SessionId::from("history-pages-headless");
    admit(store.as_ref(), &headless).await;
    let missing = store
        .load_ancestors(&headless, HistoryAnchor::Head, budget(1, 1024))
        .await
        .expect_err("catalog admission alone creates no head row");
    assert!(matches!(missing, StoreError::SessionNotFound { .. }));

    store
        .delete_session(&state.session_id)
        .await
        .expect("delete paging fixture");
    let deleted = store
        .load_ancestors(&state.session_id, HistoryAnchor::Head, budget(1, 1024))
        .await
        .expect_err("deleted session has no readable history");
    assert!(matches!(deleted, StoreError::SessionDeleted { .. }));
}

/// A corrupt pointer or row never turns a window read into a shorter answer.
pub async fn history_window_rejects_corrupt_anchors<Make, Fut>(make: Make)
where
    Make: Fn(&'static str) -> Fut,
    Fut: std::future::Future<Output = Arc<dyn ConformanceDeployment>>,
{
    for case in [
        "base-not-frame",
        "foreign-pointer",
        "middle-row",
        "gen0-parent",
        "base-no-parent",
        "bad-size",
        "head-pointer",
    ] {
        let store = make(case).await;
        let mut state = state(&format!("history-corrupt-{case}"));
        admit(store.as_ref(), &state.session_id).await;
        state.ensure_agent_frame_initialized();
        append_nodes(&mut state, 3);
        commit(store.as_ref(), &mut state).await;
        let ids = active_tail(&state, 3);
        if case == "base-no-parent" {
            open_frame(&mut state, "history-corrupt-second-frame");
            append_nodes(&mut state, 1);
            commit(store.as_ref(), &mut state).await;
        }
        let frame = state.current_frame_node_id.clone().expect("current frame");
        match case {
            "base-not-frame" => {
                store
                    .corrupt_graph_row_for_testing(
                        &frame.clone().into_inner().into(),
                        GraphRowCorruption::SetPayloadKindToPlugin,
                    )
                    .await
            }
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
            "gen0-parent" => {
                store
                    .corrupt_graph_row_for_testing(
                        &frame.clone().into_inner().into(),
                        GraphRowCorruption::SetParent(Some(NodeId::from("missing-parent"))),
                    )
                    .await
            }
            "base-no-parent" => {
                store
                    .corrupt_graph_row_for_testing(
                        &frame.clone().into_inner().into(),
                        GraphRowCorruption::SetParent(None),
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
            "base-not-frame" | "foreign-pointer" | "gen0-parent" | "base-no-parent" => {
                assert!(matches!(
                    error,
                    StoreError::InvalidWindowAnchor { .. } | StoreError::StoredDataCorrupt { .. }
                ))
            }
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
    append_nodes(&mut source, 1);
    commit(store.as_ref(), &mut source).await;
    // A pin retains a live tip, so the fork point is pinned while it is the
    // source's leaf, before the source grows past it.
    let fork_point = active_tail(&source, 1).remove(0);
    store.pin(&fork_point).await.expect("retain fork point");
    append_nodes(&mut source, 1);
    commit(store.as_ref(), &mut source).await;
    let middle = [fork_point, active_tail(&source, 1).remove(0)];
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

    let grandchild = SessionId::from("history-fork-grandchild");
    store
        .fork_session(&ForkSessionRequest {
            session_id: grandchild.clone(),
            node_id: middle[0].clone(),
            relation: SessionRelation::Root,
            pending_observer_intents: Vec::new(),
            policy: SessionPolicy::new(TurnBudget::Unbounded),
        })
        .await
        .expect("fork an inherited node through the child lineage");
    let grandchild_read = window(store.as_ref(), &grandchild).await;
    assert_eq!(
        grandchild_read.current_frame_node_id,
        read.current_frame_node_id
    );
    assert_eq!(
        grandchild_read.window.leaf_node_id,
        read.window.leaf_node_id
    );
    assert!(
        store
            .contains_active_ancestor(&grandchild, &middle[0])
            .await
            .expect("grandchild inherited fork point")
    );
    assert!(
        !store
            .contains_active_ancestor(&grandchild, &middle[1])
            .await
            .expect("grandchild remains below source ceiling")
    );
}

/// Fork `child` from `source` at `node_id` with a root relation.
async fn fork_at(store: &dyn ConformanceDeployment, child: &SessionId, node_id: &NodeId) {
    store
        .fork_session(&ForkSessionRequest {
            session_id: child.clone(),
            node_id: node_id.clone(),
            relation: SessionRelation::Root,
            pending_observer_intents: Vec::new(),
            policy: SessionPolicy::new(TurnBudget::Unbounded),
        })
        .await
        .expect("fork at a retained node");
}

/// A one-node page anchored at `node_id`, as `session_id` reads it.
async fn one_node(
    store: &dyn ConformanceDeployment,
    session_id: &SessionId,
    node_id: &NodeId,
) -> Result<crate::store::HistoryPage, StoreError> {
    store
        .load_ancestors(
            session_id,
            HistoryAnchor::Node(node_id.clone()),
            budget(1, u64::MAX),
        )
        .await
}

fn assert_not_readable(result: Result<crate::store::HistoryPage, StoreError>, what: &str) {
    match result {
        Err(StoreError::HistoryAnchorUnavailable {
            reason: crate::store::AnchorUnavailable::NotReadable,
            ..
        }) => {}
        other => panic!("{what} must be NotReadable, got {other:?}"),
    }
}

/// ADR 0057, edge authority: A0→A1→A2 belong to A and B forks at A1. A
/// corrupt B→A ceiling raised from 1 to 2 must not let B read A2, predicate
/// it active, resume a cursor at it, page through it, or append past a
/// request naming it. The ceiling may only narrow what the edges admit.
pub async fn inflated_fork_ceiling_cannot_expose_post_fork_source_nodes(
    store: Arc<dyn ConformanceDeployment>,
) {
    let mut source = state("inflated-ceiling-source");
    admit(store.as_ref(), &source.session_id).await;
    source.ensure_agent_frame_initialized();
    append_nodes(&mut source, 1);
    commit(store.as_ref(), &mut source).await;
    let [a0, a1] = <[NodeId; 2]>::try_from(active_tail(&source, 2)).expect("A0 and A1");
    let child = SessionId::from("inflated-ceiling-child");
    fork_at(store.as_ref(), &child, &a1).await;
    append_nodes(&mut source, 1);
    commit(store.as_ref(), &mut source).await;
    let a2 = active_tail(&source, 1).remove(0);

    // Before the corruption: B reads A0 and A1, never A2.
    for inherited in [&a0, &a1] {
        one_node(store.as_ref(), &child, inherited)
            .await
            .expect("an inherited node is readable");
    }
    assert_not_readable(
        one_node(store.as_ref(), &child, &a2).await,
        "a post-fork source node under an honest ceiling",
    );

    store
        .force_fork_lineage_for_testing(&child, &a2)
        .await
        .expect("raise B's ceiling on A to A2");

    // The auditor's case: B's head is still A1.
    assert_not_readable(
        one_node(store.as_ref(), &child, &a2).await,
        "A2 through an inflated ceiling",
    );
    assert!(
        !store
            .contains_active_ancestor(&child, &a2)
            .await
            .expect("predicate over an inflated ceiling"),
        "A2 is not on B's active path"
    );
    let head = store
        .load_ancestors(&child, HistoryAnchor::Head, budget(16, u64::MAX))
        .await
        .expect("B's own ancestry stays readable");
    assert_eq!(
        head.nodes
            .iter()
            .map(|node| node.record.node_id.clone())
            .collect::<Vec<_>>(),
        vec![a1.clone(), a0.clone()],
        "B's head page is its edge path"
    );
    // A forged cursor stamped with the corrupt lineage resumes nowhere new.
    let forged = crate::store::HistoryCursor::new(
        child.clone(),
        a2.clone(),
        crate::store::LineageStamp::of_lineage([(&source.session_id, 2_u64)]),
        a2.clone(),
        2,
    );
    assert_not_readable(
        store
            .load_ancestors(&child, HistoryAnchor::Cursor(forged), budget(16, u64::MAX))
            .await,
        "a cursor resuming at A2",
    );
    // An admitted base naming A2 is no base of B's.
    match store
        .load_session_window(
            &child,
            WindowSelector::Admitted(crate::store::SessionHeadRef {
                generation: 0,
                revision: 0,
                leaf: Some(a2.clone()),
                checkpoint: None,
            }),
        )
        .await
    {
        Err(StoreError::TurnBaseNotRetained { .. }) => {}
        other => panic!("an admitted window at A2 must be refused, got {other:?}"),
    }

    // B appends B2 at A2's generation. The corrupt ceiling now admits two
    // rows there; neither the predicate nor a page may take A2 for B's. B's
    // window ends at A1, below both, so B still loads.
    let runtime: &dyn crate::store::RuntimeStore = store.as_ref();
    let mut child_state = crate::conformance::helpers::load_window_state(runtime, &child)
        .await
        .expect("load B under the corrupt ceiling")
        .expect("B has a head");
    append_nodes(&mut child_state, 1);
    commit(store.as_ref(), &mut child_state).await;
    let b2 = active_tail(&child_state, 1).remove(0);
    assert!(
        !store
            .contains_active_ancestor(&child, &a2)
            .await
            .expect("predicate beside B2"),
        "A2 is not on B's active path even at B2's generation"
    );
    assert!(
        store
            .contains_active_ancestor(&child, &a1)
            .await
            .expect("predicate over the real fork point"),
        "the corrupt ceiling does not deny A1, which the edges reach"
    );
    assert_not_readable(one_node(store.as_ref(), &child, &a2).await, "A2 beside B2");
    match store
        .load_ancestors(&child, HistoryAnchor::Head, budget(16, u64::MAX))
        .await
    {
        Ok(page) => assert!(
            page.nodes.iter().all(|node| node.record.node_id != a2),
            "B's head page must never carry A2: {:?}",
            page.nodes
                .iter()
                .map(|node| &node.record.node_id)
                .collect::<Vec<_>>()
        ),
        Err(StoreError::StoredDataCorrupt { .. }) => {}
        Err(other) => panic!("B's head page over a corrupt ceiling: {other:?}"),
    }

    // The commit fence: an append that requires A2 active is refused, and
    // one that requires A1 commits.
    let mut refused = child_state.clone();
    let commit_a2 = crate::store::append_request_commit_for_testing(
        &mut refused,
        "inflated-ceiling-requires-a2",
        &[crate::SessionAppendNode::plugin(
            "history-conformance",
            serde_json::json!({"requires": "a2"}),
        )],
        Some(a2.as_str()),
    )
    .expect("build an append requiring A2");
    match store.commit_runtime_state(commit_a2).await {
        Err(StoreError::AppendAncestorNotActive { required_node_id }) => {
            assert_eq!(required_node_id, a2);
        }
        other => panic!("an append requiring A2 must be refused, got {other:?}"),
    }
    let commit_a1 = crate::store::append_request_commit_for_testing(
        &mut child_state,
        "inflated-ceiling-requires-a1",
        &[crate::SessionAppendNode::plugin(
            "history-conformance",
            serde_json::json!({"requires": "a1"}),
        )],
        Some(a1.as_str()),
    )
    .expect("build an append requiring A1");
    let receipt = store
        .commit_runtime_state(commit_a1)
        .await
        .expect("an append requiring the real fork point commits");
    assert_ne!(receipt.committed_leaf_node_id.as_ref(), Some(&b2));
}

/// ADR 0057 and ADR 0112 §5: a history read selects its rows and confirms
/// them in one snapshot. While a writer commits one node and one usage token
/// per commit, every concurrent window read agrees with itself: its head
/// revision, leaf generation and usage totals all describe the same commit,
/// and every paged read is one edge path from its pinned leaf to the root.
pub async fn history_selection_and_confirmation_share_one_snapshot(
    store: Arc<dyn ConformanceDeployment>,
) {
    const COMMITS: u64 = 60;
    let mut state = state("history-one-snapshot");
    admit(store.as_ref(), &state.session_id).await;
    state.ensure_agent_frame_initialized();
    commit(store.as_ref(), &mut state).await;
    let session_id = state.session_id.clone();
    let base = window(store.as_ref(), &session_id).await;
    let base_revision = base.head_revision;
    let base_generation = u64::try_from(base.window.nodes.len()).expect("small window") - 1;

    let writer = {
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            for _ in 0..COMMITS {
                append_nodes(&mut state, 1);
                commit_entries(
                    store.as_ref(),
                    &mut state,
                    &[TokenLedgerEntry::reported(
                        "turn",
                        "snapshot-model",
                        TokenUsage {
                            input_tokens: 1,
                            ..TokenUsage::default()
                        },
                    )],
                )
                .await;
            }
        })
    };
    let readers = (0..2)
        .map(|_| {
            let store = Arc::clone(&store);
            let session_id = session_id.clone();
            tokio::spawn(async move {
                let mut reads = 0_u64;
                loop {
                    let read = window(store.as_ref(), &session_id).await;
                    let commits = read.head_revision - base_revision;
                    let tokens = read
                        .usage
                        .rows
                        .iter()
                        .map(|row| u64::try_from(row.usage.input_tokens).expect("tokens"))
                        .sum::<u64>();
                    assert_eq!(
                        tokens, commits,
                        "usage totals and head revision {} come from different snapshots",
                        read.head_revision
                    );
                    let leaf = read.window.nodes.last().expect("a committed window");
                    assert_eq!(read.window.leaf_node_id.as_ref(), Some(&leaf.node_id));
                    assert_eq!(
                        u64::try_from(read.window.nodes.len()).expect("small window") - 1,
                        base_generation + commits,
                        "window rows and head revision {} come from different snapshots",
                        read.head_revision
                    );
                    let page = store
                        .load_ancestors(&session_id, HistoryAnchor::Head, budget(1_000, u64::MAX))
                        .await
                        .expect("page the whole ancestry");
                    assert_eq!(page.stop, HistoryStop::Root);
                    assert_eq!(
                        page.nodes.first().map(|node| &node.record.node_id),
                        page.pinned_leaf.as_ref()
                    );
                    for pair in page.nodes.windows(2) {
                        assert_eq!(
                            pair[0].record.parent_node_id.as_ref(),
                            Some(&pair[1].record.node_id)
                        );
                    }
                    let pinned = page.pinned_leaf.clone().expect("a pinned leaf");
                    assert!(
                        store
                            .contains_active_ancestor(&session_id, &pinned)
                            .await
                            .expect("the pinned leaf stays active"),
                    );
                    reads += 1;
                    if commits == COMMITS {
                        return reads;
                    }
                }
            })
        })
        .collect::<Vec<_>>();
    writer.await.expect("snapshot writer");
    for reader in readers {
        let reads = reader.await.expect("snapshot reader");
        assert!(reads > 0, "every reader read at least once");
    }
}

/// ADR 0057: generation is a checked increment, and the check sits inside
/// the commit. A commit whose second node would overflow the stored
/// generation writes nothing: no node, no head move, no usage row, and no
/// receipt, so the identical commit is fresh once the parent is sound.
pub async fn graph_generation_overflow_rolls_back_every_write(
    store: Arc<dyn ConformanceDeployment>,
) {
    let mut state = state("history-generation-overflow");
    admit(store.as_ref(), &state.session_id).await;
    state.ensure_agent_frame_initialized();
    append_nodes(&mut state, 1);
    commit(store.as_ref(), &mut state).await;
    let leaf = active_tail(&state, 1).remove(0);
    let before_meta = store
        .load_session_head_meta(&state.session_id)
        .await
        .expect("head before the overflow")
        .expect("committed head");
    let before_usage = store
        .load_usage_totals(&state.session_id)
        .await
        .expect("usage before the overflow");
    store
        .corrupt_graph_row_for_testing(
            &leaf,
            GraphRowCorruption::SetGeneration(i64::MAX as u64 - 1),
        )
        .await
        .expect("move the leaf to the last generation below the ceiling");

    append_nodes(&mut state, 2);
    let operation = OperationId::turn(&state.session_id, "history-overflow", "commit");
    let (commit, _) = RuntimeCommit::persisted_state_with_operation(
        &mut state,
        &[TokenLedgerEntry::reported(
            "turn",
            "overflow-model",
            TokenUsage {
                input_tokens: 1,
                ..TokenUsage::default()
            },
        )],
        operation,
    )
    .expect("prepare the overflowing commit");
    let appended = commit
        .graph
        .nodes()
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(appended.len(), 2, "the commit appends two nodes");
    match store.commit_runtime_state(commit.clone()).await {
        Err(StoreError::MonotonicCounterOverflow { counter, current }) => {
            assert_eq!(counter, "session_graph_generation");
            assert_eq!(current, i64::MAX as u64);
        }
        other => panic!("the second node's generation must overflow, got {other:?}"),
    }

    let after_meta = store
        .load_session_head_meta(&state.session_id)
        .await
        .expect("head after the overflow")
        .expect("committed head");
    assert_eq!(after_meta.head_revision, before_meta.head_revision);
    assert_eq!(after_meta.leaf_node_id, before_meta.leaf_node_id);
    assert_eq!(after_meta.checkpoint_ref, before_meta.checkpoint_ref);
    assert_eq!(
        store
            .load_usage_totals(&state.session_id)
            .await
            .expect("usage after the overflow"),
        before_usage
    );
    for node_id in &appended {
        assert_not_readable(
            one_node(store.as_ref(), &state.session_id, node_id).await,
            "a node from the refused commit",
        );
    }

    store
        .corrupt_graph_row_for_testing(&leaf, GraphRowCorruption::SetGeneration(1))
        .await
        .expect("restore the leaf's generation");
    let receipt = store
        .commit_runtime_state(commit)
        .await
        .expect("the identical commit is fresh once the parent is sound");
    assert!(
        !receipt.receipt_replayed,
        "the refused commit left a receipt behind"
    );
    assert_eq!(receipt.head_revision, before_meta.head_revision + 1);
}

/// ADR 0057: frame facts are derived as nodes are appended, so the first
/// node of a root append must be a `FrameOpen`. A later `FrameOpen` in the
/// same append does not rescue the root nodes before it, and the refused
/// append writes nothing.
pub async fn a_later_frame_open_cannot_rescue_earlier_root_nodes(
    store: Arc<dyn ConformanceDeployment>,
) {
    let mut state = state("history-late-frame-open");
    admit(store.as_ref(), &state.session_id).await;
    append_nodes(&mut state, 1);
    open_frame(&mut state, "history-late-frame");
    append_nodes(&mut state, 1);
    let operation = OperationId::turn(&state.session_id, "history-late-frame", "commit");
    let (commit, _) = RuntimeCommit::persisted_state_with_operation(&mut state, &[], operation)
        .expect("prepare the root append");
    let appended = commit
        .graph
        .nodes()
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(appended.len(), 3, "plugin, FrameOpen, plugin");
    let root = appended[0].clone();
    match store.commit_runtime_state(commit).await {
        Err(StoreError::MissingFrameOpenAncestor { leaf_node_id }) => {
            assert_eq!(leaf_node_id, root, "the refusal names the unframed root");
        }
        other => panic!("a root append must open its frame first, got {other:?}"),
    }
    assert!(
        store
            .load_session_head_meta(&state.session_id)
            .await
            .expect("head after the refusal")
            .is_none_or(|head| head.leaf_node_id.is_none()),
        "the refused append published no leaf"
    );
    for node_id in &appended {
        assert_not_readable(
            one_node(store.as_ref(), &state.session_id, node_id).await,
            "a node from the refused root append",
        );
    }
}
