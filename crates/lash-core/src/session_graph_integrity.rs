//! Structural-integrity checks shared by graph construction and resident validation.
//!
//! Split from `session_graph` so the integrity rules read as one unit: identity uniqueness,
//! ancestry acyclicity from a resolvable leaf, and whole-graph parent topology.

use std::collections::{HashMap, HashSet};

use crate::session_graph::SessionGraph;

pub(crate) fn graph_node_indices(
    graph: &SessionGraph,
) -> Result<HashMap<String, usize>, crate::StoreError> {
    let mut by_id = HashMap::with_capacity(graph.nodes.len());
    for (idx, node) in graph.nodes.iter().enumerate() {
        if by_id.insert(node.node_id.clone(), idx).is_some() {
            return Err(crate::StoreError::NodeIdCollision {
                node_id: node.node_id.clone(),
            });
        }
    }
    Ok(by_id)
}

pub(crate) fn ancestry_indices(
    graph: &SessionGraph,
    by_id: &HashMap<String, usize>,
    node_id: Option<&str>,
) -> Result<Vec<usize>, crate::StoreError> {
    let mut path = Vec::new();
    let mut visited = HashSet::with_capacity(graph.nodes.len());
    let mut current = match node_id {
        Some(node_id) => Some(by_id.get(node_id).copied().ok_or_else(|| {
            crate::StoreError::InvalidGraphLeaf {
                leaf_node_id: Some(node_id.to_string()),
            }
        })?),
        None => None,
    };
    while let Some(idx) = current {
        let node = &graph.nodes[idx];
        if !visited.insert(idx) {
            return Err(crate::StoreError::InvalidGraphParent {
                node_id: node.node_id.clone(),
                expected: None,
                actual: node.parent_node_id.clone(),
            });
        }
        path.push(idx);
        current = node
            .parent_node_id
            .as_ref()
            .and_then(|parent| by_id.get(parent).copied());
    }
    Ok(path)
}

pub(crate) fn validate_graph_parent_topology(
    graph: &SessionGraph,
    by_id: &HashMap<String, usize>,
) -> Result<(), crate::StoreError> {
    for node in &graph.nodes {
        if let Some(parent_node_id) = node.parent_node_id.as_deref()
            && !by_id.contains_key(parent_node_id)
        {
            return Err(crate::StoreError::InvalidGraphParent {
                node_id: node.node_id.clone(),
                expected: None,
                actual: node.parent_node_id.clone(),
            });
        }
    }

    const UNVISITED: u8 = 0;
    const VISITING: u8 = 1;
    const RESOLVED: u8 = 2;
    let mut state = vec![UNVISITED; graph.nodes.len()];
    for start in 0..graph.nodes.len() {
        if state[start] != UNVISITED {
            continue;
        }
        let mut path = Vec::new();
        let mut current = Some(start);
        while let Some(idx) = current {
            if state[idx] == RESOLVED {
                break;
            }
            state[idx] = VISITING;
            path.push(idx);
            let node = &graph.nodes[idx];
            let parent = node
                .parent_node_id
                .as_ref()
                .and_then(|parent_node_id| by_id.get(parent_node_id).copied());
            if parent.is_some_and(|parent_idx| state[parent_idx] == VISITING) {
                return Err(crate::StoreError::InvalidGraphParent {
                    node_id: node.node_id.clone(),
                    expected: None,
                    actual: node.parent_node_id.clone(),
                });
            }
            current = parent;
        }
        for idx in path {
            state[idx] = RESOLVED;
        }
    }
    Ok(())
}
