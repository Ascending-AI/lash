use lash_sansio::SessionId;
use pretty_assertions::assert_eq;
use std::sync::Arc;

use super::helpers::node_readable;
use crate::store::{HistoryAnchor, HistoryBudget, SessionStore, WindowSelector};
use crate::{
    DeploymentStore, ForkSessionRequest, RuntimeCommit, RuntimeSessionState, SessionCreationHead,
    SessionRelation, SessionStoreCreateRequest, StoreError,
};

use super::DeploymentViewExt as _;

pub use lash_core::testing::lineage::*;

async fn assert_plan_matches_edge_walk(
    injector: &Arc<dyn LineageConformanceInjector>,
    session_id: &SessionId,
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn assert_readability_equals_edge_reachability(
    store: &SessionStore,
    injector: &Arc<dyn LineageConformanceInjector>,
    session_id: &SessionId,
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
            node_readable(store, &fact.node_id)
                .await
                .expect("page one node while checking lineage equivalence"),
            edge_ids.contains(fact.node_id.as_str()),
            "one-node page readability iff edge-reachable for node `{}`",
            fact.node_id
        );
    }
}

/// The node ids of the view's current window, oldest first.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn window_node_ids(store: &SessionStore) -> Vec<lash_core::NodeId> {
    store
        .load_session_window(WindowSelector::Current)
        .await
        .expect("load the lineage window")
        .expect("the lineage session has a head")
        .window
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect()
}

fn request(session_id: &SessionId) -> SessionStoreCreateRequest {
    SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: session_id.clone(),
        relation: SessionRelation::Root,
        config: crate::PersistedSessionConfig::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
            crate::SessionToolAccess::ambient(),
        ),
        head: SessionCreationHead::Config,
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn seed(
    factory: &Arc<dyn DeploymentStore>,
    session_id: &SessionId,
    plugins: usize,
) -> (SessionStore, Vec<lash_core::NodeId>) {
    let store = factory
        .admit_view(&request(session_id))
        .await
        .expect("admit lineage conformance session");
    let mut state = RuntimeSessionState {
        session_id: SessionId::fixture(session_id.to_string()),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ))
    };
    state.ensure_agent_frame_initialized();
    for ordinal in 0..plugins {
        state.session_graph.append_plugin(
            "lineage-conformance",
            serde_json::json!({"ordinal": ordinal}),
        );
    }
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect("seed lineage conformance graph");
    let nodes = window_node_ids(&store).await;
    (store, nodes)
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
/// Fork `source`'s newest revision at `node_id` into `session_id`.
async fn fork(
    factory: &Arc<dyn DeploymentStore>,
    source: &str,
    session_id: &SessionId,
    node_id: &str,
) -> SessionStore {
    let source = SessionId::fixture(source);
    let head_revision = super::helpers::revision_at(factory.as_ref(), &source, node_id).await;
    factory
        .fork_session(&ForkSessionRequest {
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            source_session_id: source,
            head_revision,
            relation: SessionRelation::Root,
            config: crate::PersistedSessionConfig::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
                crate::SessionToolAccess::ambient(),
            ),
        })
        .await
        .expect("create lineage conformance fork");
    factory
        .live_view(session_id)
        .await
        .expect("look up lineage conformance fork")
        .expect("lineage conformance fork exists")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn append(store: &SessionStore, count: usize) -> Vec<lash_core::NodeId> {
    let mut state = crate::store::load_session_window_state(store, WindowSelector::Current)
        .await
        .map(|loaded| loaded.map(|loaded| loaded.state))
        .expect("load lineage append state")
        .expect("lineage append state exists");
    for ordinal in 0..count {
        state.session_graph.append_plugin(
            "lineage-conformance-append",
            serde_json::json!({"ordinal": ordinal}),
        );
    }
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect("commit lineage append");
    window_node_ids(store).await
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn fork_lineage_conformance(handles: LineageConformanceHandles) {
    let factory = handles.factory;
    let injector = handles.injector;
    // The first fork ceiling is a revision the source grows past; it stays
    // forkable with no pin.
    let (source, _) = seed(&factory, &SessionId::from("lineage-a"), 1).await;
    let source_nodes = append(&source, 1).await;
    let branch = fork(
        &factory,
        "lineage-a",
        &SessionId::from("lineage-b"),
        &source_nodes[1],
    )
    .await;
    let branch_nodes = append(&branch, 2).await;
    let leaf = branch_nodes.last().expect("branch leaf").clone();
    let deep = fork(&factory, "lineage-b", &SessionId::from("lineage-c"), &leaf).await;

    assert!(
        node_readable(&deep, &source_nodes[0])
            .await
            .expect("read A0")
    );
    assert!(
        node_readable(&deep, &source_nodes[1])
            .await
            .expect("read A1")
    );
    assert!(
        !node_readable(&deep, &source_nodes[2])
            .await
            .expect("deny A2")
    );
    assert!(node_readable(&deep, &leaf).await.expect("read B ceiling"));
    let deep_graph = deep
        .load_session_window(WindowSelector::Current)
        .await
        .expect("load distinct-ceiling window")
        .expect("distinct-ceiling session exists")
        .window;
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
        !node_readable(&deep, source_after.last().expect("source post-fork node"))
            .await
            .expect("deny source append after fork")
    );
    let branch_after = append(&branch, 1).await;
    assert!(
        !node_readable(&deep, branch_after.last().expect("branch post-fork node"))
            .await
            .expect("deny branch append after deep fork")
    );

    let (_unrelated, unrelated_nodes) =
        seed(&factory, &SessionId::from("lineage-unrelated"), 0).await;
    assert!(
        !node_readable(&deep, &unrelated_nodes[0])
            .await
            .expect("deny unrelated node")
    );

    let zero = fork(
        &factory,
        "lineage-a",
        &SessionId::from("lineage-zero"),
        &source_nodes[1],
    )
    .await;
    let zero_leaf = zero
        .load_session_window(WindowSelector::Current)
        .await
        .expect("load zero-node fork")
        .expect("zero-node fork exists")
        .window
        .leaf_node_id
        .clone()
        .expect("zero-node fork leaf");
    let _collapsed = fork(
        &factory,
        "lineage-zero",
        &SessionId::from("lineage-collapsed"),
        &zero_leaf,
    )
    .await;
    assert_eq!(
        injector
            .lineage_ancestors(&SessionId::from("lineage-collapsed"))
            .await
            .into_iter()
            .map(|ancestor| ancestor.ancestor_session_id)
            .collect::<Vec<_>>(),
        vec!["lineage-a".to_string()],
        "zero-node retention sources must collapse to the fork node owner"
    );

    let mut prior_leaf = source_nodes[1].clone();
    let mut prior_session = "lineage-a".to_string();
    for depth in 0..12 {
        let session_id = SessionId::fixture(format!("lineage-chain-{depth}"));
        let chained = fork(&factory, &prior_session, &session_id, &prior_leaf).await;
        prior_session = session_id.to_string();
        prior_leaf = append(&chained, 1)
            .await
            .last()
            .expect("deep fork-chain leaf")
            .clone();
    }
    let terminal = factory
        .live_view(&SessionId::from("lineage-chain-11"))
        .await
        .expect("look up terminal fork chain")
        .expect("terminal fork chain exists");
    assert!(
        node_readable(&terminal, &source_nodes[0])
            .await
            .expect("read through deep fork chain")
    );

    for session_id in ["lineage-a", "lineage-b", "lineage-c"] {
        let facts = injector.edge_path(&SessionId::from(session_id)).await;
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
        .force_lineage(&SessionId::from("lineage-c"), &unrelated_nodes[0])
        .await;
    // A forged lineage row is corrupt data. The store may refuse the probe as
    // unreadable or as corrupt, but it must never serve the unrelated node.
    let forged = super::helpers::load_one_node(
        deep.store().as_ref(),
        deep.session_id(),
        &unrelated_nodes[0],
    )
    .await;
    assert!(
        matches!(
            forged,
            Err(crate::StoreError::HistoryAnchorUnavailable {
                reason: crate::store::AnchorUnavailable::NotReadable,
                ..
            }) | Err(crate::StoreError::StoredDataCorrupt { .. })
        ),
        "lineage-readable must imply edge-reachable: a false lineage row must \
         not serve the unrelated node, got {forged:?}"
    );

    let (_carrier_root, carrier_root_nodes) = seed(
        &factory,
        &SessionId::from("lineage-deleted-carrier-root"),
        0,
    )
    .await;
    let deleted_owner = fork(
        &factory,
        "lineage-deleted-carrier-root",
        &SessionId::from("lineage-deleted-owner"),
        &carrier_root_nodes[0],
    )
    .await;
    let deleted_owner_nodes = append(&deleted_owner, 1).await;
    let deleted_owner_node = deleted_owner_nodes
        .last()
        .expect("deleted owner appended node")
        .clone();
    let surviving_carrier = fork(
        &factory,
        "lineage-deleted-owner",
        &SessionId::from("lineage-surviving-carrier"),
        &deleted_owner_node,
    )
    .await;
    append(&surviving_carrier, 1).await;
    factory
        .delete_session(&SessionId::from("lineage-deleted-owner"))
        .await
        .expect("delete node-owning intermediate session");
    // The carrier's creation revision names the deleted owner's node.
    let recovered = fork(
        &factory,
        "lineage-surviving-carrier",
        &SessionId::from("lineage-after-owner-delete"),
        &deleted_owner_node,
    )
    .await;
    let recovered_graph = recovered
        .load_session_window(WindowSelector::Current)
        .await
        .expect("load fork after owner deletion")
        .expect("fork after owner deletion exists")
        .window;
    assert_eq!(
        recovered_graph
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>(),
        [carrier_root_nodes[0].as_str(), deleted_owner_node.as_str(),],
        "a surviving lineage carrier must preserve ancestors whose owner session was deleted"
    );

    // A page walks the pinned leaf's ancestry and checks every parent edge,
    // so a retired row in the middle of it is a gap (ADR 0112 §6).
    injector.tombstone_node(&source_nodes[1]).await;
    let corruption = deep
        .load_ancestors(
            HistoryAnchor::Head,
            HistoryBudget {
                max_nodes: std::num::NonZeroU32::MAX,
                max_bytes: std::num::NonZeroU64::MAX,
            },
        )
        .await
        .expect_err("an intermediate tombstone must be corruption");
    assert!(
        matches!(corruption, StoreError::StoredDataCorrupt { .. }),
        "an intermediate tombstone is a gap in the ancestry: {corruption:?}"
    );
}

/// Pin a non-root-owned revision and delete its owner with no descendant
/// carrier: the pin is deleted with its session, so a fork of the revision
/// refuses the deleted session, and the root the owner forked from keeps its
/// own lineage whole.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn fork_lineage_no_carrier_law(handles: LineageConformanceHandles) {
    let factory = handles.factory;
    let injector = handles.injector;
    let run_id = SessionId::from("no-carrier-root");
    let owner_id = SessionId::from("no-carrier-owner");
    let (root, root_nodes) = seed(&factory, &run_id, 1).await;
    let owner = fork(
        &factory,
        "no-carrier-root",
        &owner_id,
        root_nodes.last().expect("no-carrier root leaf"),
    )
    .await;
    let owner_nodes = append(&owner, 1).await;
    let owner_leaf = owner_nodes.last().expect("no-carrier owner leaf").clone();
    let owner_revision =
        super::helpers::revision_at(factory.as_ref(), &owner_id, &owner_leaf).await;
    let pinned = crate::Target::Revision(owner_revision);
    factory
        .pin(&owner_id, &pinned)
        .await
        .expect("pin no-carrier owner leaf");
    factory
        .delete_session(&owner_id)
        .await
        .expect("delete no-carrier owner");

    let refused = factory
        .fork_session(&ForkSessionRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("no-carrier-recovered"),
            source_session_id: owner_id.clone(),
            head_revision: owner_revision,
            relation: SessionRelation::Root,
            config: crate::PersistedSessionConfig::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
                crate::SessionToolAccess::ambient(),
            ),
        })
        .await
        .expect_err("a deleted owner's pin holds nothing");
    assert!(
        matches!(
            &refused,
            StoreError::SessionDeleted { session_id } if *session_id == owner_id
        ),
        "a pin is deleted with its session: {refused:?}"
    );
    assert!(
        !node_readable(&root, &owner_leaf)
            .await
            .expect("read the deleted owner's node through the root"),
        "the deleted owner's node has no carrier"
    );

    let recovered = fork(
        &factory,
        "no-carrier-root",
        &SessionId::from("no-carrier-recovered"),
        root_nodes.last().expect("no-carrier root leaf"),
    )
    .await;
    let graph = recovered
        .load_session_window(WindowSelector::Current)
        .await
        .expect("load no-carrier fork")
        .expect("no-carrier fork exists")
        .window;
    assert_eq!(
        graph
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>(),
        [root_nodes[0].as_str(), root_nodes[1].as_str()],
        "the root's own revision is untouched by the owner's deletion"
    );
    assert_plan_matches_edge_walk(&injector, &SessionId::from("no-carrier-recovered")).await;
    assert_readability_equals_edge_reachability(
        &recovered,
        &injector,
        &SessionId::from("no-carrier-recovered"),
    )
    .await;
}

/// Independently reconstruct the expected per-owner maxima from raw edges and
/// compare them with the backend's installed ForkPlan.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn fork_plan_matches_edge_walk_law(handles: LineageConformanceHandles) {
    let factory = handles.factory;
    let injector = handles.injector;
    let (_root, root_nodes) = seed(&factory, &SessionId::from("plan-ground-truth-root"), 1).await;
    let middle = fork(
        &factory,
        "plan-ground-truth-root",
        &SessionId::from("plan-ground-truth-middle"),
        root_nodes.last().expect("ground-truth root leaf"),
    )
    .await;
    let middle_nodes = append(&middle, 2).await;
    let leaf = middle_nodes
        .last()
        .expect("ground-truth middle leaf")
        .clone();
    let _deep = fork(
        &factory,
        "plan-ground-truth-middle",
        &SessionId::from("plan-ground-truth-deep"),
        &leaf,
    )
    .await;
    assert_plan_matches_edge_walk(&injector, &SessionId::from("plan-ground-truth-deep")).await;
}
