//! A keyed index whose snapshots share one store of writes.
//!
//! A snapshot sees the writes preceding its length. Appends at the shared
//! tip add one write without copying entries held by older readers. A
//! writer branching from an older snapshot gets a private index of that
//! prefix; ordinary commits always advance the tip (FIG-4060).
//!
//! No write allocates in proportion to the writes already resident
//! (FIG-5673): keys live in a binary trie over their hashes, where a new
//! key adds one leaf and at most one branch, and the write log is a linked
//! list. A growable table or vector would instead reallocate every
//! resident entry whenever an append found it full.

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::hash::{BuildHasher, Hash, RandomState};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

#[derive(Debug)]
pub(crate) struct SnapshotIndex<K, V> {
    data: Arc<StdMutex<IndexWrites<K, V>>>,
    len: usize,
}

#[derive(Debug)]
struct IndexWrites<K, V> {
    hasher: RandomState,
    root: Node<K, V>,
    /// The key hash of every write, newest first.
    log: Option<Box<LogEntry>>,
    /// How many writes the store holds; a write's position is the length
    /// it was pushed at.
    len: usize,
    holders: BTreeMap<usize, usize>,
}

#[derive(Debug)]
struct LogEntry {
    hash: u64,
    older: Option<Box<LogEntry>>,
}

/// A crit-bit trie over key hashes: a branch tests the most significant
/// bit its two subtrees differ in, and bits grow less significant toward
/// the leaves.
#[derive(Debug)]
enum Node<K, V> {
    Empty,
    Leaf(Box<Leaf<K, V>>),
    Branch(Box<Branch<K, V>>),
}

#[derive(Debug)]
struct Branch<K, V> {
    bit: u32,
    children: [Node<K, V>; 2],
}

#[derive(Debug)]
struct Leaf<K, V> {
    hash: u64,
    key: K,
    /// The key's first write, inline: most keys are written once.
    first: (usize, V),
    /// Its later writes, by ascending position.
    later: Vec<(usize, V)>,
    /// Another key with the same hash.
    collision: Option<Box<Leaf<K, V>>>,
}

fn side(hash: u64, bit: u32) -> usize {
    usize::from(hash & (1 << (63 - bit)) != 0)
}

impl<K, V> Leaf<K, V> {
    fn newest_position(&self) -> usize {
        self.later
            .last()
            .map_or(self.first.0, |(position, _)| *position)
    }

    fn visible(&self, len: usize) -> impl Iterator<Item = &V> {
        std::iter::once(&self.first)
            .chain(&self.later)
            .take_while(move |(position, _)| *position < len)
            .map(|(_, value)| value)
    }

    fn latest_visible(&self, len: usize) -> Option<&V> {
        match self
            .later
            .partition_point(|(position, _)| *position < len)
            .checked_sub(1)
        {
            Some(ordinal) => Some(&self.later[ordinal].1),
            None => (self.first.0 < len).then_some(&self.first.1),
        }
    }
}

impl<K, V> Node<K, V> {
    /// The child a lookup of `hash` follows, or the node itself at a leaf.
    fn step(&mut self, hash: u64) -> &mut Self {
        match self {
            Self::Branch(branch) => &mut branch.children[side(hash, branch.bit)],
            node => node,
        }
    }

    /// The leaves sharing `hash`'s path, and how many branches lead there.
    fn nearest(&self, hash: u64) -> (Option<&Leaf<K, V>>, usize) {
        let mut node = self;
        let mut depth = 0;
        loop {
            match node {
                Self::Empty => return (None, depth),
                Self::Leaf(leaf) => return (Some(leaf), depth),
                Self::Branch(branch) => {
                    node = &branch.children[side(hash, branch.bit)];
                    depth += 1;
                }
            }
        }
    }

    fn nearest_mut(&mut self, hash: u64) -> Option<&mut Leaf<K, V>> {
        let mut node = self;
        loop {
            match node {
                Self::Empty => return None,
                Self::Leaf(leaf) => return Some(leaf),
                Self::Branch(branch) => node = &mut branch.children[side(hash, branch.bit)],
            }
        }
    }
}

impl<K, V> IndexWrites<K, V> {
    fn new(hasher: RandomState) -> Self {
        Self {
            hasher,
            root: Node::Empty,
            log: None,
            len: 0,
            holders: BTreeMap::new(),
        }
    }

    fn unregister(&mut self, len: usize) {
        if let Some(count) = self.holders.get_mut(&len) {
            *count -= 1;
            if *count == 0 {
                self.holders.remove(&len);
            }
        }
    }

    fn leaf<Q: Eq + ?Sized>(&self, hash: u64, key: &Q) -> Option<&Leaf<K, V>>
    where
        K: Borrow<Q>,
    {
        let mut leaf = self.root.nearest(hash).0.filter(|leaf| leaf.hash == hash);
        while let Some(candidate) = leaf {
            let candidate_key: &Q = candidate.key.borrow();
            if candidate_key == key {
                return Some(candidate);
            }
            leaf = candidate.collision.as_deref();
        }
        None
    }

    fn push(&mut self, key: K, value: V)
    where
        K: Eq + Hash,
    {
        let hash = self.hasher.hash_one(&key);
        let position = self.len;
        self.log = Some(Box::new(LogEntry {
            hash,
            older: self.log.take(),
        }));
        self.len += 1;

        let (nearest, _) = self.root.nearest(hash);
        let nearest_hash = nearest.map(|leaf| leaf.hash);
        if nearest_hash == Some(hash) {
            let mut leaf = self.root.nearest_mut(hash);
            while let Some(candidate) = leaf {
                if candidate.key == key {
                    candidate.later.push((position, value));
                    return;
                }
                leaf = candidate.collision.as_deref_mut();
            }
        }
        let mut leaf = Box::new(Leaf {
            hash,
            key,
            first: (position, value),
            later: Vec::new(),
            collision: None,
        });
        let Some(nearest_hash) = nearest_hash else {
            self.root = Node::Leaf(leaf);
            return;
        };
        if nearest_hash == hash {
            if let Some(head) = self.root.nearest_mut(hash) {
                leaf.collision = head.collision.take();
                head.collision = Some(leaf);
            }
            return;
        }
        // The new branch sits above every branch testing a less
        // significant bit than the one the new key first differs in.
        let bit = (nearest_hash ^ hash).leading_zeros();
        let mut slot = &mut self.root;
        while matches!(&*slot, Node::Branch(branch) if branch.bit < bit) {
            slot = slot.step(hash);
        }
        let displaced = std::mem::replace(slot, Node::Empty);
        let leaf = Node::Leaf(leaf);
        *slot = Node::Branch(Box::new(Branch {
            bit,
            children: if side(hash, bit) == 0 {
                [leaf, displaced]
            } else {
                [displaced, leaf]
            },
        }));
    }

    // A discarded speculative writer must not force the next writer to copy
    // the resident prefix. Only writes no remaining snapshot can see retire.
    fn truncate(&mut self, len: usize) {
        while self.len > len {
            let Some(entry) = self.log.take() else {
                return;
            };
            self.log = entry.older;
            self.len -= 1;
            self.retire(entry.hash, self.len);
        }
    }

    /// Removes the write at `position`, the newest write of its key.
    fn retire(&mut self, hash: u64, position: usize) {
        let (_, depth) = self.root.nearest(hash);
        let Some(head) = self.root.nearest_mut(hash) else {
            return;
        };
        if head.newest_position() != position {
            let mut link = &mut head.collision;
            while link
                .as_ref()
                .is_some_and(|leaf| leaf.newest_position() != position)
            {
                let Some(leaf) = link else {
                    return;
                };
                link = &mut leaf.collision;
            }
            if let Some(leaf) = link
                && leaf.later.pop().is_none()
            {
                *link = leaf.collision.take();
            }
            return;
        }
        if head.later.pop().is_some() {
            return;
        }
        if let Some(next) = head.collision.take() {
            *head = *next;
            return;
        }
        // The key's only leaf: its sibling takes the parent's place.
        let mut slot = &mut self.root;
        for _ in 1..depth {
            slot = slot.step(hash);
        }
        *slot = match std::mem::replace(slot, Node::Empty) {
            Node::Branch(branch) if depth > 0 => {
                let [zero, one] = branch.children;
                if side(hash, branch.bit) == 0 {
                    one
                } else {
                    zero
                }
            }
            _ => Node::Empty,
        };
    }
}

impl<K: Clone, V: Clone> IndexWrites<K, V> {
    /// A private store of the first `len` writes, at their positions.
    fn prefix(&self, len: usize) -> Self {
        let mut hashes = Vec::with_capacity(len);
        let mut entry = self.log.as_deref();
        for _ in len..self.len {
            entry = entry.and_then(|entry| entry.older.as_deref());
        }
        while let Some(visible) = entry {
            hashes.push(visible.hash);
            entry = visible.older.as_deref();
        }
        let mut prefix = Self::new(self.hasher.clone());
        for hash in hashes.into_iter().rev() {
            prefix.log = Some(Box::new(LogEntry {
                hash,
                older: prefix.log.take(),
            }));
        }
        prefix.len = len;
        prefix.root = Self::visible_subtree(&self.root, len);
        prefix
    }

    fn visible_subtree(node: &Node<K, V>, len: usize) -> Node<K, V> {
        match node {
            Node::Empty => Node::Empty,
            Node::Leaf(leaf) => {
                let mut visible = None;
                let mut source = Some(leaf.as_ref());
                while let Some(leaf) = source {
                    if leaf.first.0 < len {
                        visible = Some(Box::new(Leaf {
                            hash: leaf.hash,
                            key: leaf.key.clone(),
                            first: leaf.first.clone(),
                            later: leaf
                                .later
                                .iter()
                                .take_while(|(position, _)| *position < len)
                                .cloned()
                                .collect(),
                            collision: visible,
                        }));
                    }
                    source = leaf.collision.as_deref();
                }
                visible.map_or(Node::Empty, Node::Leaf)
            }
            Node::Branch(branch) => {
                let zero = Self::visible_subtree(&branch.children[0], len);
                let one = Self::visible_subtree(&branch.children[1], len);
                match (zero, one) {
                    (Node::Empty, only) | (only, Node::Empty) => only,
                    (zero, one) => Node::Branch(Box::new(Branch {
                        bit: branch.bit,
                        children: [zero, one],
                    })),
                }
            }
        }
    }
}

impl<K, V> Drop for IndexWrites<K, V> {
    // The log is as long as the index's history; unlinking it here keeps
    // its drop from recursing once per write.
    fn drop(&mut self) {
        let mut entry = self.log.take();
        while let Some(mut newest) = entry {
            entry = newest.older.take();
        }
    }
}

impl<K, V> SnapshotIndex<K, V> {
    fn writes(&self) -> MutexGuard<'_, IndexWrites<K, V>> {
        self.data
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether both snapshots read one store: no writer between them
    /// branched off a private copy.
    #[cfg(test)]
    pub(crate) fn shares_writes_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.data, &other.data)
    }
}

impl<K: Eq + Hash + Clone, V: Clone> SnapshotIndex<K, V> {
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = (K, V)>) -> Self {
        let mut data = IndexWrites::new(RandomState::new());
        for (key, value) in entries {
            data.push(key, value);
        }
        let len = data.len;
        data.holders.insert(len, 1);
        Self {
            len,
            data: Arc::new(StdMutex::new(data)),
        }
    }

    /// The newest value this snapshot sees written for `key`.
    pub(crate) fn get<Q: Eq + Hash + ?Sized>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        let data = self.writes();
        let hash = data.hasher.hash_one(key);
        data.leaf(hash, key)?.latest_visible(self.len).cloned()
    }

    /// Every value this snapshot sees written for `key`, oldest first.
    pub(crate) fn all<Q: Eq + Hash + ?Sized>(&self, key: &Q) -> Vec<V>
    where
        K: Borrow<Q>,
    {
        let data = self.writes();
        let hash = data.hasher.hash_one(key);
        data.leaf(hash, key)
            .map(|leaf| leaf.visible(self.len).cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) fn insert(&mut self, key: K, value: V) {
        let mut data = self.writes();
        let visible_tip = data.holders.last_key_value().map_or(0, |(len, _)| *len);
        data.truncate(visible_tip);
        if self.len < data.len {
            let mut private = data.prefix(self.len);
            data.unregister(self.len);
            drop(data);
            private.holders.insert(self.len, 1);
            self.data = Arc::new(StdMutex::new(private));
            data = self.writes();
        }
        data.push(key, value);
        data.unregister(self.len);
        *data.holders.entry(self.len + 1).or_default() += 1;
        drop(data);
        self.len += 1;
    }
}

impl<K, V> Clone for SnapshotIndex<K, V> {
    fn clone(&self) -> Self {
        *self.writes().holders.entry(self.len).or_default() += 1;
        Self {
            data: Arc::clone(&self.data),
            len: self.len,
        }
    }
}

impl<K, V> Drop for SnapshotIndex<K, V> {
    fn drop(&mut self) {
        let len = self.len;
        self.writes().unregister(len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const KEYS: usize = 96;

    fn assert_reads(index: &SnapshotIndex<usize, usize>, writes: &[(usize, usize)]) {
        let mut expected = HashMap::<usize, Vec<usize>>::new();
        for (key, value) in &writes[..index.len] {
            expected.entry(*key).or_default().push(*value);
        }
        for key in 0..KEYS {
            let values = expected.remove(&key).unwrap_or_default();
            assert_eq!(index.all(&key), values, "key {key} at {}", index.len);
            assert_eq!(index.get(&key), values.last().copied());
        }
    }

    #[test]
    fn a_snapshot_reads_exactly_the_writes_before_its_length() {
        // A fixed linear congruential sequence: rewrites, abandoned
        // speculative writers and branches from older snapshots interleave.
        let mut state = 0x9e37_79b9_usize;
        let mut next = |bound: usize| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223) & 0xffff_ffff;
            (state >> 8) % bound
        };
        let mut index = SnapshotIndex::<usize, usize>::from_entries([]);
        let mut writes = Vec::new();
        let mut held = Vec::new();
        for step in 0..4_000 {
            match next(8) {
                0 => {
                    let mut speculative = index.clone();
                    for _ in 0..next(4) {
                        speculative.insert(next(KEYS), usize::MAX);
                    }
                }
                1 => held.push((index.clone(), writes.clone())),
                2 if !held.is_empty() => {
                    let branch = next(held.len());
                    (index, writes) = held.swap_remove(branch);
                }
                _ => {}
            }
            let key = next(KEYS);
            index.insert(key, step);
            writes.push((key, step));
            if step % 64 == 0 {
                assert_reads(&index, &writes);
                for (snapshot, snapshot_writes) in &held {
                    assert_reads(snapshot, snapshot_writes);
                }
            }
        }
        assert_reads(&index, &writes);
        for (snapshot, snapshot_writes) in &held {
            assert_reads(snapshot, snapshot_writes);
        }
    }
}
