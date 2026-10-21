//! Structural-integrity checks shared by graph construction and resident validation.
//!
//! Split from `session_graph` so the integrity rules read as one unit: identity uniqueness,
//! ancestry acyclicity from a resolvable leaf, and whole-graph parent topology.
//!
//! An anchored graph (a store-read window, ADR 0112 §5) admits exactly one
//! parent edge that leaves the graph: its base's edge to the anchor's
//! external parent. Every other dangling parent is corrupt, as it is for a
//! graph with no anchor.

use std::collections::{HashMap, HashSet};

use crate::session_graph::SessionGraph;
use lash_vocabulary::identity::NodeId;

/// The full structural-integrity check: unique ids, the anchor's base
/// resident, an acyclic ancestry from a resolvable leaf, and a parent
/// topology with no dangling edge but the anchor's. Answers the id index
/// and the leaf's ancestry, leaf first, for the graph's cache.
pub(crate) fn validate_structure(
    graph: &SessionGraph,
) -> Result<(HashMap<NodeId, usize>, Vec<usize>), crate::StoreError> {
    let by_id = graph_node_indices(graph)?;
    if let Some(anchor) = graph.anchor()
        && !by_id.contains_key(anchor.base_node_id().as_str())
    {
        return Err(crate::StoreError::InvalidWindowAnchor {
            frame_node_id: anchor.base_node_id().clone(),
            violation: crate::store::WindowAnchorViolation::BaseIsNotLeafFrame,
        });
    }
    let path = ancestry_indices(graph, &by_id, graph.leaf_node_id.as_deref())?;
    validate_graph_parent_topology(graph, &by_id)?;
    Ok((by_id, path))
}

pub fn graph_node_indices(
    graph: &SessionGraph,
) -> Result<HashMap<NodeId, usize>, crate::StoreError> {
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
    by_id: &HashMap<NodeId, usize>,
    node_id: Option<&str>,
) -> Result<Vec<usize>, crate::StoreError> {
    let mut path = Vec::new();
    let mut visited = HashSet::with_capacity(graph.nodes.len());
    let mut current = match node_id {
        Some(node_id) => Some(by_id.get(node_id).copied().ok_or_else(|| {
            crate::StoreError::InvalidGraphLeaf {
                leaf_node_id: NodeId::parse(node_id).ok(),
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
    by_id: &HashMap<NodeId, usize>,
) -> Result<(), crate::StoreError> {
    for node in &graph.nodes {
        if let Some(parent_node_id) = node.parent_node_id.as_ref()
            && !by_id.contains_key(parent_node_id)
            && !graph
                .anchor()
                .is_some_and(|anchor| anchor.admits_external_parent(node))
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
