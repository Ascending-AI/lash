use super::*;
use crate::SessionCommitStore as _;

async fn freshness_runtime() -> (LashRuntime, Arc<RecordingStore>) {
    let store = Arc::new(RecordingStore::default());
    let runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;
    (runtime, store)
}

async fn append_history(runtime: &mut LashRuntime, depth: usize) {
    assert!(depth >= 1, "the initial frame is the first history node");
    if depth == 1 {
        return;
    }
    runtime
        .append_session_nodes(crate::AppendSessionNodesRequest {
            operation_id: format!("freshness-depth-{depth}"),
            nodes: (1..depth)
                .map(|ordinal| {
                    crate::SessionAppendNode::plugin(
                        "freshness-depth",
                        serde_json::json!({ "ordinal": ordinal }),
                    )
                })
                .collect(),
            requires_ancestor_node_id: None,
        })
        .await
        .expect("append freshness history");
    assert_eq!(runtime.state.session_graph.nodes.len(), depth);
}

#[tokio::test]
async fn unchanged_session_freshness_is_independent_of_history_depth() {
    for depth in [10, 256] {
        let (mut runtime, store) = freshness_runtime().await;
        append_history(&mut runtime, depth).await;
        let head_reads_before = store.load_session_head_meta_count();
        let full_loads_before = store.load_session_count();

        runtime
            .refresh_session_graph_from_store()
            .await
            .expect("refresh unchanged session");

        assert_eq!(
            store.load_session_count() - full_loads_before,
            0,
            "unchanged freshness must not hydrate the session at depth {depth}"
        );
        assert_eq!(
            store.load_session_head_meta_count() - head_reads_before,
            1,
            "freshness must read exactly one head projection at depth {depth}"
        );
    }
}

#[tokio::test]
async fn freshness_hydrates_when_revision_changed() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, 2).await;
    let mut head = store
        .load_session_head_meta()
        .await
        .expect("read head")
        .expect("session head exists");
    head.head_revision += 1;
    store.save_session_head_meta(head.clone()).await;
    let full_loads_before = store.load_session_count();

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh revision change");

    assert_eq!(store.load_session_count() - full_loads_before, 1);
    assert_eq!(runtime.state.head_revision, head.head_revision);
}

#[tokio::test]
async fn freshness_hydrates_when_leaf_changed() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, 2).await;
    let frame_node_id = runtime.state.session_graph.nodes[0].node_id.clone();
    let mut head = store
        .load_session_head_meta()
        .await
        .expect("read head")
        .expect("session head exists");
    assert_ne!(head.leaf_node_id.as_deref(), Some(frame_node_id.as_str()));
    head.leaf_node_id = Some(frame_node_id.clone());
    store.save_session_head_meta(head).await;
    let full_loads_before = store.load_session_count();

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh leaf change");

    assert_eq!(store.load_session_count() - full_loads_before, 1);
    assert_eq!(
        runtime.state.session_graph.leaf_node_id.as_deref(),
        Some(frame_node_id.as_str())
    );
}

#[tokio::test]
async fn freshness_hydrates_when_only_checkpoint_ref_changed() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, 2).await;
    let mut head = store
        .load_session_head_meta()
        .await
        .expect("read head")
        .expect("session head exists");
    let original_revision = head.head_revision;
    let original_leaf = head.leaf_node_id.clone();
    let changed_checkpoint_ref: crate::BlobRef = "checkpoint-ref-only-change".to_string().into();
    assert_ne!(head.checkpoint_ref.as_ref(), Some(&changed_checkpoint_ref));
    head.checkpoint_ref = Some(changed_checkpoint_ref.clone());
    store.save_session_head_meta(head).await;
    let full_loads_before = store.load_session_count();

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh checkpoint-ref-only change");

    assert_eq!(store.load_session_count() - full_loads_before, 1);
    assert_eq!(runtime.state.head_revision, original_revision);
    assert_eq!(runtime.state.session_graph.leaf_node_id, original_leaf);
    assert_eq!(
        runtime.state.checkpoint_ref.as_ref(),
        Some(&changed_checkpoint_ref)
    );
}

#[tokio::test]
async fn freshness_skips_hydration_when_nothing_changed() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, 2).await;
    let resident_head = (
        runtime.state.head_revision,
        runtime.state.session_graph.leaf_node_id.clone(),
        runtime.state.checkpoint_ref.clone(),
    );
    let full_loads_before = store.load_session_count();

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh unchanged session");

    assert_eq!(store.load_session_count() - full_loads_before, 0);
    assert_eq!(
        (
            runtime.state.head_revision,
            runtime.state.session_graph.leaf_node_id.clone(),
            runtime.state.checkpoint_ref.clone(),
        ),
        resident_head
    );
}
