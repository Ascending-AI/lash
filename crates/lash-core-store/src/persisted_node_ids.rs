use std::hash::{Hash, Hasher};

use crate::NodeId;
use crate::persistent_map::PersistentMap;

/// The node ids a session knows exist durably.
///
/// Shared by every clone of the state that holds it: a runtime clones its
/// state several times a turn, and each clone used to copy one id per durable
/// node of the frame (FIG-4060). The set is persistent (FIG-6889): a clone
/// shares it, and a write copies only the path to the id it changes, so a
/// state adopting a window takes the id set the window's graph keeps
/// without walking it. The order-independent digest the observation
/// fingerprint folds in is kept in step with every write, so no publish
/// walks the set.
#[derive(Clone, Debug, Default)]
pub struct PersistedNodeIds {
    ids: PersistentMap<NodeId, ()>,
    digest: u64,
}

impl PersistedNodeIds {
    /// Adds `node_id`, answering whether it was absent.
    pub fn insert(&mut self, node_id: NodeId) -> bool {
        let hash = id_hash(&node_id);
        let inserted = self.ids.insert(node_id, ());
        if inserted {
            self.digest = self.digest.wrapping_add(hash);
        }
        inserted
    }

    /// Removes `node_id`, answering whether it was present.
    pub fn remove(&mut self, node_id: &NodeId) -> bool {
        if !self.ids.remove(node_id.as_str()) {
            return false;
        }
        self.digest = self.digest.wrapping_sub(id_hash(node_id));
        true
    }

    /// Whether `node_id` is in the set.
    pub fn contains(&self, node_id: &str) -> bool {
        self.ids.get(node_id).is_some()
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.len() == 0
    }

    /// Every id, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = &NodeId> {
        let mut ids = Vec::with_capacity(self.ids.len());
        self.ids.for_each(|id, ()| ids.push(id));
        ids.into_iter()
    }

    /// A digest of the set that does not depend on insertion order.
    pub fn digest(&self) -> u64 {
        self.digest
    }
}

fn id_hash(node_id: &NodeId) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    node_id.as_str().hash(&mut hasher);
    hasher.finish()
}

impl Extend<NodeId> for PersistedNodeIds {
    fn extend<I: IntoIterator<Item = NodeId>>(&mut self, node_ids: I) {
        for node_id in node_ids {
            self.insert(node_id);
        }
    }
}

impl FromIterator<NodeId> for PersistedNodeIds {
    fn from_iter<I: IntoIterator<Item = NodeId>>(node_ids: I) -> Self {
        let mut persisted = Self::default();
        persisted.extend(node_ids);
        persisted
    }
}

impl PartialEq for PersistedNodeIds {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self.digest == other.digest
            && self.iter().all(|id| other.contains(id))
    }
}

impl Eq for PersistedNodeIds {}
