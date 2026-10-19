//! The built-in table's indexes: rows sorted by key once, then searched.
//! Lowering builds the table in every process, and one sort of each index
//! costs less than a search tree built row by row.

/// Rows sorted by their key, each key once.
#[derive(Debug)]
pub(crate) struct SortedMap<V> {
    rows: Vec<(&'static str, V)>,
}

impl<V> Default for SortedMap<V> {
    fn default() -> Self {
        Self { rows: Vec::new() }
    }
}

/// [`SortedMap::iter`]'s item.
type Row<'a, V> = (&'a &'static str, &'a V);

impl<V> SortedMap<V> {
    /// `rows` in key order.
    ///
    /// # Panics
    ///
    /// Two rows with one key: the built-in rows answer every question once.
    pub(super) fn new(mut rows: Vec<(&'static str, V)>, what: &str) -> Self {
        rows.sort_by(|a, b| a.0.cmp(b.0));
        if let Some(pair) = rows.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            panic!(
                "two built-in rows answer the same question: {what} `{}`",
                pair[0].0
            );
        }
        Self { rows }
    }

    /// Rows already sorted by key, each key once, as [`Self::new`] keeps them.
    pub(super) fn sorted(rows: Vec<(&'static str, V)>) -> Self {
        Self { rows }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&V> {
        self.rows
            .binary_search_by(|(row, _)| (*row).cmp(key))
            .ok()
            .map(|at| &self.rows[at].1)
    }

    pub(crate) fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = Row<'_, V>> {
        self.rows.iter().map(|(key, value)| (key, value))
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &&'static str> {
        self.rows.iter().map(|(key, _)| key)
    }

    /// The rows whose key is `key` or follows it.
    pub(crate) fn from(&self, key: &str) -> impl Iterator<Item = Row<'_, V>> {
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
pub(crate) struct SortedSet {
    names: Vec<&'static str>,
}

impl SortedSet {
    pub(super) fn new(mut names: Vec<&'static str>) -> Self {
        names.sort_unstable();
        names.dedup();
        Self { names }
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.names.binary_search(&name).is_ok()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub(crate) fn iter(&self) -> std::slice::Iter<'_, &'static str> {
        self.names.iter()
    }

    /// The names that are `name` or follow it.
    pub(crate) fn from(&self, name: &str) -> std::slice::Iter<'_, &'static str> {
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
