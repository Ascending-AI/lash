//! A large candidate derived on demand, for the page laws' bounded-buffer
//! and retirement scale (FIG-6191).

use std::collections::BTreeSet;

use lash_core_store::checkpoint_graph::stage::{
    CandidateManifest, EdgeLog, ManifestEntry, ManifestSummary, MemoryEdgeLog, StageFault,
    node_range,
};
use lash_core_store::checkpoint_graph::{CommitId, EncodedNode, NodeId, NodeKind};

use super::encoded;

/// A candidate of `leaves` payload leaves under keyed leaves of `fanout`
/// children, one keyed index and a CheckpointRoot, derived on demand: the
/// stream keeps each node's digest and recipe, never a leaf's or keyed
/// leaf's bytes.
pub struct GeneratedManifest {
    tag: String,
    pub(super) summary: ManifestSummary,
    leaves: Vec<NodeId>,
    fanout: usize,
    keyed: EncodedNode,
    root: EncodedNode,
    order: Vec<(NodeId, Recipe)>,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Recipe {
    Leaf(u32),
    Index(u32),
    Keyed,
    Root,
}

impl GeneratedManifest {
    /// The candidate `tag` names, of `leaves` leaves.
    #[must_use]
    pub fn new(tag: &str, leaves: u32, fanout: usize, commit: CommitId) -> Self {
        let leaf_ids: Vec<NodeId> = (0..leaves).map(|leaf| leaf_page(tag, leaf).id).collect();
        let indexes: Vec<NodeId> = leaf_ids
            .chunks(fanout)
            .enumerate()
            .map(|(index, chunk)| {
                encoded(NodeKind::KEYED_LEAF, &format!("{tag} index {index}"), chunk).id
            })
            .collect();
        let keyed = encoded(NodeKind::KEYED_INDEX, &format!("{tag} keyed"), &indexes);
        let root = encoded(
            NodeKind::CHECKPOINT_ROOT,
            &format!("{tag} root"),
            &[keyed.id],
        );
        let mut order: Vec<(NodeId, Recipe)> = leaf_ids
            .iter()
            .enumerate()
            .map(|(leaf, id)| (*id, Recipe::Leaf(leaf as u32)))
            .chain(
                indexes
                    .iter()
                    .enumerate()
                    .map(|(index, id)| (*id, Recipe::Index(index as u32))),
            )
            .chain([(keyed.id, Recipe::Keyed), (root.id, Recipe::Root)])
            .collect();
        order.sort_by_key(|(id, _)| *id);
        let summary = ManifestSummary {
            root: root.id,
            base: None,
            commit,
            fresh_nodes: order.len() as u64,
            fresh_bytes: 0,
            fresh_edges: u64::from(leaves) + indexes.len() as u64 + 1,
            external: 0,
        };
        Self {
            tag: tag.to_owned(),
            summary,
            leaves: leaf_ids,
            fanout,
            keyed,
            root,
            order,
        }
    }

    /// Every node the candidate adds.
    #[must_use]
    pub fn nodes(&self) -> BTreeSet<NodeId> {
        self.order.iter().map(|(id, _)| *id).collect()
    }

    pub(super) fn build(&self, recipe: Recipe) -> EncodedNode {
        match recipe {
            Recipe::Leaf(leaf) => leaf_page(&self.tag, leaf),
            Recipe::Index(index) => {
                let start = index as usize * self.fanout;
                let end = (start + self.fanout).min(self.leaves.len());
                encoded(
                    NodeKind::KEYED_LEAF,
                    &format!("{} index {index}", self.tag),
                    &self.leaves[start..end],
                )
            }
            Recipe::Keyed => self.keyed.clone(),
            Recipe::Root => self.root.clone(),
        }
    }
}

fn leaf_page(tag: &str, leaf: u32) -> EncodedNode {
    encoded(NodeKind::PAYLOAD_LEAF, &format!("{tag} leaf {leaf}"), &[])
}

impl CandidateManifest for GeneratedManifest {
    fn summary(&self) -> ManifestSummary {
        self.summary
    }

    fn entries(
        &mut self,
        after: Option<NodeId>,
        limit: usize,
    ) -> Result<Vec<ManifestEntry>, StageFault> {
        let start = match after {
            None => 0,
            Some(after) => self.order.partition_point(|(id, _)| *id <= after),
        };
        Ok(self.order[start..]
            .iter()
            .take(limit)
            .map(|(_, recipe)| ManifestEntry::Fresh(self.build(*recipe)))
            .collect())
    }

    fn fresh_range(&mut self, id: &NodeId, offset: u64, len: usize) -> Result<Vec<u8>, StageFault> {
        let at = self
            .order
            .binary_search_by_key(id, |(id, _)| *id)
            .map_err(|_| StageFault::Missing(*id))?;
        node_range(&self.build(self.order[at].1), offset, len)
    }

    fn edge_log(&mut self) -> Result<Box<dyn EdgeLog>, StageFault> {
        Ok(Box::new(MemoryEdgeLog::default()))
    }
}
