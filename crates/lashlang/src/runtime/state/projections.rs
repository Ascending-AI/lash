//! What a host reads about bindings the host view cannot carry.

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

    /// The records that own the bindings and the heap they reach into: the
    /// runtime roots while heap-backed, the plain record otherwise.
    pub(crate) fn owning_roots(&self) -> (&Record, Option<&Heap>) {
        match &self.mode {
            StateMode::Plain(globals) => (globals, None),
            StateMode::HeapBacked(backed) => (&backed.runtime_globals, Some(&backed.heap)),
        }
    }
}
