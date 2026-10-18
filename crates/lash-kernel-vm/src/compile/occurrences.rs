//! Layout-independent action indexes and their saved site keys.
use super::*;

impl Compiler<'_> {
    /// Assigns dense indexes from semantic sites, never from code or op order.
    /// Library calls remain uncounted; perform/sleep/spawn still count there.
    pub(super) fn index_occurrences(&mut self) -> (BTreeMap<Site, usize>, Box<[Vec<u8>]>) {
        let mut sites = BTreeSet::new();
        for stmt in &self.stmts {
            if let Form::Action(action) = &stmt.form
                && (!matches!(action.site.unit, Unit::Library(_))
                    || matches!(
                        action.kind,
                        ActionKind::Perform { .. }
                            | ActionKind::Sleep { .. }
                            | ActionKind::Spawn { .. }
                    ))
            {
                sites.insert(action.site.clone());
            }
        }
        for action in &self.stream.nested {
            if !matches!(action.kind, NestedKind::Body(_))
                && (!matches!(action.site.unit, Unit::Library(_))
                    || matches!(
                        action.kind,
                        NestedKind::Perform { .. }
                            | NestedKind::Sleep { .. }
                            | NestedKind::Spawn(_)
                    ))
            {
                sites.insert(action.site.clone());
            }
        }
        let sites: BTreeMap<_, _> = sites
            .into_iter()
            .map(|site| {
                (
                    lash_kernel_state::kernel_internals::metadata::metadata_key(&site),
                    site,
                )
            })
            .collect();
        let keys = sites.keys().cloned().collect();
        let at: BTreeMap<_, _> = sites
            .into_values()
            .enumerate()
            .map(|(index, site)| (site, self.base.occurrences + index))
            .collect();
        for stmt in &mut self.stmts {
            if let Form::Action(action) = &mut stmt.form {
                action.occurrence = at.get(&action.site).copied().unwrap_or(usize::MAX);
            }
        }
        for action in &mut self.stream.nested {
            action.occurrence = at.get(&action.site).copied().unwrap_or(usize::MAX);
        }
        (at, keys)
    }
}

impl Executable {
    pub(crate) fn occurrence_count(&self) -> usize {
        self.library.0.tables.occurrence_keys.len() + self.own.tables.occurrence_keys.len()
    }

    pub(crate) fn occurrence_index(&self, site: &Site) -> Option<usize> {
        if matches!(site.unit, Unit::Library(_)) {
            self.library.0.tables.occurrence_at.get(site).copied()
        } else {
            self.own.tables.occurrence_at.get(site).copied()
        }
    }

    /// Saved metadata keys can be inline tokens or a digest of a long site.
    /// Match the compiled key directly; neither spelling is decoded as a Site.
    pub(crate) fn saved_occurrence_index(&self, key: &[u8]) -> Option<usize> {
        let library = &self.library.0.tables.occurrence_keys;
        library
            .binary_search_by(|saved| saved.as_slice().cmp(key))
            .ok()
            .or_else(|| {
                self.own
                    .tables
                    .occurrence_keys
                    .binary_search_by(|saved| saved.as_slice().cmp(key))
                    .ok()
                    .map(|own| library.len() + own)
            })
    }

    pub(crate) fn occurrence_key(&self, index: usize) -> &[u8] {
        let library = &self.library.0.tables.occurrence_keys;
        match index.checked_sub(library.len()) {
            None => &library[index],
            Some(own) => &self.own.tables.occurrence_keys[own],
        }
    }
}
