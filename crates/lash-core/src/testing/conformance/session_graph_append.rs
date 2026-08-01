//! Branch-liveness conformance for
//! [`AppendSessionNodesRequest::requires_ancestor_node_id`](crate::AppendSessionNodesRequest::requires_ancestor_node_id).
//!
//! The precondition is deliberately *not* a compare-and-swap on the session
//! head: a derive-then-append caller whose base was merely overtaken keeps its
//! work, and only a caller whose base has left the active path is refused. Both
//! halves are load-bearing and both are asserted here, against every backend,
//! for both public append entry points (the direct
//! [`LashRuntime`](crate::LashRuntime) method and the plugin-facing
//! [`SessionGraphService`](crate::plugin::SessionGraphService)).

use super::*;

/// Run the session-graph append branch-liveness suite against `factory`.
pub async fn session_graph_append_branch_liveness(factory: Arc<dyn crate::SessionStoreFactory>) {
    Box::pin(session_graph_append_tolerates_an_advanced_head(&factory)).await;
    Box::pin(session_graph_service_append_tolerates_an_advanced_head(
        &factory,
    ))
    .await;
    Box::pin(session_graph_append_rejects_an_abandoned_branch(&factory)).await;
    Box::pin(session_graph_service_append_rejects_an_abandoned_branch(
        &factory,
    ))
    .await;
}

/// An ancestor base plus an advanced head must still append. The derivation is
/// expensive and remains true of the prefix it read, so it is kept and
/// re-parented onto the current leaf; nothing already committed is lost.
///
/// Reddens if the precondition is tightened into a head compare-and-swap
/// (`leaf_node_id == Some(required)`): the append would be refused as
/// `StaleBranch` and the derivation silently discarded.
async fn session_graph_append_tolerates_an_advanced_head(
    factory: &Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        "append-advanced-head",
        "append-fence-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .create_store(&request)
        .await
        .expect("create advanced-head session store");
    let mut runtime = append_conformance_runtime(&store, &request).await;

    // The base a derive-then-append caller reads and derives from.
    let observed_base = append_conformance_plugin_node(&mut runtime, "observe-base", 0).await;

    // Another writer advances the durable head while the derivation runs.
    let advanced_leaf = advance_durable_head_behind_the_runtime(&store).await;
    assert_ne!(
        advanced_leaf, observed_base,
        "the scenario needs the head to have moved past the observed base"
    );

    let result = runtime
        .append_session_nodes(derived_append_request(&observed_base, "derived-append"))
        .await
        .expect("an ancestor base is a live branch, not a store error");

    let appended = assert_appended_onto_current_leaf(
        &store,
        result,
        &observed_base,
        &advanced_leaf,
        "LashRuntime::append_session_nodes",
    )
    .await;
    assert_ne!(appended, advanced_leaf);
}

/// Same contract through the plugin seam, where the service captured its
/// snapshot before the head moved — the shape a post-turn hook actually has.
async fn session_graph_service_append_tolerates_an_advanced_head(
    factory: &Arc<dyn crate::SessionStoreFactory>,
) {
    let request = session_store_request(
        "service-append-advanced-head",
        "append-fence-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .create_store(&request)
        .await
        .expect("create service advanced-head session store");
    let mut runtime = append_conformance_runtime(&store, &request).await;
    let observed_base = append_conformance_plugin_node(&mut runtime, "observe-base", 0).await;

    // Captured at the observed base, exactly as a plugin hook captures it
    // before spending seconds deriving something.
    let service = runtime
        .session_graph_service()
        .expect("session graph service");
    let advanced_leaf = advance_durable_head_behind_the_runtime(&store).await;

    let result = service
        .append_session_nodes(
            &request.session_id,
            derived_append_request(&observed_base, "service-derived-append"),
        )
        .await
        .expect("an ancestor base is a live branch, not a plugin error");

    assert_appended_onto_current_leaf(
        &store,
        result,
        &observed_base,
        &advanced_leaf,
        "SessionGraphService::append_session_nodes",
    )
    .await;
}

/// A base that has left the active path must be refused with nothing written.
/// The base still exists in shared history — what changed is that this session
/// no longer executes the branch it sits on.
///
/// Reddens if the precondition is dropped: the append would commit onto the
/// branch's leaf, moving the head and durably recording a node derived from an
/// abandoned line of history.
async fn session_graph_append_rejects_an_abandoned_branch(
    factory: &Arc<dyn crate::SessionStoreFactory>,
) {
    let scenario = abandoned_branch_scenario(factory, "append-abandoned").await;
    let mut runtime = append_conformance_runtime(&scenario.branch, &scenario.branch_request).await;
    let before = read_conformance_session(&scenario.branch).await;

    let result = runtime
        .append_session_nodes(derived_append_request(
            &scenario.abandoned_base,
            "abandoned-append",
        ))
        .await
        .expect("an abandoned branch is a typed outcome, not a store error");

    assert_stale_branch_changed_nothing(
        &scenario.branch,
        result,
        &scenario.abandoned_base,
        before,
        "LashRuntime::append_session_nodes",
    )
    .await;
}

/// Same refusal through the plugin seam.
async fn session_graph_service_append_rejects_an_abandoned_branch(
    factory: &Arc<dyn crate::SessionStoreFactory>,
) {
    let scenario = abandoned_branch_scenario(factory, "service-append-abandoned").await;
    let runtime = append_conformance_runtime(&scenario.branch, &scenario.branch_request).await;
    let service = runtime
        .session_graph_service()
        .expect("session graph service");
    let before = read_conformance_session(&scenario.branch).await;

    let result = service
        .append_session_nodes(
            &scenario.branch_request.session_id,
            derived_append_request(&scenario.abandoned_base, "service-abandoned-append"),
        )
        .await
        .expect("an abandoned branch is a typed outcome, not a plugin error");

    assert_stale_branch_changed_nothing(
        &scenario.branch,
        result,
        &scenario.abandoned_base,
        before,
        "SessionGraphService::append_session_nodes",
    )
    .await;
}

/// A session whose active path has abandoned `abandoned_base`, reached the way
/// a host actually rewinds under ADR 0047: retain a node, create a session
/// there, and let the descendants of that node belong to the old line only.
struct AbandonedBranchScenario {
    branch_request: crate::SessionStoreCreateRequest,
    branch: Arc<dyn crate::RuntimePersistence>,
    abandoned_base: String,
}

async fn abandoned_branch_scenario(
    factory: &Arc<dyn crate::SessionStoreFactory>,
    prefix: &str,
) -> AbandonedBranchScenario {
    let source_request = session_store_request(
        &format!("{prefix}-source"),
        "append-fence-model",
        crate::SessionRelation::Root,
    );
    let source = factory
        .create_store(&source_request)
        .await
        .expect("create abandoned-branch source store");
    let mut source_runtime = append_conformance_runtime(&source, &source_request).await;
    let fork_point = append_conformance_plugin_node(&mut source_runtime, "fork-point", 0).await;
    factory
        .pin(&fork_point)
        .await
        .expect("retain the rewind target");
    // The base the caller read and derived from, on the line that is about to
    // be abandoned.
    let abandoned_base =
        append_conformance_plugin_node(&mut source_runtime, "abandoned-base", 1).await;

    let branch_request = crate::ForkSessionRequest {
        session_id: format!("{prefix}-branch"),
        node_id: fork_point.clone(),
        relation: crate::SessionRelation::Root,
        policy: source_request.policy.clone(),
    };
    factory
        .fork_at(&branch_request)
        .await
        .expect("create the rewound session at the retained node");
    let branch_open_request = crate::SessionStoreCreateRequest {
        session_id: branch_request.session_id.clone(),
        relation: branch_request.relation.clone(),
        policy: branch_request.policy.clone(),
    };
    let branch = factory
        .open_existing_store(&branch_open_request)
        .await
        .expect("open the rewound session")
        .expect("the rewound session exists");

    assert!(
        source
            .load_node(&abandoned_base)
            .await
            .expect("load the abandoned base from shared history")
            .is_some(),
        "the abandoned base must still exist in shared history: the fence is \
         about active-path membership, not about node existence"
    );
    let branch_read = read_conformance_session(&branch).await;
    assert_eq!(
        branch_read.graph.leaf_node_id.as_deref(),
        Some(fork_point.as_str()),
        "the rewound session executes from the retained node"
    );
    assert!(
        !branch_read.graph.active_path_contains(&abandoned_base),
        "the rewound session must have abandoned the base's branch"
    );

    AbandonedBranchScenario {
        branch_request: branch_open_request,
        branch,
        abandoned_base,
    }
}

async fn assert_appended_onto_current_leaf(
    store: &Arc<dyn crate::RuntimePersistence>,
    result: crate::AppendSessionNodesResult,
    observed_base: &str,
    advanced_leaf: &str,
    entry_point: &str,
) -> String {
    let crate::AppendSessionNodesResult::Appended {
        node_ids,
        leaf_node_id,
    } = result
    else {
        panic!(
            "{entry_point}: an ancestor base with an advanced head must keep the \
             derivation, not abandon it: {result:?}"
        );
    };
    let appended = node_ids
        .first()
        .cloned()
        .expect("the appended node's durable id");
    assert_eq!(node_ids.len(), 1);
    assert_eq!(leaf_node_id, appended);

    let read = read_conformance_session(store).await;
    let node = read
        .graph
        .find_node(&appended)
        .expect("the appended node is durable");
    assert_eq!(
        node.parent_node_id.as_deref(),
        Some(advanced_leaf),
        "{entry_point}: the append must parent on the current leaf, not on the \
         ancestor it required"
    );
    assert_eq!(
        read.graph.leaf_node_id.as_deref(),
        Some(appended.as_str()),
        "{entry_point}: the durable leaf must be the appended node"
    );

    let path = read
        .graph
        .active_path_nodes()
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    let position = |node_id: &str| {
        path.iter()
            .position(|candidate| candidate == node_id)
            .unwrap_or_else(|| panic!("{entry_point}: `{node_id}` left the active path: {path:?}"))
    };
    let base_at = position(observed_base);
    let advanced_at = position(advanced_leaf);
    let appended_at = position(&appended);
    assert!(
        base_at < advanced_at && advanced_at < appended_at,
        "{entry_point}: history must stay linear with nothing lost: {path:?}"
    );
    assert_eq!(
        appended_at,
        path.len() - 1,
        "{entry_point}: the appended node must be the tip: {path:?}"
    );
    for window in read.graph.active_path_nodes().windows(2) {
        assert_eq!(
            window[1].parent_node_id.as_deref(),
            Some(window[0].node_id.as_str()),
            "{entry_point}: the active path must be a single parent chain"
        );
    }
    appended
}

async fn assert_stale_branch_changed_nothing(
    store: &Arc<dyn crate::RuntimePersistence>,
    result: crate::AppendSessionNodesResult,
    abandoned_base: &str,
    before: crate::store::PersistedSessionRead,
    entry_point: &str,
) {
    let crate::AppendSessionNodesResult::StaleBranch { required_node_id } = result else {
        panic!("{entry_point}: an abandoned base must be refused: {result:?}");
    };
    assert_eq!(
        required_node_id, abandoned_base,
        "{entry_point}: the refusal must name the base that lost its branch"
    );

    let after = read_conformance_session(store).await;
    assert_eq!(
        after.head_revision, before.head_revision,
        "{entry_point}: a refused append must not move the head revision"
    );
    assert_eq!(
        after.graph.leaf_node_id, before.graph.leaf_node_id,
        "{entry_point}: a refused append must not move the leaf"
    );
    assert_eq!(
        after
            .graph
            .nodes
            .iter()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>(),
        before
            .graph
            .nodes
            .iter()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>(),
        "{entry_point}: a refused append must not write nodes"
    );
    assert_eq!(
        after.checkpoint_ref, before.checkpoint_ref,
        "{entry_point}: a refused append must not write a checkpoint"
    );
}

async fn append_conformance_runtime(
    store: &Arc<dyn crate::RuntimePersistence>,
    request: &crate::SessionStoreCreateRequest,
) -> crate::LashRuntime {
    let state = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .expect("load session state for the append conformance runtime")
        .unwrap_or_else(|| crate::RuntimeSessionState {
            session_id: request.session_id.clone(),
            policy: request.policy.clone(),
            ..Default::default()
        });
    // The protocol-session capability is embedder-supplied; the in-tree fake is
    // enough here because this suite never runs a turn.
    let plugins = crate::PluginHost::new(crate::testing::test_standard_protocol_factories())
        .build_session(request.session_id.clone(), state.plugin_snapshot.as_ref())
        .expect("append conformance plugin session");
    crate::LashRuntime::from_persistent_embedded_state(
        request.policy.clone(),
        crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::in_memory()),
        crate::PersistentRuntimeServices::new(plugins, Arc::clone(store)),
        state,
    )
    .await
    .expect("append conformance runtime")
}

/// Append one plugin node through the runtime and return its durable id.
async fn append_conformance_plugin_node(
    runtime: &mut crate::LashRuntime,
    operation_id: &str,
    step: u64,
) -> String {
    let result = runtime
        .append_session_nodes(crate::AppendSessionNodesRequest {
            operation_id: operation_id.to_string(),
            nodes: vec![crate::SessionAppendNode::plugin(
                "append-fence-conformance",
                serde_json::json!({ "step": step }),
            )],
            requires_ancestor_node_id: None,
        })
        .await
        .expect("seed the append conformance graph");
    match result {
        crate::AppendSessionNodesResult::Appended { node_ids, .. } => {
            node_ids.into_iter().next().expect("seeded node id")
        }
        other => panic!("an unfenced append must succeed: {other:?}"),
    }
}

/// Commit one node straight to the store, so the runtime's resident head is
/// behind the durable head without the runtime ever observing the writer.
async fn advance_durable_head_behind_the_runtime(
    store: &Arc<dyn crate::RuntimePersistence>,
) -> String {
    let mut state = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .expect("load state for the concurrent writer")
        .expect("the session is already durable");
    append_conformance_event_node(
        &mut state,
        "advanced-head",
        "content the derivation never read",
    );
    commit_conformance_state(store, &mut state)
        .await
        .expect("advance the durable head behind the runtime");
    state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("the advanced leaf")
}

fn derived_append_request(
    required_node_id: &str,
    operation_id: &str,
) -> crate::AppendSessionNodesRequest {
    crate::AppendSessionNodesRequest {
        operation_id: operation_id.to_string(),
        nodes: vec![crate::SessionAppendNode::plugin(
            "append-fence-conformance",
            serde_json::json!({ "derived_from": required_node_id }),
        )],
        requires_ancestor_node_id: Some(required_node_id.to_string()),
    }
}

async fn read_conformance_session(
    store: &Arc<dyn crate::RuntimePersistence>,
) -> crate::store::PersistedSessionRead {
    store
        .load_session()
        .await
        .expect("read the durable session")
        .expect("the durable session exists")
}
