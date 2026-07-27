use super::{GraphCommitDelta, StoreError};

impl GraphCommitDelta {
    pub fn validate_append_topology(&self) -> Result<(), StoreError> {
        let Self::Append {
            nodes,
            leaf_node_id,
        } = self
        else {
            return Ok(());
        };
        if let Some(last) = nodes.last()
            && leaf_node_id.as_deref() != Some(last.node_id.as_str())
        {
            return Err(StoreError::InvalidGraphLeaf {
                leaf_node_id: leaf_node_id.clone(),
            });
        }
        let proposed_ids = nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let mut earlier_ids = std::collections::HashSet::with_capacity(nodes.len());
        for node in nodes {
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
        for pair in nodes.windows(2) {
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
