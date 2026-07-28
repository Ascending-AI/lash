use super::{GraphCommitDelta, IncarnationId, OperationId, StoreError, derive_history_node_id};

impl GraphCommitDelta {
    pub(crate) fn derive_node_ids(
        &mut self,
        session_id: &str,
        incarnation_id: &IncarnationId,
        operation: &OperationId,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let (nodes, leaf_node_id) = match self {
            Self::Append {
                nodes,
                leaf_node_id,
            } => (nodes, leaf_node_id),
            Self::Unchanged { .. } => return Ok(Vec::new()),
        };
        let mut remapped = std::collections::HashMap::<String, String>::new();
        let mut mapping = Vec::with_capacity(nodes.len());
        for (ordinal, node) in nodes.iter_mut().enumerate() {
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
        if let Some(leaf) = leaf_node_id.as_mut()
            && let Some(derived_leaf) = remapped.get(leaf)
        {
            *leaf = derived_leaf.clone();
        }
        Ok(mapping)
    }

    pub fn appended_nodes(&self) -> impl Iterator<Item = &crate::SessionNodeRecord> {
        match self {
            Self::Append { nodes, .. } => nodes.as_slice(),
            Self::Unchanged { .. } => &[],
        }
        .iter()
    }

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
