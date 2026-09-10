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
async fn historical_frame_switch_refuses_and_keeps_resident_config() {
    let (mut runtime, store) = freshness_runtime().await;
    let opened = runtime
        .open_agent_frame(crate::OpenAgentFrameRequest::new(
            crate::FrameKey::from_caller_material("changed-policy-frame")
                .expect("non-empty frame material"),
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

    let resident_policy_before_refusal = runtime.state.effective_policy().clone();
    let resident_protocol_options_before_refusal = runtime.state.protocol_turn_options.clone();
    let resident_frame_before_refusal = runtime.state.current_frame_node_id.clone();
    let durable_head_before_refusal = store
        .load_session_head_meta()
        .await
        .expect("load durable head before historical-frame refusal");

    let error = runtime
        .open_agent_frame(crate::OpenAgentFrameRequest::new(
            crate::FrameKey::from_caller_material("initial-frame")
                .expect("non-empty frame material"),
            crate::AgentFrameReason::new("test"),
        ))
        .await
        .expect_err("switching to a pre-existing historical frame must refuse");

    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported
    );
    assert_eq!(
        runtime.state.effective_policy(),
        &resident_policy_before_refusal
    );
    assert_eq!(
        runtime.state.protocol_turn_options,
        resident_protocol_options_before_refusal
    );
    assert_eq!(
        runtime.state.current_frame_node_id,
        resident_frame_before_refusal
    );
    let durable_head_after_refusal = store
        .load_session_head_meta()
        .await
        .expect("load durable head after historical-frame refusal");
    assert_eq!(
        durable_head_after_refusal
            .as_ref()
            .map(|head| head.head_revision),
        durable_head_before_refusal
            .as_ref()
            .map(|head| head.head_revision)
    );
    assert_eq!(
        durable_head_after_refusal
            .as_ref()
            .and_then(|head| head.current_frame_node_id.as_ref()),
        durable_head_before_refusal
            .as_ref()
            .and_then(|head| head.current_frame_node_id.as_ref())
    );
    assert_eq!(
        durable_head_after_refusal
            .as_ref()
            .and_then(|head| head.leaf_node_id.as_deref()),
        durable_head_before_refusal
            .as_ref()
            .and_then(|head| head.leaf_node_id.as_deref())
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
    runtime.resident_session.mark_graph_head_stale();
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
    assert!(!runtime.resident_session.graph_head_is_stale());
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

/// FIG-1875 (head-authoritative adoption): a resident refresh adopts the
/// durable head's prompt. Session config settles through the commanded
/// durable write (FIG-1555/FIG-1895), so the head already carries every
/// committed override — no resident copy is preserved across adoption.
#[tokio::test]
async fn resident_refresh_adopts_the_durable_head_prompt() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, 2).await;
    runtime
        .add_prompt_contribution(crate::PromptContribution::guidance(
            "Settled host change",
            "COMMITTED THROUGH THE COMMANDED WRITE",
        ))
        .await
        .expect("apply prompt change through the commanded write");

    let head_prompt = crate::PromptLayer::new().with_contribution(
        crate::PromptContribution::guidance("Advanced durable value", "THE HEAD WINS"),
    );
    let mut durable_head = store
        .load_session_head_meta()
        .await
        .expect("read durable head")
        .expect("session head exists");
    durable_head.head_revision += 1;
    durable_head.config.prompt = Some(head_prompt.clone());
    store.save_session_head_meta(durable_head).await;

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh resident graph");

    assert_eq!(
        runtime.state.effective_policy().prompt,
        head_prompt,
        "adoption is head-authoritative: the durable head's prompt wins"
    );
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

/// FIG-1875 (head-authoritative adoption): a resident refresh adopts the
/// durable head's model — there is no live-model preservation carve-out.
#[tokio::test]
async fn resident_refresh_adopts_the_durable_head_model() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, 2).await;
    let settled_model = crate::ModelSpec::builder("settled-live-model")
        .context_window_tokens(123_456)
        .build()
        .expect("settled model");
    runtime
        .update_session_config(crate::SessionConfigPatch {
            model: Some(settled_model.clone()),
            ..Default::default()
        })
        .await
        .expect("apply model change through the commanded write");

    let head_model = crate::ModelSpec::builder("advanced-durable-model")
        .context_window_tokens(65_536)
        .build()
        .expect("advanced durable model");
    let mut durable_head = store
        .load_session_head_meta()
        .await
        .expect("read durable head")
        .expect("session head exists");
    durable_head.head_revision += 1;
    durable_head.config.model = head_model.clone();
    store.save_session_head_meta(durable_head).await;

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh resident graph");

    assert_eq!(
        runtime.state.effective_policy().model,
        head_model,
        "adoption is head-authoritative: the durable head's model wins"
    );
}

/// FIG-1875 (head-authoritative adoption): a resident refresh adopts the
/// durable head's provider id. The provider *resolver* stays live-owned — it
/// is not part of the durable head — but the recorded provider id is a
/// durable fact and the head wins on it.
#[tokio::test]
async fn resident_refresh_adopts_the_durable_head_provider_id() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, 2).await;
    let settled_provider = TestProvider::builder()
        .kind("settled-live-provider")
        .complete_error("provider must not be called by refresh")
        .build()
        .into_handle();
    runtime
        .update_session_config(crate::SessionConfigPatch {
            provider: Some(settled_provider),
            ..Default::default()
        })
        .await
        .expect("apply provider change through the commanded write");

    let mut durable_head = store
        .load_session_head_meta()
        .await
        .expect("read durable head")
        .expect("session head exists");
    durable_head.head_revision += 1;
    durable_head.config.provider_id = "advanced-durable-provider".to_string();
    store.save_session_head_meta(durable_head).await;

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("refresh resident graph");

    assert_eq!(
        runtime.state.effective_policy().provider_id,
        "advanced-durable-provider",
        "adoption is head-authoritative: the durable head's provider id wins"
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

fn commanded_turn_options(dialect: &str) -> crate::ProtocolTurnOptions {
    crate::ProtocolTurnOptions {
        payload: serde_json::json!({ "dialect": dialect }),
    }
}

/// FIG-2479: the protocol-turn-options setter settles through the commanded
/// durable write — the session head accepts the value before resident state
/// publishes it.
#[tokio::test]
async fn protocol_turn_options_settle_through_the_commanded_write() {
    let (mut runtime, store) = freshness_runtime().await;
    let options = commanded_turn_options("commanded-durable");

    runtime
        .set_protocol_turn_options(options.clone())
        .await
        .expect("settle protocol turn options durably");

    assert_eq!(
        runtime.protocol_turn_options(),
        &options,
        "resident state must publish the settled value"
    );
    let head = store
        .load_session_head_meta()
        .await
        .expect("read durable head")
        .expect("session head exists");
    assert_eq!(
        head.config.protocol_turn_options,
        Some(options),
        "the durable head must have accepted the value at settlement time"
    );
}

/// FIG-2479 regression: options set before an invalidation reload survive it
/// via the head — the reload restores the commanded head value, not stale
/// resident or checkpoint state.
#[tokio::test]
async fn protocol_turn_options_set_before_invalidation_reload_survive_via_the_head() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, 2).await;
    let options = commanded_turn_options("survives-invalidation-reload");

    runtime
        .set_protocol_turn_options(options.clone())
        .await
        .expect("settle protocol turn options durably");
    runtime.invalidate_resident_session_state();
    runtime
        .reload_invalidated_resident_session_state_for_session()
        .await
        .expect("reload invalidated resident session state");

    assert_eq!(
        runtime.protocol_turn_options(),
        &options,
        "an invalidation reload must restore the settled options from the head"
    );
    let head = store
        .load_session_head_meta()
        .await
        .expect("read durable head")
        .expect("session head exists");
    assert_eq!(
        head.config.protocol_turn_options,
        Some(options),
        "the reload source is the durable head row"
    );
}

/// The all-frames setter shares the commanded settlement path.
#[tokio::test]
async fn protocol_turn_options_all_frames_setter_settles_durably() {
    let (mut runtime, store) = freshness_runtime().await;
    let options = commanded_turn_options("all-frames-commanded");

    runtime
        .set_protocol_turn_options_all_frames(options.clone())
        .await
        .expect("settle all-frames protocol turn options durably");

    assert_eq!(runtime.protocol_turn_options(), &options);
    let head = store
        .load_session_head_meta()
        .await
        .expect("read durable head")
        .expect("session head exists");
    assert_eq!(head.config.protocol_turn_options, Some(options));
}

/// FIG-1875 pin (a): a live policy override followed by an invalidation
/// reload yields the durable head's values. The override settles through the
/// commanded write; when a competing executor then advances the head, the
/// reload adopts that head head-authoritatively — no resident-copy
/// preservation masks the advance.
#[tokio::test]
async fn live_policy_override_then_invalidation_reload_yields_the_head_values() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, 2).await;
    let overridden_model = crate::ModelSpec::builder("live-override-model")
        .context_window_tokens(123_456)
        .build()
        .expect("override model");
    runtime
        .update_session_config(crate::SessionConfigPatch {
            model: Some(overridden_model),
            ..Default::default()
        })
        .await
        .expect("apply the live override through the commanded write");
    runtime
        .add_prompt_contribution(crate::PromptContribution::guidance(
            "Live override",
            "SETTLED THROUGH THE COMMANDED WRITE",
        ))
        .await
        .expect("apply the live prompt override");

    let head_model = crate::ModelSpec::builder("advanced-head-model")
        .context_window_tokens(65_536)
        .build()
        .expect("advanced head model");
    let head_prompt = crate::PromptLayer::new().with_contribution(
        crate::PromptContribution::guidance("Advanced durable value", "THE HEAD WINS"),
    );
    let mut durable_head = store
        .load_session_head_meta()
        .await
        .expect("read durable head")
        .expect("session head exists");
    durable_head.head_revision += 1;
    durable_head.config.model = head_model.clone();
    durable_head.config.prompt = Some(head_prompt.clone());
    store.save_session_head_meta(durable_head).await;

    runtime.invalidate_resident_session_state();
    runtime
        .reload_invalidated_resident_session_state_for_session()
        .await
        .expect("reload invalidated resident session state");

    assert_eq!(
        runtime.state.effective_policy().model,
        head_model,
        "the invalidation reload must adopt the head's model"
    );
    assert_eq!(
        runtime.state.effective_policy().prompt,
        head_prompt,
        "the invalidation reload must adopt the head's prompt"
    );
    assert_eq!(
        *runtime.resident_session.validity(),
        ResidentSessionState::Valid
    );
}

/// FIG-1875 pin (b): a successful invalidation reload settles the freshness
/// facts — `Valid` plus `graph_loaded_from_store` — so the turn that
/// triggered it issues no second durable probe (`load_session_head_meta`)
/// on top of the full reload it already performed.
#[tokio::test]
async fn successful_invalidation_reload_issues_no_extra_head_meta_probe() {
    let store = Arc::new(RecordingStore::default());
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "reload settled the freshness facts".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        }]),
        test_host_config(),
        store.clone() as Arc<dyn crate::RuntimePersistence>,
    )
    .await;
    append_history(&mut runtime, 2).await;

    runtime.invalidate_resident_session_state();
    let head_probes_before = store.load_session_head_meta_count();
    let full_loads_before = store.load_session_count();

    let turn = runtime
        .run_turn_assembled(
            TurnInput::text("drive the invalidated turn"),
            CancellationToken::new(),
            named_turn_scope("root", "reload-settles-freshness"),
        )
        .await
        .expect("the invalidated turn reloads and runs");
    assert_eq!(
        turn.assistant_output.safe_text,
        "reload settled the freshness facts"
    );

    assert_eq!(
        store.load_session_count() - full_loads_before,
        1,
        "the invalidation reload performs exactly one full durable read"
    );
    assert_eq!(
        store.load_session_head_meta_count() - head_probes_before,
        0,
        "a successful reload settles freshness; no bounded head probe may follow it"
    );
    assert_eq!(
        *runtime.resident_session.validity(),
        ResidentSessionState::Valid
    );
    assert!(
        runtime.resident_session.graph_loaded_from_store(),
        "the reload settles graph_loaded_from_store"
    );
}

fn reopen_prompt(label: &str) -> crate::PromptLayer {
    crate::PromptLayer::new().with_contribution(crate::PromptContribution::guidance(label, label))
}

#[tokio::test]
async fn reopen_seed_delayed_retry_adopts_advanced_head() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, 2).await;
    let base = store.load_session_head_meta().await.unwrap().unwrap();
    runtime.state.policy.prompt = reopen_prompt("seed");
    let retry = runtime.state.clone();
    runtime
        .settle_reopen_seeded_config(&base.config)
        .await
        .unwrap();
    runtime
        .update_session_config(crate::SessionConfigPatch::with_prompt(reopen_prompt(
            "newer",
        )))
        .await
        .unwrap();
    let newer = store.load_session_head_meta().await.unwrap().unwrap();
    runtime.state = retry;
    runtime
        .settle_reopen_seeded_config(&base.config)
        .await
        .unwrap();
    assert_eq!(runtime.state.policy.prompt, reopen_prompt("newer"));
    assert_eq!(runtime.state.head_revision, newer.head_revision);
    let after = store.load_session_head_meta().await.unwrap().unwrap();
    assert_eq!(after.head_revision, newer.head_revision);
    assert_eq!(after.config, newer.config);
}

#[tokio::test]
async fn reopen_seed_same_base_replay_is_idempotent() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, 2).await;
    let base = store.load_session_head_meta().await.unwrap().unwrap();
    runtime.state.policy.prompt = reopen_prompt("seed");
    let retry = runtime.state.clone();
    runtime
        .settle_reopen_seeded_config(&base.config)
        .await
        .unwrap();
    let committed = store.load_session_head_meta().await.unwrap().unwrap();
    runtime.state = retry;
    runtime
        .settle_reopen_seeded_config(&base.config)
        .await
        .unwrap();
    assert_eq!(runtime.state.policy.prompt, reopen_prompt("seed"));
    assert_eq!(runtime.state.head_revision, committed.head_revision);
    let after = store.load_session_head_meta().await.unwrap().unwrap();
    assert_eq!(after.head_revision, committed.head_revision);
    assert_eq!(after.config, committed.config);
}

#[tokio::test]
async fn reopen_seed_alternating_seeds_advance_without_panicking() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, 2).await;
    for label in ["a", "b", "a", "b"] {
        let base = store.load_session_head_meta().await.unwrap().unwrap();
        runtime.state.policy.prompt = reopen_prompt(label);
        runtime
            .settle_reopen_seeded_config(&base.config)
            .await
            .unwrap();
        let head = store.load_session_head_meta().await.unwrap().unwrap();
        assert!(head.head_revision > base.head_revision);
        assert_eq!(head.config.prompt, Some(reopen_prompt(label)));
        assert_eq!(runtime.state.head_revision, head.head_revision);
    }
}

/// FIG-2782: the checkpoint-adopt path owns its own rehydration of the
/// outstanding usage-attempt registry, and nothing else covers it.
///
/// Session construction rebuilds the registry from the durable ledger it opened
/// on, so every reopen witness stays green even with the adopt-path rebuild
/// deleted. This drives the other order: a live handle whose registry is empty
/// adopts a head that already carries holes, while a resident correction that
/// has not settled into any durable row is layered on top. The rebuilt set must
/// be the durable holes minus the ones the resident correction already fills,
/// which is only true if the adopt path rebuilds it at all.
#[tokio::test]
async fn checkpoint_adopt_rehydrates_outstanding_usage_attempts_from_the_adopted_head() {
    let (mut runtime, store) = freshness_runtime().await;
    append_history(&mut runtime, 2).await;
    let model = runtime.state.effective_policy().model.id.clone();

    // Durable holes committed outside this handle: two billed attempts whose
    // usage never arrived, carried by one accumulating `(source, model)` row.
    let durable_holes = crate::TokenLedgerEntry {
        source: "turn".to_string(),
        model: model.clone(),
        usage: crate::TokenUsage::default(),
        usage_disposition: crate::LedgerUsageDisposition::unreported([
            crate::UnreportedLedgerAttempt {
                call_id: "fig2782-call-alpha".to_string(),
                attempt_ordinal: 1,
                generation_id: Some("gen-alpha".to_string()),
            },
            crate::UnreportedLedgerAttempt {
                call_id: "fig2782-call-beta".to_string(),
                attempt_ordinal: 1,
                generation_id: Some("gen-beta".to_string()),
            },
        ]),
    };
    let operation = crate::store::OperationId::new(
        crate::ExecutionScope::runtime_operation("fig2782-external-usage-writer"),
        "commit",
    );
    store.usage_deltas.lock_recover().extend(
        crate::store::RuntimeUsageDelta::for_operation(&operation, &[durable_holes])
            .expect("durable usage delta identities"),
    );

    // A resident correction for one of those holes: recorded on the shared
    // ledger by a reconciliation that has not ridden a commit boundary yet, so
    // it exists only in memory while its hole is still durable.
    runtime.shared_token_ledger.lock_recover().push(
        crate::runtime::session_manager::PendingTokenLedgerEntry::unstaged(
            crate::TokenLedgerEntry {
                source: "turn".to_string(),
                model: model.clone(),
                usage: crate::TokenUsage {
                    input_tokens: 334,
                    ..crate::TokenUsage::default()
                },
                usage_disposition: crate::LedgerUsageDisposition::Reconciled {
                    call_id: "fig2782-call-beta".to_string(),
                    attempt_ordinal: 1,
                },
            },
        ),
    );

    assert!(
        runtime.unreported_usage_attempts().is_empty(),
        "the live handle must start with nothing registered, or the adopt is not what fills it"
    );

    let mut advanced = store
        .load_session_head_meta()
        .await
        .expect("read durable head")
        .expect("session head exists");
    advanced.head_revision += 1;
    store.save_session_head_meta(advanced.clone()).await;
    let head_before_adopt = runtime.state.head_revision;

    runtime
        .refresh_session_graph_from_store()
        .await
        .expect("adopt the advanced durable head");

    assert!(
        runtime.state.head_revision > head_before_adopt,
        "the adopt must actually advance the head, or the adopt path never ran"
    );
    assert_eq!(runtime.state.head_revision, advanced.head_revision);
    assert_eq!(
        runtime.unreported_usage_attempts(),
        [crate::UnreportedUsageAttempt {
            call_id: "fig2782-call-alpha".to_string(),
            attempt_ordinal: 1,
            source: "turn".to_string(),
            model: model.clone(),
            generation_id: Some("gen-alpha".to_string()),
        }],
        "the adopted head must rebuild the outstanding set: every durable hole, \
         minus the one the resident correction already fills"
    );
    assert_eq!(
        runtime.shared_token_ledger.lock_recover().len(),
        1,
        "rebuilding from the resident correction must not consume it"
    );
}
