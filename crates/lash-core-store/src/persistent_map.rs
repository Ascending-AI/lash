//! A persistent hash map: a clone shares every node with its source, and a
//! write copies only the path to the key it changes (FIG-6889).
//!
//! A resident session graph's indexes are cloned with every snapshot of the
//! graph, and the store's window and the runtimes that adopt it extend their
//! own copies independently. No write here allocates in proportion to the
//! entries already resident (FIG-5673), however many clones share them: a
//! write unshares at most one branch per trie level, and writes into a node
//! no other clone holds happen in place.

use std::borrow::Borrow;
use std::hash::{BuildHasher, Hash, RandomState};
use std::sync::Arc;

/// Five hash bits per level: a branch has at most 32 entries.
const BITS: u32 = 5;

pub(crate) struct PersistentMap<K, V> {
    root: Option<Arc<Branch<K, V>>>,
    len: usize,
    hasher: RandomState,
}

struct Branch<K, V> {
    /// Bit `i` set: slot `i` is present, at its rank among the set bits.
    bitmap: u32,
    entries: Vec<Entry<K, V>>,
}

enum Entry<K, V> {
    /// Every key whose hash is `hash`; more than one only on a collision.
    Leaf(Arc<Leaf<K, V>>),
    Branch(Arc<Branch<K, V>>),
}

#[derive(Clone)]
struct Leaf<K, V> {
    hash: u64,
    pairs: Vec<(K, V)>,
}

impl<K, V> Clone for Branch<K, V> {
    fn clone(&self) -> Self {
        Self {
            bitmap: self.bitmap,
            entries: self.entries.clone(),
        }
    }
}

impl<K, V> Clone for Entry<K, V> {
    fn clone(&self) -> Self {
        match self {
            Self::Leaf(leaf) => Self::Leaf(Arc::clone(leaf)),
            Self::Branch(branch) => Self::Branch(Arc::clone(branch)),
        }
    }
}

impl<K, V> Clone for PersistentMap<K, V> {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            len: self.len,
            hasher: self.hasher.clone(),
        }
    }
}

impl<K, V> Default for PersistentMap<K, V> {
    fn default() -> Self {
        Self {
            root: None,
            len: 0,
            hasher: RandomState::new(),
        }
    }
}

impl<K: std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for PersistentMap<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut map = f.debug_map();
        self.for_each(|key, value| {
            map.entry(key, value);
        });
        map.finish()
    }
}

fn slot(hash: u64, level: u32) -> u32 {
    let shift = level * BITS;
    // Past the hash's last bit every key of a leaf shares the hash, so a
    // deeper level is never reached; shifting by 64 would overflow.
    u32::try_from(hash.checked_shr(shift).unwrap_or(0) & ((1 << BITS) - 1)).unwrap_or(0)
}

impl<K, V> Branch<K, V> {
    fn rank(&self, slot: u32) -> usize {
        (self.bitmap & ((1 << slot) - 1)).count_ones() as usize
    }

    fn get(&self, slot: u32) -> Option<&Entry<K, V>> {
        (self.bitmap & (1 << slot) != 0).then(|| &self.entries[self.rank(slot)])
    }

    /// A branch at `level` holding two leaves of distinct hashes.
    fn pair(level: u32, first: Arc<Leaf<K, V>>, second: Arc<Leaf<K, V>>) -> Self {
        let (a, b) = (slot(first.hash, level), slot(second.hash, level));
        if a == b {
            return Self {
                bitmap: 1 << a,
                entries: vec![Entry::Branch(Arc::new(Self::pair(
                    level + 1,
                    first,
                    second,
                )))],
            };
        }
        let (low, high) = if a < b {
            (first, second)
        } else {
            (second, first)
        };
        Self {
            bitmap: (1 << a) | (1 << b),
            entries: vec![Entry::Leaf(low), Entry::Leaf(high)],
        }
    }

    fn find<Q>(&self, hash: u64, key: &Q, level: u32) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        match self.get(slot(hash, level))? {
            Entry::Leaf(leaf) if leaf.hash == hash => leaf
                .pairs
                .iter()
                .find(|(held, _)| held.borrow() == key)
                .map(|(_, value)| value),
            Entry::Leaf(_) => None,
            Entry::Branch(branch) => branch.find(hash, key, level + 1),
        }
    }

    fn for_each<'a>(&'a self, visit: &mut impl FnMut(&'a K, &'a V)) {
        for entry in &self.entries {
            match entry {
                Entry::Leaf(leaf) => {
                    for (key, value) in &leaf.pairs {
                        visit(key, value);
                    }
                }
                Entry::Branch(branch) => branch.for_each(visit),
            }
        }
    }
}

impl<K: Clone + Eq, V: Clone> Branch<K, V> {
    /// Writes `key`: `merge` folds `value` into a held value, else `value`
    /// is stored. Answers whether the key was absent.
    fn upsert(
        &mut self,
        hash: u64,
        key: K,
        value: V,
        merge: impl FnOnce(&mut V, V),
        level: u32,
    ) -> bool {
        let slot = slot(hash, level);
        let rank = self.rank(slot);
        if self.bitmap & (1 << slot) == 0 {
            self.bitmap |= 1 << slot;
            self.entries.insert(
                rank,
                Entry::Leaf(Arc::new(Leaf {
                    hash,
                    pairs: vec![(key, value)],
                })),
            );
            return true;
        }
        match &mut self.entries[rank] {
            Entry::Branch(branch) => {
                Arc::make_mut(branch).upsert(hash, key, value, merge, level + 1)
            }
            Entry::Leaf(leaf) if leaf.hash == hash => {
                let leaf = Arc::make_mut(leaf);
                if let Some((_, held)) = leaf.pairs.iter_mut().find(|(held, _)| *held == key) {
                    merge(held, value);
                    return false;
                }
                leaf.pairs.push((key, value));
                true
            }
            Entry::Leaf(leaf) => {
                let held = Arc::clone(leaf);
                let added = Arc::new(Leaf {
                    hash,
                    pairs: vec![(key, value)],
                });
                self.entries[rank] = Entry::Branch(Arc::new(Self::pair(level + 1, held, added)));
                true
            }
        }
    }

    /// Removes `key`, answering whether it was present. An emptied slot is
    /// cleared; a branch left with one entry stays a branch.
    fn remove<Q>(&mut self, hash: u64, key: &Q, level: u32) -> bool
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        let slot = slot(hash, level);
        if self.bitmap & (1 << slot) == 0 {
            return false;
        }
        let rank = self.rank(slot);
        let emptied = match &mut self.entries[rank] {
            Entry::Branch(branch) => {
                if branch.find(hash, key, level + 1).is_none() {
                    return false;
                }
                let branch = Arc::make_mut(branch);
                branch.remove(hash, key, level + 1);
                branch.bitmap == 0
            }
            Entry::Leaf(leaf) => {
                if leaf.hash != hash || !leaf.pairs.iter().any(|(held, _)| held.borrow() == key) {
                    return false;
                }
                let leaf = Arc::make_mut(leaf);
                leaf.pairs.retain(|(held, _)| held.borrow() != key);
                leaf.pairs.is_empty()
            }
        };
        if emptied {
            self.bitmap &= !(1 << slot);
            self.entries.remove(rank);
        }
        true
    }
}

impl<K, V> PersistentMap<K, V> {
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Visit every entry, in no particular order.
    pub(crate) fn for_each<'a>(&'a self, mut visit: impl FnMut(&'a K, &'a V)) {
        if let Some(root) = &self.root {
            root.for_each(&mut visit);
        }
    }
}

impl<K: Clone + Eq + Hash, V: Clone> PersistentMap<K, V> {
    pub(crate) fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let hash = self.hasher.hash_one(key);
        self.root.as_ref()?.find(hash, key, 0)
    }

    /// Stores `value` under `key`, answering whether the key was absent.
    pub(crate) fn insert(&mut self, key: K, value: V) -> bool {
        self.upsert(key, value, |held, value| *held = value)
    }

    /// Writes `key`: `merge` folds `value` into the held value, else
    /// `value` is stored. Answers whether the key was absent.
    pub(crate) fn upsert(&mut self, key: K, value: V, merge: impl FnOnce(&mut V, V)) -> bool {
        let hash = self.hasher.hash_one(&key);
        let root = Arc::make_mut(self.root.get_or_insert_with(|| {
            Arc::new(Branch {
                bitmap: 0,
                entries: Vec::new(),
            })
        }));
        let inserted = root.upsert(hash, key, value, merge, 0);
        if inserted {
            self.len += 1;
        }
        inserted
    }

    /// Removes `key`, answering whether it was present.
    pub(crate) fn remove<Q>(&mut self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let hash = self.hasher.hash_one(key);
        let Some(root) = self.root.as_mut() else {
            return false;
        };
        if root.find(hash, key, 0).is_none() {
            return false;
        }
        Arc::make_mut(root).remove(hash, key, 0);
        self.len -= 1;
        true
    }
}

impl<K: Clone + Eq + Hash, V: Clone> FromIterator<(K, V)> for PersistentMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(entries: I) -> Self {
        let mut map = Self::default();
        for (key, value) in entries {
            map.insert(key, value);
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// The map answers every lookup and length exactly as a plain map does,
    /// across inserts, merges and removals, and across clones written
    /// independently after they diverge: a write to one never shows in
    /// another.
    #[test]
    fn the_map_reads_as_a_plain_map_across_diverging_clones() {
        let mut state = 0x2545_f491_u64;
        let mut next = |bound: u64| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) % bound
        };
        type Pair = (PersistentMap<u64, Vec<u64>>, HashMap<u64, Vec<u64>>);
        let mut held: Vec<Pair> = vec![(PersistentMap::default(), HashMap::new())];
        for step in 0..8_000 {
            let which = usize::try_from(next(held.len() as u64)).unwrap_or(0);
            if next(16) == 0 {
                let copy = held[which].clone();
                held.push(copy);
            }
            let (map, model) = &mut held[which];
            let key = next(700);
            match next(4) {
                0 => assert_eq!(map.remove(&key), model.remove(&key).is_some()),
                1 => {
                    let absent = !model.contains_key(&key);
                    model.entry(key).or_default().push(step);
                    assert_eq!(
                        map.upsert(key, vec![step], |held, mut value| held.append(&mut value)),
                        absent
                    );
                }
                _ => {
                    assert_eq!(
                        map.insert(key, vec![step]),
                        model.insert(key, vec![step]).is_none()
                    );
                }
            }
        }
        for (map, model) in &held {
            assert_eq!(map.len(), model.len());
            for key in 0..700 {
                assert_eq!(map.get(&key), model.get(&key));
            }
            let mut visited = HashMap::new();
            map.for_each(|key, value| {
                visited.insert(*key, value.clone());
            });
            assert_eq!(visited, *model);
        }
    }
}
