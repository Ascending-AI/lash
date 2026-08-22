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
async fn frame_switch_refreshes_policy_readers_from_target_frame() {
    let (mut runtime, _store) = freshness_runtime().await;
    let initial_frame_node_id = runtime
        .state
        .current_frame_node_id
        .clone()
        .expect("runtime initializes the initial frame");
    let initial_policy = runtime.state.effective_policy().clone();

    let opened = runtime
        .open_agent_frame(crate::OpenAgentFrameRequest::new(
            "changed-policy-frame",
            crate::AgentFrameReason::new("test"),
        ))
        .await
        .expect("open a second agent frame");
    assert!(opened.opened, "the second frame must be newly opened");

    let changed_model = crate::ModelSpec::builder("changed-frame-model")
        .context_window_tokens(123_456)
        .build()
        .expect("changed model");
    runtime
        .update_session_config(crate::SessionConfigPatch {
            model: Some(changed_model.clone()),
            ..Default::default()
        })
        .await
        .expect("change the live policy on the second frame");
    assert_eq!(runtime.state.effective_policy().model, changed_model);

    runtime
        .open_agent_frame(crate::OpenAgentFrameRequest::new(
            "initial-frame",
            crate::AgentFrameReason::new("test"),
        ))
        .await
        .expect("switch back to the pre-existing initial frame");

    assert_eq!(
        runtime.state.current_frame_node_id.as_deref(),
        Some(initial_frame_node_id.as_str())
    );
    assert_eq!(
        runtime.state.effective_policy(),
        &initial_policy,
        "the state accessor must expose the switched-to frame policy"
    );
    assert_eq!(
        runtime.session_policy(),
        initial_policy,
        "session_policy must expose the switched-to frame policy"
    );
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
async fn freshness_falls_back_to_full_read_when_head_is_indeterminate() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, 2).await;
    let head_reads_before = store.load_session_head_meta_count();
    let full_loads_before = store.load_session_count();
    runtime
        .resident_graph_head_stale
        .store(true, Ordering::Release);
    store.fail_next_load_session_head_meta();

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("an indeterminate head must fall back to the full session read");

    assert_eq!(
        store.load_session_head_meta_count() - head_reads_before,
        1,
        "freshness must attempt the bounded head projection first"
    );
    assert_eq!(
        store.load_session_count() - full_loads_before,
        1,
        "a failed head projection must not be treated as a fresh session"
    );
    assert!(!runtime.resident_graph_head_stale.load(Ordering::Acquire));
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
async fn resident_refresh_does_not_revert_live_uncommitted_prompt() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, 2).await;
    runtime
        .add_prompt_contribution(crate::PromptContribution::guidance(
            "Live host change",
            "KEEP THIS LIVE PROMPT",
        ))
        .await
        .expect("apply live prompt change");
    let live_prompt = runtime.state.effective_policy().prompt.clone();

    let mut durable_head = store
        .load_session_head_meta()
        .await
        .expect("read durable head")
        .expect("session head exists");
    durable_head.head_revision += 1;
    durable_head.config.prompt = Some(crate::PromptLayer::new().with_contribution(
        crate::PromptContribution::guidance("Stale durable value", "DO NOT RESTORE THIS"),
    ));
    store.save_session_head_meta(durable_head).await;

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh resident graph");

    assert_eq!(runtime.state.effective_policy().prompt, live_prompt);
}

#[tokio::test]
async fn prompt_helper_composes_with_reloaded_prompt_on_invalidated_resident_path() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, 2).await;

    let mut durable_head = store
        .load_session_head_meta()
        .await
        .expect("read durable head")
        .expect("session head exists");
    durable_head.head_revision += 1;
    durable_head.config.prompt = Some(crate::PromptLayer::new().with_contribution(
        crate::PromptContribution::guidance("Durable base", "KEEP THE DURABLE PROMPT"),
    ));
    store.save_session_head_meta(durable_head).await;
    runtime.invalidate_resident_session_state();

    runtime
        .add_prompt_contribution(crate::PromptContribution::guidance(
            "Live edit",
            "KEEP THE LIVE EDIT",
        ))
        .await
        .expect("apply prompt edit after resident reload");

    assert_eq!(
        runtime.state.effective_policy().prompt,
        crate::PromptLayer::new()
            .with_contribution(crate::PromptContribution::guidance(
                "Durable base",
                "KEEP THE DURABLE PROMPT",
            ))
            .with_contribution(crate::PromptContribution::guidance(
                "Live edit",
                "KEEP THE LIVE EDIT",
            )),
        "the helper must edit the prompt reloaded by the config applier"
    );
}

#[tokio::test]
async fn resident_refresh_does_not_revert_live_uncommitted_model() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, 2).await;
    let live_model = crate::ModelSpec::builder("live-uncommitted-model")
        .context_window_tokens(123_456)
        .build()
        .expect("live model");
    runtime
        .update_session_config(crate::SessionConfigPatch {
            model: Some(live_model.clone()),
            ..Default::default()
        })
        .await
        .expect("apply live model change");

    let mut durable_head = store
        .load_session_head_meta()
        .await
        .expect("read durable head")
        .expect("session head exists");
    durable_head.head_revision += 1;
    durable_head.config.model = crate::ModelSpec::builder("stale-durable-model")
        .context_window_tokens(65_536)
        .build()
        .expect("stale durable model");
    store.save_session_head_meta(durable_head).await;

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh resident graph");

    assert_eq!(runtime.state.effective_policy().model, live_model);
}

#[tokio::test]
async fn resident_refresh_does_not_revert_live_uncommitted_provider() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, 2).await;
    let live_provider = TestProvider::builder()
        .kind("live-uncommitted-provider")
        .complete_error("provider must not be called by refresh")
        .build()
        .into_handle();
    runtime
        .update_session_config(crate::SessionConfigPatch {
            provider: Some(live_provider),
            ..Default::default()
        })
        .await
        .expect("apply live provider change");

    let mut durable_head = store
        .load_session_head_meta()
        .await
        .expect("read durable head")
        .expect("session head exists");
    durable_head.head_revision += 1;
    durable_head.config.provider_id = "stale-durable-provider".to_string();
    store.save_session_head_meta(durable_head).await;

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh resident graph");

    assert_eq!(
        runtime.state.effective_policy().provider_id,
        "live-uncommitted-provider"
    );
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
