//! Shared corrupt-durable-graph read conformance.

use std::future::Future;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphIntegrityCorruption {
    OrphanLeaf,
    DuplicateNodeId,
    DanglingLeafId,
    ParentCycle,
}

impl GraphIntegrityCorruption {
    fn label(self) -> &'static str {
        match self {
            Self::OrphanLeaf => "orphan-leaf",
            Self::DuplicateNodeId => "duplicate-node-id",
            Self::DanglingLeafId => "dangling-leaf-id",
            Self::ParentCycle => "parent-cycle",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphIntegrityRead {
    ActivePath,
    WholeGraph,
}

impl GraphIntegrityRead {
    fn label(self) -> &'static str {
        match self {
            Self::ActivePath => "active-path",
            Self::WholeGraph => "whole-graph",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphIntegrityTarget {
    pub session_id: String,
    pub root_node_id: String,
    pub leaf_node_id: String,
    pub missing_node_id: String,
    pub corruption: GraphIntegrityCorruption,
    pub read: GraphIntegrityRead,
}

#[async_trait::async_trait]
pub trait GraphIntegrityInjector: Send + Sync {
    async fn inject(&self, target: &GraphIntegrityTarget);

    async fn load_whole_graph(
        &self,
        session_id: &str,
    ) -> Result<crate::SessionGraph, crate::StoreError>;

    async fn cleanup(&self, _target: &GraphIntegrityTarget) {}
}

pub struct GraphIntegrityHandles {
    pub runtime: Arc<dyn crate::RuntimePersistence>,
    pub injector: Arc<dyn GraphIntegrityInjector>,
}

/// Drives every structural corruption class through both built-in graph read shapes.
pub async fn graph_integrity_conformance<Make, Fut>(make: Make)
where
    Make: Fn(&'static str) -> Fut,
    Fut: Future<Output = GraphIntegrityHandles>,
{
    for corruption in [
        GraphIntegrityCorruption::OrphanLeaf,
        GraphIntegrityCorruption::DuplicateNodeId,
        GraphIntegrityCorruption::DanglingLeafId,
        GraphIntegrityCorruption::ParentCycle,
    ] {
        for read in [
            GraphIntegrityRead::ActivePath,
            GraphIntegrityRead::WholeGraph,
        ] {
            let case = match (corruption, read) {
                (GraphIntegrityCorruption::OrphanLeaf, GraphIntegrityRead::ActivePath) => {
                    "orphan-leaf-active-path"
                }
                (GraphIntegrityCorruption::OrphanLeaf, GraphIntegrityRead::WholeGraph) => {
                    "orphan-leaf-whole-graph"
                }
                (GraphIntegrityCorruption::DuplicateNodeId, GraphIntegrityRead::ActivePath) => {
                    "duplicate-node-id-active-path"
                }
                (GraphIntegrityCorruption::DuplicateNodeId, GraphIntegrityRead::WholeGraph) => {
                    "duplicate-node-id-whole-graph"
                }
                (GraphIntegrityCorruption::DanglingLeafId, GraphIntegrityRead::ActivePath) => {
                    "dangling-leaf-id-active-path"
                }
                (GraphIntegrityCorruption::DanglingLeafId, GraphIntegrityRead::WholeGraph) => {
                    "dangling-leaf-id-whole-graph"
                }
                (GraphIntegrityCorruption::ParentCycle, GraphIntegrityRead::ActivePath) => {
                    "parent-cycle-active-path"
                }
                (GraphIntegrityCorruption::ParentCycle, GraphIntegrityRead::WholeGraph) => {
                    "parent-cycle-whole-graph"
                }
            };
            run_case(make(case).await, case, corruption, read).await;
        }
    }
}

async fn run_case(
    handles: GraphIntegrityHandles,
    case: &'static str,
    corruption: GraphIntegrityCorruption,
    read: GraphIntegrityRead,
) {
    let session_id = format!("graph-integrity-{case}");
    let mut state = crate::RuntimeSessionState {
        session_id: session_id.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    state
        .session_graph
        .append_plugin("graph-integrity", serde_json::json!({"case": case}));
    handles
        .runtime
        .admit_and_bind_session(&crate::SessionBinding::root(&session_id))
        .await
        .expect("admit graph-integrity session");
    handles
        .runtime
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("seed valid graph-integrity state");

    let healthy = match read {
        GraphIntegrityRead::ActivePath => {
            handles
                .runtime
                .load_session()
                .await
                .expect("load healthy active-path graph")
                .expect("healthy graph session exists")
                .graph
        }
        GraphIntegrityRead::WholeGraph => handles
            .injector
            .load_whole_graph(&session_id)
            .await
            .expect("load healthy whole graph"),
    };
    assert!(
        healthy.nodes.len() >= 2,
        "{case}: healthy fixture must exercise a non-trivial graph"
    );
    let target = GraphIntegrityTarget {
        session_id: session_id.clone(),
        root_node_id: healthy.nodes.first().expect("healthy root").node_id.clone(),
        leaf_node_id: healthy
            .leaf_node_id
            .clone()
            .expect("healthy graph has a resident leaf"),
        missing_node_id: format!("missing-{case}"),
        corruption,
        read,
    };

    handles.injector.inject(&target).await;
    let result = match read {
        GraphIntegrityRead::ActivePath => handles.runtime.load_session().await.map(|_| ()),
        GraphIntegrityRead::WholeGraph => handles
            .injector
            .load_whole_graph(&session_id)
            .await
            .map(|_| ()),
    };
    handles.injector.cleanup(&target).await;

    match result.expect_err("corrupt durable graph must be refused") {
        crate::StoreError::StoredDataCorrupt {
            record_kind,
            message,
        } => {
            assert_eq!(record_kind, "SessionGraph", "{case}");
            assert!(
                message.contains(&target.missing_node_id)
                    || message.contains(&target.leaf_node_id)
                    || message.contains(&target.root_node_id),
                "{} {} diagnostic must identify the corrupt graph row: {message}",
                corruption.label(),
                read.label(),
            );
        }
        other => panic!("{case}: expected StoredDataCorrupt for SessionGraph, got {other:?}"),
    }
}
