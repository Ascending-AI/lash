//! Graph fault injection vocabulary for backend test support.

use lash_kernel_value::identity::SessionId;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphIntegrityCorruption {
    OrphanLeaf,
    DuplicateNodeId,
    DanglingLeafId,
    ParentCycle,
}

impl GraphIntegrityCorruption {
    pub fn label(self) -> &'static str {
        match self {
            Self::OrphanLeaf => "orphan-leaf",
            Self::DuplicateNodeId => "duplicate-node-id",
            Self::DanglingLeafId => "dangling-leaf-id",
            Self::ParentCycle => "parent-cycle",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphIntegrityRead {
    ActivePath,
    WholeGraph,
}

impl GraphIntegrityRead {
    pub fn label(self) -> &'static str {
        match self {
            Self::ActivePath => "active-path",
            Self::WholeGraph => "whole-graph",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphIntegrityTarget {
    pub session_id: SessionId,
    pub root_node_id: lash_vocabulary::identity::NodeId,
    pub leaf_node_id: lash_vocabulary::identity::NodeId,
    pub missing_node_id: lash_vocabulary::identity::NodeId,
    pub corruption: GraphIntegrityCorruption,
    pub read: GraphIntegrityRead,
}
