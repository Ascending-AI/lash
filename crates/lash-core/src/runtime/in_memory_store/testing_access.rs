//! Raw diagnostics exposed only to tests and the explicit `testing` feature.

use super::InMemorySessionStore;

impl InMemorySessionStore {
    /// This diagnostic accessor deliberately does not take `write_transaction`.
    /// It also holds `tombstoned_node_ids` while acquiring `session_graph`;
    /// callers must not use it concurrently with writes or introduce the
    /// inverse lock order.
    pub fn raw_graph_nodes_for_testing(&self) -> Vec<crate::SessionNodeRecord> {
        let tombstoned = self
            .tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes");
        self.global_session_graph
            .lock()
            .expect("lock global graph")
            .nodes
            .iter()
            .filter(|node| !tombstoned.contains(&node.node_id))
            .cloned()
            .collect()
    }

    /// Return the durable leaf-node id without loading a session read model.
    pub fn raw_leaf_node_id_for_testing(&self) -> Option<String> {
        self.session_head_meta
            .lock()
            .expect("lock store")
            .as_ref()
            .and_then(|meta| meta.leaf_node_id.clone())
    }

    /// Return the durable head revision without loading a session read model.
    pub fn raw_head_revision_for_testing(&self) -> Option<u64> {
        self.session_head_meta
            .lock()
            .expect("lock store")
            .as_ref()
            .map(|meta| meta.head_revision)
    }

    pub fn corrupt_node_refcount_for_testing(&self, node_id: &str, incoming_refs: i64) {
        self.incoming_node_refs
            .lock()
            .expect("lock incoming node refs")
            .insert(node_id.to_string(), incoming_refs);
    }

    /// Return raw pending-input lifecycle state for differential tests.
    pub fn raw_pending_turn_inputs_for_testing(
        &self,
    ) -> Vec<(String, crate::TurnInputState, Option<u64>)> {
        self.pending_turn_inputs
            .lock()
            .expect("lock pending turn inputs")
            .iter()
            .map(|entry| {
                (
                    entry.input.input_id.clone(),
                    entry.input.state,
                    entry
                        .claim_token
                        .as_ref()
                        .map(|_| entry.claim_session_lease_generation),
                )
            })
            .collect()
    }
}
