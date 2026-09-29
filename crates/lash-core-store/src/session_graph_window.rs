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

    /// Drop the durable nodes below the current frame and re-anchor the
    /// graph at the current `FrameOpen` (ADR 0112 §9). Returns the dropped
    /// ids, or `None` when nothing is dropped.
    ///
    /// The current frame is the last `FrameOpen` on the active path. The new
    /// anchor is derived from the old one (or, for a graph built in memory,
    /// from its root at generation 0): the generation plus the path offset,
    /// the `FrameOpen`'s parent, and the frame of that parent. A node is
    /// dropped only when it is in `durable` and is not the `FrameOpen` or a
    /// descendant of it. Nothing is dropped while the `FrameOpen` itself is
    /// pending, while a kept node would name a dropped parent, or when the
    /// parent's frame cannot be resolved from the graph.
    pub(crate) fn retire_below_current_frame(
        &mut self,
        durable: &HashSet<NodeId>,
    ) -> Option<Vec<NodeId>> {
        let path = crate::facade_support::SessionGraphFacadeOps::active_path_nodes(self);
        let frame_offset = path.iter().rposition(|node| node.frame_open().is_some())?;
        if frame_offset == 0 {
            return None;
        }
        let frame = path[frame_offset];
        if !durable.contains(&frame.node_id) {
            return None;
        }
        let root = path.first()?;
        let base_generation = match self.anchor() {
            Some(anchor) => anchor.generation,
            None if root.parent_node_id.is_none() => 0,
            None => return None,
        };
        let previous_offset = path[..frame_offset]
            .iter()
            .rposition(|node| node.frame_open().is_some())?;
        let previous_frame_node_id =
            crate::FrameNodeId::new(path[previous_offset].node_id.as_str()).ok()?;
        let frame_node_id = crate::FrameNodeId::new(frame.node_id.as_str()).ok()?;
        let external_parent = frame.parent_node_id.clone()?;
        let mut kept_ids = HashSet::new();
        kept_ids.insert(frame.node_id.as_str());
        for node in &path[frame_offset + 1..] {
            kept_ids.insert(node.node_id.as_str());
        }
        let below = path[..frame_offset]
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<HashSet<_>>();
        // Off-path nodes: a durable one below the frame goes, a pending one
        // stays, and so does anything that descends from a kept node.
        let mut dropped = Vec::new();
        for node in self.nodes.iter() {
            let id = node.node_id.as_str();
            if kept_ids.contains(id) {
                continue;
            }
            if below.contains(id) || durable.contains(&node.node_id) {
                if durable.contains(&node.node_id) {
                    dropped.push(node.node_id.clone());
                } else {
                    return None;
                }
            } else {
                kept_ids.insert(id);
            }
        }
        if dropped.is_empty() {
            return None;
        }
        let dropped_ids = dropped.iter().map(NodeId::as_str).collect::<HashSet<_>>();
        let kept = self
            .nodes
            .iter()
            .filter(|node| !dropped_ids.contains(node.node_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if kept.iter().any(|node| {
            node.node_id != frame.node_id
                && node
                    .parent_node_id
                    .as_deref()
                    .is_some_and(|parent| dropped_ids.contains(parent))
        }) {
            return None;
        }
        let anchor = WindowAnchor {
            frame_node_id,
            generation: base_generation + frame_offset as u64,
            external_parent: Some(external_parent),
            previous_frame_node_id: Some(previous_frame_node_id),
        };
        let leaf_node_id = self.leaf_node_id.clone();
        let rebuilt = Self::from_shared_anchored_nodes(kept, leaf_node_id, Some(anchor)).ok()?;
        *self = rebuilt;
        Some(dropped)
    }

    /// The window anchor of a store-read graph, `None` for a graph built in
    /// memory with no store.
    pub fn anchor(&self) -> Option<&WindowAnchor> {
        std::ops::Deref::deref(self).anchor.as_ref()
    }
}
