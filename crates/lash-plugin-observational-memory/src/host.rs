use std::sync::Arc;

use lash_core::plugin::PluginError;
use lash_core::plugin::runtime_host::SessionGraphService;
use lash_core::{
    AppendSessionNodesRequest, AppendSessionNodesResult, DirectCompletion, DirectRequest,
    SessionAppendNode, SessionGraph,
};

pub(crate) struct OmRuntimeHost<'a> {
    session_id: &'a str,
    session_graph: &'a Arc<dyn SessionGraphService>,
    direct_completions: lash_core::DirectCompletionClient<'a>,
}

impl<'a> OmRuntimeHost<'a> {
    pub(crate) fn new(
        session_id: &'a str,
        session_graph: &'a Arc<dyn SessionGraphService>,
        direct_completions: lash_core::DirectCompletionClient<'a>,
    ) -> Self {
        Self {
            session_id,
            session_graph,
            direct_completions,
        }
    }

    pub(crate) fn session_id(&self) -> &str {
        self.session_id
    }

    pub(crate) async fn append_plugin_nodes(
        &self,
        graph: &SessionGraph,
        nodes: Vec<(String, serde_json::Value)>,
    ) -> Result<Option<SessionGraph>, PluginError> {
        append_plugin_nodes(self.session_id, self.session_graph, graph, nodes).await
    }

    pub(crate) async fn direct_completion(
        &self,
        request: DirectRequest,
        usage_source: &str,
    ) -> Result<DirectCompletion, PluginError> {
        self.direct_completions
            .direct_completion(request, usage_source)
            .await
    }
}

async fn append_plugin_nodes(
    session_id: &str,
    session_graph: &Arc<dyn SessionGraphService>,
    graph: &SessionGraph,
    nodes: Vec<(String, serde_json::Value)>,
) -> Result<Option<SessionGraph>, PluginError> {
    if nodes.is_empty() {
        return Ok(Some(graph.clone()));
    }
    let request_nodes = nodes
        .iter()
        .cloned()
        .map(|(plugin_type, body)| SessionAppendNode::plugin(plugin_type, body))
        .collect::<Vec<_>>();
    match session_graph
        .append_session_nodes(
            session_id,
            AppendSessionNodesRequest {
                operation_id: format!(
                    "observational-memory:{}:{}",
                    graph.leaf_node_id.as_deref().unwrap_or("root"),
                    request_nodes.len()
                ),
                nodes: request_nodes,
                requires_ancestor_node_id: graph.leaf_node_id.clone(),
            },
        )
        .await?
    {
        AppendSessionNodesResult::Appended { .. } => {
            let mut next = graph.clone();
            for (plugin_type, body) in nodes {
                next.append_plugin(plugin_type, body);
            }
            Ok(Some(next))
        }
        // Ruling: observational-memory maintenance is derived, best-effort
        // enrichment. A concurrent durable turn owns the newer branch; do not
        // graft an observation computed from the stale transcript onto it.
        // The next post-persist maintenance pass recomputes from durable graph
        // state, so dropping this stale derived append is intentional.
        AppendSessionNodesResult::StaleBranch { .. } => Ok(None),
    }
}
