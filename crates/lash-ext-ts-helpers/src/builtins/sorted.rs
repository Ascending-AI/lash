//! Borrowed indexes the build sorts and checks against the owning rows.

/// Rows sorted by their key, each key once.
#[derive(Debug)]
pub struct SortedMap<V: 'static> {
    rows: &'static [(&'static str, V)],
}

impl<V> Default for SortedMap<V> {
    fn default() -> Self {
        Self { rows: &[] }
    }
}

/// [`SortedMap::iter`]'s item.
type Row<'a, V> = (&'a &'static str, &'a V);

impl<V> SortedMap<V> {
    pub(super) const fn borrowed(rows: &'static [(&'static str, V)]) -> Self {
        Self { rows }
    }

    pub fn get(&self, key: &str) -> Option<&V> {
        self.rows
            .binary_search_by(|(row, _)| (*row).cmp(key))
            .ok()
            .map(|at| &self.rows[at].1)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = Row<'_, V>> {
        self.rows.iter().map(|(key, value)| (key, value))
    }

    pub fn keys(&self) -> impl Iterator<Item = &&'static str> {
        self.rows.iter().map(|(key, _)| key)
    }

    /// The rows whose key is `key` or follows it.
    pub fn from(&self, key: &str) -> impl Iterator<Item = Row<'_, V>> {
        let at = self.rows.partition_point(|(row, _)| *row < key);
        self.rows[at..].iter().map(|(key, value)| (key, value))
    }
}

impl<'a, V> IntoIterator for &'a SortedMap<V> {
    type Item = Row<'a, V>;
    type IntoIter = std::iter::Map<
        std::slice::Iter<'a, (&'static str, V)>,
        fn(&'a (&'static str, V)) -> Row<'a, V>,
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.iter().map(|(key, value)| (key, value))
    }
}

/// Names sorted once, each once.
#[derive(Debug, Default)]
pub struct SortedSet {
    names: &'static [&'static str],
}

impl SortedSet {
    pub(super) const fn borrowed(names: &'static [&'static str]) -> Self {
        Self { names }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.names.binary_search(&name).is_ok()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, &'static str> {
        self.names.iter()
    }

    /// The names that are `name` or follow it.
    pub fn from(&self, name: &str) -> std::slice::Iter<'_, &'static str> {
        self.names[self.names.partition_point(|row| *row < name)..].iter()
    }
}

impl<'a> IntoIterator for &'a SortedSet {
    type Item = &'a &'static str;
    type IntoIter = std::slice::Iter<'a, &'static str>;

    fn into_iter(self) -> Self::IntoIter {
        self.names.iter()
    }
}
