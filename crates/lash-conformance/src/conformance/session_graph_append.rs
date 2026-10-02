//! Branch-liveness conformance for
//! [`AppendSessionNodesRequest::requires_ancestor_node_id`](crate::AppendSessionNodesRequest::requires_ancestor_node_id).
//!
//! The precondition is deliberately *not* a compare-and-swap on the session
//! head: a derive-then-append caller whose base was merely overtaken keeps its
//! work, and only a caller whose base has left the active path is refused. Both
//! halves are load-bearing and both are asserted here, against every backend,
//! for both public append entry points (a host's append, a session command
//! the shift applies at a turn boundary (FIG-4202), and the plugin-facing
//! [`SessionGraphService`](crate::plugin::SessionGraphService)).

use super::*;
use crate::facade_support::SessionGraphFacadeOps;
use lash_core::plugin::PluginSessionRequest;
use pretty_assertions::assert_eq;

pub async fn session_graph_append_branch_liveness(factory: Arc<dyn crate::DeploymentStore>) {
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
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_graph_append_tolerates_an_advanced_head(
    factory: &Arc<dyn crate::DeploymentStore>,
) {
    let request = session_store_request(
        &SessionId::from("append-advanced-head"),
        "append-fence-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .admit_view(&request)
        .await
        .expect("create advanced-head session store");
    let mut runtime = append_conformance_runtime(store.store(), &request).await;

    // The base a derive-then-append caller reads and derives from.
    let observed_base = Box::pin(append_conformance_plugin_node(
        &mut runtime,
        store.store().as_ref(),
        "observe-base",
        0,
    ))
    .await;

    // Another writer advances the durable head while the derivation runs.
    let advanced_leaf = advance_durable_head_behind_the_runtime(&store).await;
    assert_ne!(
        advanced_leaf, observed_base,
        "the scenario needs the head to have moved past the observed base"
    );

    let result = Box::pin(crate::testing::runtime_helpers::apply_host_append(
        &mut runtime,
        store.store().as_ref(),
        derived_append_request(&observed_base, "derived-append"),
    ))
    .await
    .expect("an ancestor base is a live branch, not a store error");

    let appended = assert_appended_onto_current_leaf(
        &store,
        result,
        &observed_base,
        &advanced_leaf,
        "the host append command",
    )
    .await;
    assert_ne!(appended, advanced_leaf);
}

/// Same contract through the plugin seam, where the service captured its
/// snapshot before the head moved — the shape a post-turn hook actually has.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_graph_service_append_tolerates_an_advanced_head(
    factory: &Arc<dyn crate::DeploymentStore>,
) {
    let request = session_store_request(
        &SessionId::from("service-append-advanced-head"),
        "append-fence-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .admit_view(&request)
        .await
        .expect("create service advanced-head session store");
    let mut runtime = append_conformance_runtime(store.store(), &request).await;
    let observed_base = Box::pin(append_conformance_plugin_node(
        &mut runtime,
        store.store().as_ref(),
        "observe-base",
        0,
    ))
    .await;

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
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_graph_append_rejects_an_abandoned_branch(
    factory: &Arc<dyn crate::DeploymentStore>,
) {
    let scenario = Box::pin(abandoned_branch_scenario(factory, "append-abandoned")).await;
    let mut runtime =
        append_conformance_runtime(scenario.branch.store(), &scenario.branch_request).await;
    let before = read_conformance_session(&scenario.branch).await;

    let result = Box::pin(crate::testing::runtime_helpers::apply_host_append(
        &mut runtime,
        scenario.branch.store().as_ref(),
        derived_append_request(&scenario.abandoned_base, "abandoned-append"),
    ))
    .await
    .expect("an abandoned branch is a typed outcome, not a store error");

    assert_stale_branch_changed_nothing(
        &scenario.branch,
        result,
        &scenario.abandoned_base,
        before,
        SettlementCommits::One,
        "the host append command",
    )
    .await;
}

/// Same refusal through the plugin seam.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_graph_service_append_rejects_an_abandoned_branch(
    factory: &Arc<dyn crate::DeploymentStore>,
) {
    let scenario = Box::pin(abandoned_branch_scenario(
        factory,
        "service-append-abandoned",
    ))
    .await;
    let runtime =
        append_conformance_runtime(scenario.branch.store(), &scenario.branch_request).await;
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
        SettlementCommits::None,
        "SessionGraphService::append_session_nodes",
    )
    .await;
}

/// A session whose active path has abandoned `abandoned_base`, reached the way
/// a host actually rewinds under ADR 0047: retain a node, create a session
/// there, and let the descendants of that node belong to the old line only.
struct AbandonedBranchScenario {
    branch_request: crate::SessionStoreCreateRequest,
    branch: crate::store::SessionStore,
    abandoned_base: String,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn abandoned_branch_scenario(
    factory: &Arc<dyn crate::DeploymentStore>,
    prefix: &str,
) -> AbandonedBranchScenario {
    let source_request = session_store_request(
        &SessionId::fixture(format!("{prefix}-source")),
        "append-fence-model",
        crate::SessionRelation::Root,
    );
    let source = factory
        .admit_view(&source_request)
        .await
        .expect("create abandoned-branch source store");
    let mut source_runtime = append_conformance_runtime(source.store(), &source_request).await;
    let fork_point = Box::pin(append_conformance_plugin_node(
        &mut source_runtime,
        source.store().as_ref(),
        "fork-point",
        0,
    ))
    .await;
    // The rewind target is the revision this append published: the source
    // retains it without a pin until the host collects.
    let fork_revision = super::helpers::revision_at(
        factory.as_ref(),
        &source_request.session_id,
        fork_point.as_str(),
    )
    .await;
    // The base the caller read and derived from, on the line that is about to
    // be abandoned.
    let abandoned_base = Box::pin(append_conformance_plugin_node(
        &mut source_runtime,
        source.store().as_ref(),
        "abandoned-base",
        1,
    ))
    .await;

    let branch_request = crate::ForkSessionRequest {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::fixture(format!("{prefix}-branch")),
        source_session_id: source_request.session_id.clone(),
        head_revision: fork_revision,
        relation: crate::SessionRelation::Root,
        config: source_request.config.session_policy().into(),
    };
    factory
        .fork_session(&branch_request)
        .await
        .expect("create the rewound session at the retained node");
    let branch_open_request = crate::SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: branch_request.session_id.clone(),
        relation: branch_request.relation.clone(),
        config: branch_request.config.clone(),
        head: crate::SessionCreationHead::Config,
    };
    let branch = factory
        .live_view_for(&branch_open_request)
        .await
        .expect("open the rewound session")
        .expect("the rewound session exists");

    assert!(
        crate::conformance::helpers::node_readable(&source, &abandoned_base)
            .await
            .expect("load the abandoned base from shared history"),
        "the abandoned base must still exist in shared history: the fence is \
         about active-path membership, not about node existence"
    );
    let branch_read = read_conformance_session(&branch).await;
    assert_eq!(
        branch_read.window.leaf_node_id.as_deref(),
        Some(fork_point.as_str()),
        "the rewound session executes from the retained node"
    );
    assert!(
        !branch_read.window.active_path_contains(&abandoned_base),
        "the rewound session must have abandoned the base's branch"
    );

    AbandonedBranchScenario {
        branch_request: branch_open_request,
        branch,
        abandoned_base,
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn assert_appended_onto_current_leaf(
    store: &crate::store::SessionStore,
    result: crate::AppendSessionNodesOutcome,
    observed_base: &str,
    advanced_leaf: &str,
    entry_point: &str,
) -> String {
    let crate::AppendSessionNodesOutcome::Appended {
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
    assert_eq!(leaf_node_id, Some(appended.clone()));

    let read = read_conformance_session(store).await;
    let node = read
        .window
        .find_node(&appended)
        .expect("the appended node is durable");
    assert_eq!(
        node.parent_node_id.as_deref(),
        Some(advanced_leaf),
        "{entry_point}: the append must parent on the current leaf, not on the \
         ancestor it required"
    );
    assert_eq!(
        read.window.leaf_node_id.as_deref(),
        Some(appended.as_str()),
        "{entry_point}: the durable leaf must be the appended node"
    );

    let path = read
        .window
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
    for window in read.window.active_path_nodes().windows(2) {
        assert_eq!(
            window[1].parent_node_id.as_deref(),
            Some(window[0].node_id.as_str()),
            "{entry_point}: the active path must be a single parent chain"
        );
    }
    appended.to_string()
}

/// How many head commits a refused append's entry point makes to settle it.
/// A host append is a session command: its refusal settles in one commit that
/// completes the command's row and carries the typed outcome, which advances
/// the head revision and writes nothing else. A plugin's in-turn append
/// refuses without a commit.
#[derive(Clone, Copy)]
enum SettlementCommits {
    None,
    One,
}

async fn assert_stale_branch_changed_nothing(
    store: &crate::store::SessionStore,
    result: crate::AppendSessionNodesOutcome,
    abandoned_base: &str,
    before: crate::store::SessionWindowRead,
    settlement: SettlementCommits,
    entry_point: &str,
) {
    let crate::AppendSessionNodesOutcome::StaleBranch { required_node_id } = result else {
        panic!("{entry_point}: an abandoned base must be refused: {result:?}");
    };
    assert_eq!(
        required_node_id, abandoned_base,
        "{entry_point}: the refusal must name the base that lost its branch"
    );

    let after = read_conformance_session(store).await;
    let settlement_commits = match settlement {
        SettlementCommits::None => 0,
        SettlementCommits::One => 1,
    };
    assert_eq!(
        after.head_revision,
        before.head_revision + settlement_commits,
        "{entry_point}: a refused append moves the head revision only by its settlement"
    );
    assert_eq!(
        after.window.leaf_node_id, before.window.leaf_node_id,
        "{entry_point}: a refused append must not move the leaf"
    );
    assert_eq!(
        after
            .window
            .nodes
            .iter()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>(),
        before
            .window
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn append_conformance_runtime(
    store: &Arc<dyn crate::RuntimeStore>,
    request: &crate::SessionStoreCreateRequest,
) -> crate::LashRuntime {
    let state = crate::conformance::helpers::load_window_state(store, &request.session_id)
        .await
        .expect("load session state for the append conformance runtime")
        .unwrap_or_else(|| crate::RuntimeSessionState {
            session_id: request.session_id.clone(),
            policy: request.config.session_policy(),
            ..crate::RuntimeSessionState::new(request.config.session_policy())
        });
    // The protocol-session capability is embedder-supplied; the in-tree fake is
    // enough here because this suite never runs a turn.
    let host = crate::PluginHost::new(crate::testing::test_standard_protocol_factories());
    let plugins = match state.plugin_state() {
        Some(snapshot) => host.build_session(PluginSessionRequest::rematerialization(
            request.session_id.clone(),
            snapshot,
            crate::plugin::SessionAuthorityContext {
                plugin_config: state.admitted_plugin_config(),
                ..Default::default()
            },
        )),
        None => host.build_session(PluginSessionRequest::creation(
            request.session_id.clone(),
            Default::default(),
        )),
    }
    .expect("append conformance plugin session");
    let runtime_host = crate::EmbeddedRuntimeHost::new(crate::StoreLawBackend::new().host_config(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    ));
    let runtime_services = crate::PersistentRuntimeServices::new(
        plugins,
        crate::conformance::helpers::session_view(store, request.session_id.clone()),
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    crate::LashRuntime::from_persistent_embedded_state(
        request.config.session_policy(),
        runtime_host,
        runtime_services,
        state,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .expect("append conformance runtime")
}

/// Rewrite a committed receipt to a genuine pre-upgrade JSON shape and prove
/// that the public runtime result still returns the original non-empty leaf.
///
/// `rewrite_receipt` performs backend-specific row surgery after the first
/// append, removing the newer result fields from durable storage while leaving
/// the receipt's request identity intact so the public replay path is exercised.
///
/// Integrator class (ADR 0051): **conformance-suite embedders**.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn old_format_append_receipt_returns_public_leaf<F, Fut>(
    store: Arc<dyn crate::RuntimeStore>,
    rewrite_receipt: F,
) where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let request = session_store_request(
        &SessionId::from("root"),
        "old-format-append-receipt-model",
        crate::SessionRelation::Root,
    );
    let runtime = append_conformance_runtime(&store, &request).await;
    // The plugin-facing service writes through the store's append receipt:
    // a host's append is a session command, whose own receipt is its batch.
    let service = runtime
        .session_graph_service()
        .expect("session graph service");
    Box::pin(service.append_session_nodes(
        &request.session_id,
        crate::AppendSessionNodesRequest {
            operation_id: "old-format-append-receipt-seed".to_string(),
            nodes: vec![crate::SessionAppendNode::plugin(
                "old-format-append-receipt-seed",
                serde_json::json!({"seed": true}),
            )],
            requires_ancestor_node_id: None,
        },
    ))
    .await
    .expect("seed old-format fixture session");
    let append = crate::AppendSessionNodesRequest {
        operation_id: "old-format-append-receipt".to_string(),
        nodes: vec![crate::SessionAppendNode::plugin(
            "old-format-append-receipt",
            serde_json::json!({"value": 1}),
        )],
        requires_ancestor_node_id: None,
    };
    let first = Box::pin(service.append_session_nodes(&request.session_id, append.clone()))
        .await
        .expect("first old-format fixture append");
    rewrite_receipt().await;
    let replay = Box::pin(service.append_session_nodes(&request.session_id, append))
        .await
        .expect("old-format fixture receipt replay");
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
        panic!("old-format append fixture must return Appended")
    };
    assert!(first_leaf.is_some());
    assert_eq!(replay_node_ids, first_node_ids);
    assert_eq!(replay_leaf, first_leaf);
    assert!(
        replay_leaf.is_some(),
        "legacy fallback must be a real node id"
    );
}

/// Append one plugin node as a host append the command lane applies, and
/// return its durable id.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn append_conformance_plugin_node(
    runtime: &mut crate::LashRuntime,
    store: &dyn crate::RuntimeStore,
    operation_id: &str,
    step: u64,
) -> String {
    let result = Box::pin(crate::testing::runtime_helpers::apply_host_append(
        runtime,
        store,
        crate::AppendSessionNodesRequest {
            operation_id: operation_id.to_string(),
            nodes: vec![crate::SessionAppendNode::plugin(
                "append-fence-conformance",
                serde_json::json!({ "step": step }),
            )],
            requires_ancestor_node_id: None,
        },
    ))
    .await
    .expect("seed the append conformance graph");
    match result {
        crate::AppendSessionNodesOutcome::Appended { node_ids, .. } => node_ids
            .into_iter()
            .next()
            .expect("seeded node id")
            .to_string(),
        other => panic!("an unfenced append must succeed: {other:?}"),
    }
}

/// Commit one node straight to the store, so the runtime's resident head is
/// behind the durable head without the runtime ever observing the writer.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn advance_durable_head_behind_the_runtime(store: &crate::store::SessionStore) -> String {
    let mut state =
        crate::conformance::helpers::load_window_state(store.store(), store.session_id())
            .await
            .expect("load state for the concurrent writer")
            .expect("the session is already durable");
    append_conformance_event_node(
        &mut state,
        "advanced-head",
        "content the derivation never read",
    );
    commit_conformance_state(store.store(), &mut state)
        .await
        .expect("advance the durable head behind the runtime");
    state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("the advanced leaf")
        .to_string()
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
        requires_ancestor_node_id: Some(crate::NodeId::fixture(required_node_id.to_string())),
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn read_conformance_session(
    store: &crate::store::SessionStore,
) -> crate::store::SessionWindowRead {
    store
        .load_session_window(crate::store::WindowSelector::Current)
        .await
        .expect("read the durable session")
        .expect("the durable session exists")
}
