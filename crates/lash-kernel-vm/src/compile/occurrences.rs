//! Layout-independent action indexes and their saved site keys.
use super::*;

type OccurrenceIndex = (BTreeMap<Site, usize>, Box<[Vec<u8>]>, Box<[Unit]>);

impl Compiler<'_> {
    /// Assigns dense indexes from semantic sites, never from code or op order.
    /// Library calls remain uncounted; perform/sleep/spawn still count there.
    pub(super) fn index_occurrences(&mut self) -> OccurrenceIndex {
        let mut sites = BTreeSet::new();
        for stmt in &self.stmts {
            if let Form::Action(action) = &stmt.form
                && (action.guest
                    || matches!(
                        action.kind,
                        ActionKind::Perform { .. } | ActionKind::Spawn { .. }
                    ))
            {
                sites.insert(action.site.clone());
            }
        }
        for action in &self.stream.nested {
            if !matches!(action.kind, NestedKind::Body(_))
                && (action.guest
                    || matches!(
                        action.kind,
                        NestedKind::Perform { .. } | NestedKind::Spawn(_)
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
        let units = sites.values().map(|site| site.unit.clone()).collect();
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
        (at, keys, units)
    }
}

impl Executable {
    pub(crate) fn occurrence_count(&self) -> usize {
        self.library.0.tables.occurrence_keys.len() + self.own.tables.occurrence_keys.len()
    }

    pub(crate) fn occurrence_index(&self, site: &Site) -> Option<usize> {
        self.own
            .tables
            .occurrence_at
            .get(site)
            .copied()
            .or_else(|| self.library.0.tables.occurrence_at.get(site).copied())
    }

    /// Saved metadata keys can be inline tokens or a digest of a long site.
    /// Match the compiled key directly; neither spelling is decoded as a Site.
    pub(crate) fn saved_occurrence_index(&self, key: &[u8]) -> Option<usize> {
        self.own
            .tables
            .occurrence_keys
            .binary_search_by(|saved| saved.as_slice().cmp(key))
            .ok()
            .map(|own| self.library.0.tables.occurrence_keys.len() + own)
            .or_else(|| {
                self.library
                    .0
                    .tables
                    .occurrence_keys
                    .binary_search_by(|saved| saved.as_slice().cmp(key))
                    .ok()
            })
    }

    pub(crate) fn occurrence_owner(
        &self,
        index: usize,
    ) -> lash_kernel_state::paged::OccurrenceOwner {
        use lash_kernel_state::paged::OccurrenceOwner;
        let library = &self.library.0.tables.occurrence_units;
        let unit = match index.checked_sub(library.len()) {
            None => &library[index],
            Some(own) => &self.own.tables.occurrence_units[own],
        };
        match unit {
            Unit::Library(function)
                if self.helpers().contains(function)
                    && !self
                        .instances
                        .values()
                        .any(|document| document.functions.contains_key(function)) =>
            {
                OccurrenceOwner::Helper(*function)
            }
            _ => OccurrenceOwner::Authored,
        }
    }

    pub(crate) fn occurrence_key(&self, index: usize) -> &[u8] {
        let library = &self.library.0.tables.occurrence_keys;
        match index.checked_sub(library.len()) {
            None => &library[index],
            Some(own) => &self.own.tables.occurrence_keys[own],
        }
    }
}
