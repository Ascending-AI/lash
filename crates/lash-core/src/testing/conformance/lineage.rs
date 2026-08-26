use std::sync::Arc;

use crate::{
    ForkSessionRequest, RuntimeCommit, RuntimePersistence, RuntimeSessionState, SessionRelation,
    SessionStoreCreateRequest, SessionStoreFactory, StoreError,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphFactObservation {
    pub node_id: String,
    pub parent_node_id: Option<String>,
    pub owning_session_id: String,
    pub generation: u64,
    pub frame_node_id: String,
    pub is_frame: bool,
}

#[async_trait::async_trait]
pub trait LineageConformanceInjector: Send + Sync {
    async fn force_lineage(&self, session_id: &str, ancestor_node_id: &str);
    async fn tombstone_node(&self, node_id: &str);
    async fn lineage_ancestors(&self, session_id: &str) -> Vec<crate::store::ForkLineageAncestor>;
    async fn edge_path(&self, session_id: &str) -> Vec<GraphFactObservation>;
    async fn all_graph_facts(&self) -> Vec<GraphFactObservation>;
}

#[derive(Clone)]
pub struct LineageConformanceHandles {
    pub factory: Arc<dyn SessionStoreFactory>,
    pub injector: Arc<dyn LineageConformanceInjector>,
}

async fn assert_plan_matches_edge_walk(
    injector: &Arc<dyn LineageConformanceInjector>,
    session_id: &str,
) {
    let mut expected = std::collections::BTreeMap::new();
    for fact in injector.edge_path(session_id).await {
        expected.insert(
            fact.owning_session_id.clone(),
            crate::store::ForkLineageAncestor {
                ancestor_session_id: fact.owning_session_id,
                fork_node_id: fact.node_id,
                fork_generation: fact.generation,
            },
        );
    }
    assert_eq!(
        injector.lineage_ancestors(session_id).await,
        expected.into_values().collect::<Vec<_>>(),
        "ForkPlan inherited ceilings must equal the raw parent-edge walk"
    );
}

async fn assert_readability_equals_edge_reachability(
    store: &Arc<dyn RuntimePersistence>,
    injector: &Arc<dyn LineageConformanceInjector>,
    session_id: &str,
) {
    let edge_path = injector.edge_path(session_id).await;
    let edge_ids = edge_path
        .iter()
        .map(|fact| fact.node_id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let lineage = injector.lineage_ancestors(session_id).await;
    for fact in injector.all_graph_facts().await {
        let lineage_readable = fact.owning_session_id == session_id
            || lineage.iter().any(|ancestor| {
                ancestor.ancestor_session_id == fact.owning_session_id
                    && fact.generation <= ancestor.fork_generation
            });
        assert_eq!(
            lineage_readable,
            edge_ids.contains(fact.node_id.as_str()),
            "lineage-readable iff edge-reachable for node `{}`",
            fact.node_id
        );
        assert_eq!(
            store
                .load_node(&fact.node_id)
                .await
                .expect("load node while checking lineage equivalence")
                .is_some(),
            edge_ids.contains(fact.node_id.as_str()),
            "load_node readability iff edge-reachable for node `{}`",
            fact.node_id
        );
    }
}

fn request(session_id: &str) -> SessionStoreCreateRequest {
    SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: session_id.to_string(),
        relation: SessionRelation::Root,
        policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
    }
}

async fn seed(
    factory: &Arc<dyn SessionStoreFactory>,
    session_id: &str,
    plugins: usize,
) -> (Arc<dyn RuntimePersistence>, Vec<String>) {
    let store = factory
        .create_store(&request(session_id))
        .await
        .expect("create lineage conformance store");
    let mut state = RuntimeSessionState {
        session_id: session_id.to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    for ordinal in 0..plugins {
        state.session_graph.append_plugin(
            "lineage-conformance",
            serde_json::json!({"ordinal": ordinal}),
        );
    }
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("seed lineage conformance graph");
    let read = store
        .load_session()
        .await
        .expect("load seeded lineage graph")
        .expect("seeded lineage graph exists");
    (
        store,
        read.graph
            .nodes
            .iter()
            .map(|node| node.node_id.clone())
            .collect(),
    )
}

async fn fork(
    factory: &Arc<dyn SessionStoreFactory>,
    session_id: &str,
    node_id: &str,
) -> Arc<dyn RuntimePersistence> {
    factory
        .fork_at(&ForkSessionRequest {
            pending_observer_intents: Vec::new(),
            session_id: session_id.to_string(),
            node_id: node_id.to_string(),
            relation: SessionRelation::Root,
            policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        })
        .await
        .expect("create lineage conformance fork");
    factory
        .open_existing_store(&request(session_id))
        .await
        .expect("open lineage conformance fork")
        .expect("lineage conformance fork exists")
}

async fn append(store: &Arc<dyn RuntimePersistence>, count: usize) -> Vec<String> {
    let mut state = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .expect("load lineage append state")
        .expect("lineage append state exists");
    for ordinal in 0..count {
        state.session_graph.append_plugin(
            "lineage-conformance-append",
            serde_json::json!({"ordinal": ordinal}),
        );
    }
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit lineage append");
    store
        .load_session()
        .await
        .expect("load appended lineage graph")
        .expect("appended lineage graph exists")
        .graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect()
}

pub async fn fork_lineage_conformance(handles: LineageConformanceHandles) {
    let factory = handles.factory;
    let injector = handles.injector;
    let (source, mut source_nodes) = seed(&factory, "lineage-a", 1).await;
    factory
        .pin(&source_nodes[1])
        .await
        .expect("retain the first fork ceiling");
    source_nodes = append(&source, 1).await;
    let branch = fork(&factory, "lineage-b", &source_nodes[1]).await;
    let branch_nodes = append(&branch, 2).await;
    let leaf = branch_nodes.last().expect("branch leaf").clone();
    let deep = fork(&factory, "lineage-c", &leaf).await;

    assert!(
        deep.load_node(&source_nodes[0])
            .await
            .expect("read A0")
            .is_some()
    );
    assert!(
        deep.load_node(&source_nodes[1])
            .await
            .expect("read A1")
            .is_some()
    );
    assert!(
        deep.load_node(&source_nodes[2])
            .await
            .expect("deny A2")
            .is_none()
    );
    assert!(
        deep.load_node(&leaf)
            .await
            .expect("read B ceiling")
            .is_some()
    );
    let deep_graph = deep
        .load_session()
        .await
        .expect("load distinct-ceiling graph")
        .expect("distinct-ceiling session exists")
        .graph;
    let expected_deep_nodes = [
        source_nodes[0].as_str(),
        source_nodes[1].as_str(),
        branch_nodes[2].as_str(),
        branch_nodes[3].as_str(),
    ];
    assert_eq!(
        deep_graph
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>(),
        expected_deep_nodes,
        "each ancestor must apply its own fork-generation ceiling"
    );

    let source_after = append(&source, 1).await;
    assert!(
        deep.load_node(source_after.last().expect("source post-fork node"))
            .await
            .expect("deny source append after fork")
            .is_none()
    );
    let branch_after = append(&branch, 1).await;
    assert!(
        deep.load_node(branch_after.last().expect("branch post-fork node"))
            .await
            .expect("deny branch append after deep fork")
            .is_none()
    );

    let (_unrelated, unrelated_nodes) = seed(&factory, "lineage-unrelated", 0).await;
    assert!(
        deep.load_node(&unrelated_nodes[0])
            .await
            .expect("deny unrelated node")
            .is_none()
    );

    let zero = fork(&factory, "lineage-zero", &source_nodes[1]).await;
    let zero_leaf = zero
        .load_session()
        .await
        .expect("load zero-node fork")
        .expect("zero-node fork exists")
        .graph
        .leaf_node_id
        .clone()
        .expect("zero-node fork leaf");
    let _collapsed = fork(&factory, "lineage-collapsed", &zero_leaf).await;
    assert_eq!(
        injector
            .lineage_ancestors("lineage-collapsed")
            .await
            .into_iter()
            .map(|ancestor| ancestor.ancestor_session_id)
            .collect::<Vec<_>>(),
        vec!["lineage-a".to_string()],
        "zero-node retention sources must collapse to the fork node owner"
    );

    let mut prior_leaf = source_nodes[1].clone();
    for depth in 0..12 {
        let session_id = format!("lineage-chain-{depth}");
        let chained = fork(&factory, &session_id, &prior_leaf).await;
        prior_leaf = append(&chained, 1)
            .await
            .last()
            .expect("deep fork-chain leaf")
            .clone();
    }
    let terminal = factory
        .open_existing_store(&request("lineage-chain-11"))
        .await
        .expect("open terminal fork chain")
        .expect("terminal fork chain exists");
    assert!(
        terminal
            .load_node(&source_nodes[0])
            .await
            .expect("read through deep fork chain")
            .is_some()
    );

    for session_id in ["lineage-a", "lineage-b", "lineage-c"] {
        let facts = injector.edge_path(session_id).await;
        for (index, fact) in facts.iter().enumerate() {
            assert_eq!(fact.generation, index as u64);
            assert_eq!(
                fact.parent_node_id.as_deref(),
                index
                    .checked_sub(1)
                    .map(|prior| facts[prior].node_id.as_str())
            );
            let expected_frame = facts[..=index]
                .iter()
                .rev()
                .find(|candidate| candidate.is_frame)
                .expect("every durable node has a frame ancestor");
            assert_eq!(fact.frame_node_id, expected_frame.node_id);
        }
    }

    injector
        .force_lineage("lineage-c", &unrelated_nodes[0])
        .await;
    assert!(
        deep.load_node(&unrelated_nodes[0])
            .await
            .expect("false lineage accelerator is not authority")
            .is_none(),
        "lineage-readable must imply edge-reachable"
    );

    let (_carrier_root, carrier_root_nodes) =
        seed(&factory, "lineage-deleted-carrier-root", 0).await;
    let deleted_owner = fork(&factory, "lineage-deleted-owner", &carrier_root_nodes[0]).await;
    let deleted_owner_nodes = append(&deleted_owner, 1).await;
    let deleted_owner_node = deleted_owner_nodes
        .last()
        .expect("deleted owner appended node")
        .clone();
    factory
        .pin(&deleted_owner_node)
        .await
        .expect("pin deleted-owner fork point");
    let surviving_carrier = fork(&factory, "lineage-surviving-carrier", &deleted_owner_node).await;
    append(&surviving_carrier, 1).await;
    factory
        .delete_session("lineage-deleted-owner")
        .await
        .expect("delete node-owning intermediate session");
    let recovered = fork(&factory, "lineage-after-owner-delete", &deleted_owner_node).await;
    let recovered_graph = recovered
        .load_session()
        .await
        .expect("load fork after owner deletion")
        .expect("fork after owner deletion exists")
        .graph;
    assert_eq!(
        recovered_graph
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>(),
        [carrier_root_nodes[0].as_str(), deleted_owner_node.as_str(),],
        "a surviving lineage carrier must preserve ancestors whose owner session was deleted"
    );

    injector.tombstone_node(&source_nodes[1]).await;
    let corruption = deep
        .load_node(&source_nodes[0])
        .await
        .expect_err("an intermediate tombstone must be corruption");
    assert!(matches!(corruption, StoreError::StoredDataCorrupt { .. }));
}

/// Pin a non-root-owned node, delete its owner with no descendant carrier, and
/// prove the later fork remains total over the retained edge path.
pub async fn fork_lineage_no_carrier_law(handles: LineageConformanceHandles) {
    let factory = handles.factory;
    let injector = handles.injector;
    let (_root, root_nodes) = seed(&factory, "no-carrier-root", 1).await;
    let owner = fork(
        &factory,
        "no-carrier-owner",
        root_nodes.last().expect("no-carrier root leaf"),
    )
    .await;
    let owner_nodes = append(&owner, 1).await;
    let owner_leaf = owner_nodes.last().expect("no-carrier owner leaf").clone();
    factory
        .pin(&owner_leaf)
        .await
        .expect("pin no-carrier owner leaf");
    factory
        .delete_session("no-carrier-owner")
        .await
        .expect("delete no-carrier owner");

    let recovered = fork(&factory, "no-carrier-recovered", &owner_leaf).await;
    let graph = recovered
        .load_session()
        .await
        .expect("load no-carrier fork")
        .expect("no-carrier fork exists")
        .graph;
    assert_eq!(
        graph
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>(),
        [
            root_nodes[0].as_str(),
            root_nodes[1].as_str(),
            owner_leaf.as_str(),
        ],
        "a deleted owner needs no live head or descendant lineage carrier"
    );
    assert_plan_matches_edge_walk(&injector, "no-carrier-recovered").await;
    assert_readability_equals_edge_reachability(&recovered, &injector, "no-carrier-recovered")
        .await;
}

/// Independently reconstruct the expected per-owner maxima from raw edges and
/// compare them with the backend's installed ForkPlan.
pub async fn fork_plan_matches_edge_walk_law(handles: LineageConformanceHandles) {
    let factory = handles.factory;
    let injector = handles.injector;
    let (_root, root_nodes) = seed(&factory, "plan-ground-truth-root", 1).await;
    let middle = fork(
        &factory,
        "plan-ground-truth-middle",
        root_nodes.last().expect("ground-truth root leaf"),
    )
    .await;
    let middle_nodes = append(&middle, 2).await;
    let leaf = middle_nodes
        .last()
        .expect("ground-truth middle leaf")
        .clone();
    let _deep = fork(&factory, "plan-ground-truth-deep", &leaf).await;
    assert_plan_matches_edge_walk(&injector, "plan-ground-truth-deep").await;
}
