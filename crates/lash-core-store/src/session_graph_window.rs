//! Store-read windows of a session graph (ADR 0112 §5): the anchor a window
//! carries and the rules a window must satisfy to be adopted.
use std::collections::HashSet;
use std::sync::Arc;

use crate::NodeId;
use crate::session_graph::{SessionGraph, SessionNodeRecord};

/// The base of a store-read window: the current frame's `FrameOpen`, which
/// is the one node of the graph allowed a parent outside it (ADR 0112 §5).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WindowAnchor {
    /// The window base: the current frame's `FrameOpen`.
    pub frame_node_id: crate::FrameNodeId,
    /// The base's stored generation.
    pub generation: u64,
    /// The base's parent, outside the window. `Some` iff `generation > 0`.
    pub external_parent: Option<NodeId>,
    /// `frame_node_id` of `external_parent`. `Some` iff `generation > 0`.
    pub previous_frame_node_id: Option<crate::FrameNodeId>,
}

impl WindowAnchor {
    /// The base's own node id.
    pub fn base_node_id(&self) -> &str {
        self.frame_node_id.as_str()
    }

    /// Whether `node` is the base and names exactly the anchor's external
    /// parent: the one dangling parent edge an anchored graph admits.
    pub(crate) fn admits_external_parent(&self, node: &SessionNodeRecord) -> bool {
        node.node_id.as_str() == self.base_node_id()
            && node.parent_node_id.is_some()
            && node.parent_node_id == self.external_parent
    }
}

impl SessionGraph {
    /// Build the graph of one store-read window (ADR 0112 §5): `nodes` in
    /// ascending generation, from the frame's `FrameOpen` to `leaf_node_id`.
    ///
    /// The window must be one anchored chain, so a store cannot hand the
    /// runtime an unanchored suffix:
    ///
    /// - the first node is a `FrameOpen` ([`BaseNotFrameOpen`](crate::store::WindowAnchorViolation::BaseNotFrameOpen))
    ///   whose id is `anchor.frame_node_id` ([`BaseIsNotLeafFrame`](crate::store::WindowAnchorViolation::BaseIsNotLeafFrame));
    /// - no later node opens a frame, since every window row points at the
    ///   base ([`ForeignFramePointer`](crate::store::WindowAnchorViolation::ForeignFramePointer));
    /// - the base's parent is `anchor.external_parent`, which with
    ///   `previous_frame_node_id` is `Some` exactly above generation 0
    ///   ([`ExternalParentShape`](crate::store::WindowAnchorViolation::ExternalParentShape));
    /// - every later node's parent is inside the window
    ///   ([`InnerParentOutsideWindow`](crate::store::WindowAnchorViolation::InnerParentOutsideWindow))
    ///   and is the node before it, and the last node is the leaf.
    ///
    /// An empty window is refused: a head with no leaf has no anchor, and a
    /// store answers it with an empty unanchored graph.
    pub fn from_window(
        nodes: Vec<SessionNodeRecord>,
        leaf_node_id: NodeId,
        anchor: WindowAnchor,
    ) -> Result<Self, crate::StoreError> {
        use crate::store::WindowAnchorViolation as Violation;
        let violation = |violation: Violation| crate::StoreError::InvalidWindowAnchor {
            frame_node_id: NodeId::from(anchor.base_node_id()),
            violation,
        };
        let Some(base) = nodes.first() else {
            return Err(violation(Violation::BaseIsNotLeafFrame));
        };
        if base.frame_open().is_none() {
            return Err(violation(Violation::BaseNotFrameOpen));
        }
        if base.node_id.as_str() != anchor.base_node_id() {
            return Err(violation(Violation::BaseIsNotLeafFrame));
        }
        let above_root = anchor.generation > 0;
        if above_root != anchor.external_parent.is_some()
            || above_root != anchor.previous_frame_node_id.is_some()
            || base.parent_node_id != anchor.external_parent
        {
            return Err(violation(Violation::ExternalParentShape));
        }
        let mut in_window = HashSet::with_capacity(nodes.len());
        in_window.insert(base.node_id.as_str());
        for pair in nodes.windows(2) {
            let (previous, node) = (&pair[0], &pair[1]);
            if node.frame_open().is_some() {
                return Err(violation(Violation::ForeignFramePointer));
            }
            match node.parent_node_id.as_deref() {
                Some(parent) if in_window.contains(parent) => {
                    if parent != previous.node_id.as_str() {
                        return Err(crate::StoreError::InvalidGraphParent {
                            node_id: node.node_id.clone(),
                            expected: Some(previous.node_id.clone()),
                            actual: node.parent_node_id.clone(),
                        });
                    }
                }
                _ => return Err(violation(Violation::InnerParentOutsideWindow)),
            }
            in_window.insert(node.node_id.as_str());
        }
        if nodes.last().map(|node| &node.node_id) != Some(&leaf_node_id) {
            return Err(crate::StoreError::InvalidGraphLeaf {
                leaf_node_id: Some(leaf_node_id),
            });
        }
        Self::from_shared_anchored_nodes(
            nodes.into_iter().map(Arc::new).collect(),
            Some(leaf_node_id),
            Some(anchor),
        )
    }

    /// The window anchor of a store-read graph, `None` for a graph built in
    /// memory with no store.
    pub fn anchor(&self) -> Option<&WindowAnchor> {
        std::ops::Deref::deref(self).anchor.as_ref()
    }
}
