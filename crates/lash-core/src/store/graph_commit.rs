use super::{GraphAppend, IncarnationId, OperationId, StoreError, derive_history_node_id};

impl GraphAppend {
    pub(crate) fn derive_node_ids(
        &mut self,
        session_id: &str,
        incarnation_id: &IncarnationId,
        operation: &OperationId,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let mut remapped = std::collections::HashMap::<String, String>::new();
        let mut mapping = Vec::with_capacity(self.nodes.len());
        for (ordinal, node) in self.nodes.iter_mut().enumerate() {
            if let Some(parent) = node.parent_node_id.as_mut()
                && let Some(derived_parent) = remapped.get(parent)
            {
                *parent = derived_parent.clone();
            }
            let old = node.node_id.clone();
            let derived = match &node.payload {
                crate::SessionNodePayload::FrameOpen { frame_key, .. } => {
                    crate::session_graph::frame_node_id(session_id, incarnation_id, frame_key)
                }
                _ => derive_history_node_id(incarnation_id, operation, ordinal as u64)?,
            };
            node.node_id = derived.clone();
            remapped.insert(old.clone(), derived.clone());
            mapping.push((old, derived));
        }
        if let Some(leaf) = self.leaf_node_id.as_mut()
            && let Some(derived_leaf) = remapped.get(leaf)
        {
            *leaf = derived_leaf.clone();
        }
        Ok(mapping)
    }

    pub fn appended_nodes(&self) -> impl Iterator<Item = &crate::SessionNodeRecord> {
        self.nodes.iter()
    }

    pub fn validate_append_topology(&self) -> Result<(), StoreError> {
        if let Some(last) = self.nodes.last()
            && self.leaf_node_id.as_deref() != Some(last.node_id.as_str())
        {
            return Err(StoreError::InvalidGraphLeaf {
                leaf_node_id: self.leaf_node_id.clone(),
            });
        }
        let proposed_ids = self
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let mut earlier_ids = std::collections::HashSet::with_capacity(self.nodes.len());
        for node in &self.nodes {
            if let Some(parent_node_id) = node.parent_node_id.as_deref()
                && proposed_ids.contains(parent_node_id)
                && !earlier_ids.contains(parent_node_id)
            {
                return Err(StoreError::InvalidGraphParent {
                    node_id: node.node_id.clone(),
                    expected: None,
                    actual: node.parent_node_id.clone(),
                });
            }
            earlier_ids.insert(node.node_id.as_str());
        }
        for pair in self.nodes.windows(2) {
            let expected = Some(pair[0].node_id.clone());
            if pair[1].parent_node_id != expected {
                return Err(StoreError::InvalidGraphParent {
                    node_id: pair[1].node_id.clone(),
                    expected,
                    actual: pair[1].parent_node_id.clone(),
                });
            }
        }
        Ok(())
    }
}
