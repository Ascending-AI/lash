//! Inline ordinary hash leaves, with a collision buffer only when needed.

/// Most hash leaves have one key. Keep that pair inline; allocate collision
/// storage only when two distinct keys have the same complete hash.
#[derive(Clone)]
pub(super) struct Pairs<K, V> {
    first: Option<(K, V)>,
    rest: Vec<(K, V)>,
}
impl<K, V> Pairs<K, V> {
    pub(super) fn one(key: K, value: V) -> Self {
        Self {
            first: Some((key, value)),
            rest: Vec::new(),
        }
    }
    pub(super) fn with_capacity(count: usize) -> Self {
        Self {
            first: None,
            rest: Vec::with_capacity(count.saturating_sub(1)),
        }
    }
    pub(super) fn iter(&self) -> impl Iterator<Item = &(K, V)> {
        self.first.iter().chain(self.rest.iter())
    }
    pub(super) fn iter_mut(&mut self) -> impl Iterator<Item = &mut (K, V)> {
        self.first.iter_mut().chain(self.rest.iter_mut())
    }
    pub(super) fn len(&self) -> usize {
        usize::from(self.first.is_some()) + self.rest.len()
    }
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.first.is_none() && self.rest.is_empty()
    }
    pub(super) fn capacity(&self) -> usize {
        1 + self.rest.capacity()
    }
    pub(super) fn push(&mut self, pair: (K, V)) {
        if self.first.is_none() {
            self.first = Some(pair);
        } else {
            self.rest.push(pair);
        }
    }
    pub(super) fn retain(&mut self, mut keep: impl FnMut(&(K, V)) -> bool) {
        if self.first.as_ref().is_some_and(|pair| !keep(pair)) {
            self.first = None;
        }
        self.rest.retain(keep);
    }
    pub(super) fn extend(&mut self, pairs: impl IntoIterator<Item = (K, V)>) {
        for pair in pairs {
            self.push(pair);
        }
    }
}
impl<K, V> std::ops::Index<usize> for Pairs<K, V> {
    type Output = (K, V);
    fn index(&self, index: usize) -> &Self::Output {
        match &self.first {
            Some(first) if index == 0 => first,
            first => &self.rest[index - usize::from(first.is_some())],
        }
    }
}
impl<K, V> std::ops::IndexMut<usize> for Pairs<K, V> {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        let has_first = self.first.is_some();
        if has_first && index == 0 {
            self.first.as_mut().expect("present first pair")
        } else {
            &mut self.rest[index - usize::from(has_first)]
        }
    }
}
