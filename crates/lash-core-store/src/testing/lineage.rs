//! Raw lineage diagnostics for backend fixtures.
use crate::*;
use std::sync::Arc;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphFactObservation {
    pub node_id: crate::NodeId,
    pub parent_node_id: Option<crate::NodeId>,
    pub owning_session_id: SessionId,
    pub generation: u64,
    pub frame_node_id: crate::NodeId,
    pub is_frame: bool,
}

#[async_trait::async_trait]
pub trait LineageConformanceInjector: Send + Sync {
    async fn force_lineage(&self, session_id: &SessionId, ancestor_node_id: &str);
    async fn tombstone_node(&self, node_id: &str);
    async fn lineage_ancestors(
        &self,
        session_id: &SessionId,
    ) -> Vec<crate::store::ForkLineageAncestor>;
    async fn edge_path(&self, session_id: &SessionId) -> Vec<GraphFactObservation>;
    async fn all_graph_facts(&self) -> Vec<GraphFactObservation>;
}

pub struct LineageConformanceHandles<F: ?Sized> {
    pub factory: Arc<F>,
    pub injector: Arc<dyn LineageConformanceInjector>,
}

impl<F: ?Sized> Clone for LineageConformanceHandles<F> {
    fn clone(&self) -> Self {
        Self {
            factory: Arc::clone(&self.factory),
            injector: Arc::clone(&self.injector),
        }
    }
}
