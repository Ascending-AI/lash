// Which values reach a closure: the question the program boundary asks.
//
// A closure's function index only means something inside the program that
// compiled it, so nothing reaching a closure can outlive that program. The
// state boundary asks this of every runtime root before it installs an
// execution's result, and the answer is a property of the whole graph rather
// than of any one object, so it is computed here for the heap at once.

use rustc_hash::{FxHashMap, FxHashSet};

use super::{Heap, HeapId, HeapObject, Value, value_refs};

impl Heap {
    /// Which objects a closure is reachable from, closures themselves included.
    ///
    /// The answer is computed for the whole heap at once, by propagating
    /// backwards along [`HeapObject::child_refs`] from every closure, rather
    /// than by searching forward from each root: roots routinely share
    /// subgraphs, and a per-root search re-walks the shared part once per root.
    /// A heap holding no closure at all — the common case — costs one pass and
    /// allocates nothing.
    pub(crate) fn closure_reach(&self) -> ClosureReach {
        let mut reached = FxHashSet::default();
        let mut pending = Vec::new();
        for (id, object) in self.objects_in_id_order() {
            if matches!(object, HeapObject::Closure { .. }) {
                reached.insert(id);
                pending.push(id);
            }
        }
        if pending.is_empty() {
            return ClosureReach { reached };
        }
        let mut holders: FxHashMap<HeapId, Vec<HeapId>> = FxHashMap::default();
        for (id, object) in self.objects_in_id_order() {
            for child in object.child_refs() {
                holders.entry(child).or_default().push(id);
            }
        }
        while let Some(id) = pending.pop() {
            for holder in holders.get(&id).into_iter().flatten() {
                if reached.insert(*holder) {
                    pending.push(*holder);
                }
            }
        }
        ClosureReach { reached }
    }
}

/// The objects one heap's closures are reachable from, as [`Heap::closure_reach`]
/// computed them. Only valid for the heap it was taken from, and only until that
/// heap changes.
pub(crate) struct ClosureReach {
    reached: FxHashSet<HeapId>,
}

impl ClosureReach {
    /// Whether `value` reaches a closure anywhere in its object graph.
    pub(crate) fn covers(&self, value: &Value) -> bool {
        if self.reached.is_empty() {
            return false;
        }
        value_refs(value).iter().any(|id| self.reached.contains(id))
    }
}
