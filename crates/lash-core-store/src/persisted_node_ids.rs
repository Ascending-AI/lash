use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::NodeId;

/// The node ids a session knows exist durably.
///
/// Shared by every clone of the state that holds it: a runtime clones its
/// state several times a turn, and each clone used to copy one id per durable
/// node of the frame (FIG-4060). Cloning is now a pointer bump, and the first
/// write after a clone copies the set once. The order-independent digest the
/// observation fingerprint folds in is kept in step with every write, so no
/// publish walks the set.
#[derive(Clone, Debug, Default)]
pub struct PersistedNodeIds {
    ids: Arc<HashSet<NodeId>>,
    digest: u64,
}

impl PersistedNodeIds {
    /// Adds `node_id`, answering whether it was absent.
    pub fn insert(&mut self, node_id: NodeId) -> bool {
        let hash = id_hash(&node_id);
        let inserted = Arc::make_mut(&mut self.ids).insert(node_id);
        if inserted {
            self.digest = self.digest.wrapping_add(hash);
        }
        inserted
    }

    /// Removes `node_id`, answering whether it was present.
    pub fn remove(&mut self, node_id: &NodeId) -> bool {
        if !self.ids.contains(node_id) {
            return false;
        }
        Arc::make_mut(&mut self.ids).remove(node_id);
        self.digest = self.digest.wrapping_sub(id_hash(node_id));
        true
    }

    pub fn clear(&mut self) {
        *self = Self::default();
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

impl std::ops::Deref for PersistedNodeIds {
    type Target = HashSet<NodeId>;

    fn deref(&self) -> &HashSet<NodeId> {
        &self.ids
    }
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
        self.ids == other.ids
    }
}

impl Eq for PersistedNodeIds {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digest_follows_the_set_in_any_order_and_clones_share_until_written() {
        let mut forward = ["a", "b", "c"]
            .into_iter()
            .map(NodeId::from)
            .collect::<PersistedNodeIds>();
        let backward = ["c", "b", "a"]
            .into_iter()
            .map(NodeId::from)
            .collect::<PersistedNodeIds>();
        assert_eq!(forward.digest(), backward.digest());
        assert!(
            !forward.insert(NodeId::from("a")),
            "a repeat changes nothing"
        );
        assert_eq!(forward.digest(), backward.digest());

        let held = forward.clone();
        assert!(Arc::ptr_eq(&held.ids, &forward.ids));
        assert!(forward.remove(&NodeId::from("b")));
        assert!(!forward.remove(&NodeId::from("b")));
        assert!(
            held.contains(&NodeId::from("b")),
            "a held clone keeps its set"
        );
        let without_b = ["a", "c"]
            .into_iter()
            .map(NodeId::from)
            .collect::<PersistedNodeIds>();
        assert_eq!(forward.digest(), without_b.digest());
        assert_eq!(forward, without_b);
    }
}
