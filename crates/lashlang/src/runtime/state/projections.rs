//! What a host reads about bindings the host view cannot carry, and how it
//! re-resolves the projections a reload left as placeholders.

use super::*;

impl State {
    /// The bindings the host view cannot carry, each with a bounded summary.
    ///
    /// A binding is omitted from [`State::globals`] when it has no detached
    /// host shape (a `Map`, a `Date`, a record holding one) or holds a pending
    /// tool handle, yet a later cell can still name it. A host that lists the
    /// session's bindings lists these too, by this summary: a console-style
    /// rendering cut to a few members, two levels and
    /// [`BINDING_SUMMARY_MAX_CHARS`] characters (FIG-3629).
    pub fn opaque_bindings(&self) -> Vec<(String, String)> {
        let StateMode::HeapBacked(backed) = &self.mode else {
            return Vec::new();
        };
        backed
            .runtime_globals
            .iter()
            .filter(|(name, _)| backed.projected.get(name).is_none())
            .map(|(name, value)| (name.to_string(), backed.heap.summarize(value)))
            .collect()
    }

    /// Every unavailable projection a binding reaches, by binding, in binding
    /// order: the placeholders a reload left that a host may re-resolve.
    ///
    /// A projection held by an object two bindings reach is reported under
    /// both, since both bindings depend on it.
    pub fn unavailable_projections(&self) -> Vec<(String, ProjectedValue)> {
        let mut found = Vec::new();
        match &self.mode {
            StateMode::Plain(globals) => {
                let heap = Heap::default();
                for (name, value) in globals.iter() {
                    for projected in heap.unavailable_projections_from(value) {
                        found.push((name.to_string(), projected));
                    }
                }
            }
            StateMode::HeapBacked(backed) => {
                for (name, value) in backed.runtime_globals.iter() {
                    for projected in backed.heap.unavailable_projections_from(value) {
                        found.push((name.to_string(), projected));
                    }
                }
            }
        }
        found
    }

    /// Replaces unavailable projections in place with what `rebind` answers
    /// for them; one it answers `None` for stays a placeholder.
    ///
    /// Nothing is copied: a projection inside an object is replaced inside
    /// that object, so every binding that reaches the object — however many
    /// there are — sees the rebound value, and object identity is exactly what
    /// it was (FIG-3628). A projection's durable form is its identity, which
    /// rebinding does not change, so no durable fragment moves either.
    pub fn rebind_projections(
        &mut self,
        mut rebind: impl FnMut(&ProjectedValue) -> Option<ProjectedValue>,
    ) -> Result<(), RuntimeError> {
        match &mut self.mode {
            StateMode::Plain(globals) => {
                crate::runtime::projected_refresh::rebind_record(globals, &mut rebind);
            }
            StateMode::HeapBacked(backed) => {
                crate::runtime::projected_refresh::rebind_record(
                    &mut backed.runtime_globals,
                    &mut rebind,
                );
                backed.heap.rebind_projections(&mut rebind);
                backed.projected = host_view(&backed.runtime_globals, &mut backed.heap)?;
            }
        }
        Ok(())
    }
}
