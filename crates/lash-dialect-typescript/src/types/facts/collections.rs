//! Construction proofs for collection reads at definitely initialized sites.

use super::{Collector, Expr, Facts, Source, Ty, spelling};

impl<'a> Collector<'a> {
    /// Keep candidates before an early read widens the ordinary binding type.
    pub(super) fn collection_definitions(&mut self) -> Vec<(usize, &'a Expr, &'a str)> {
        let mut collections = Vec::new();
        for (binding, entry) in self.bindings.iter_mut().enumerate() {
            entry
                .sources
                .retain(|source| !matches!(source, Source::Unassigned));
            if !entry.session_scope
                && !entry.captured
                && let [Source::Value(value)] = entry.sources.as_slice()
            {
                let value = *value;
                if let Expr::New { constructor, .. } = value
                    && let Expr::Ident(root, _) = constructor.as_ref()
                {
                    collections.push((binding, value, root.as_str()));
                }
            }
            if entry.unassigned && (entry.early || entry.session_scope) {
                entry.sources.push(Source::Opaque);
            }
        }
        collections
    }

    pub(super) fn record_initialized_read(&mut self, binding: usize, name: &str) {
        if !self.bindings[binding].session_scope
            && self.function_depth == self.bindings[binding].depth
            && self.defined.contains(&binding)
        {
            self.initialized_reads.insert(spelling(name));
        }
    }
}

impl Facts {
    pub(super) fn derive_collections(&mut self, definitions: Vec<(usize, &Expr, &str)>) {
        // A single construction fixes an uncaptured local's collection kind.
        // Reads before that construction keep their ordinary unknown type.
        for (binding, value, root) in definitions {
            if !self.names(root) && !self.imported.contains_key(root) {
                let ty = self.type_of(value);
                if matches!(ty, Ty::Map | Ty::Set) {
                    self.collections.insert(binding, ty);
                }
            }
        }
    }

    pub(super) fn collection_primitive(&self, expr: &Expr) -> Option<Ty> {
        if let Expr::Ident(name, _) = expr
            && self.initialized_reads.contains(&spelling(name))
            && let Some(binding) = self.binding(name)
            && let Some(ty) = self.collections.get(&binding)
        {
            return Some(ty.clone());
        }
        None
    }
}
