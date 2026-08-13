// The persisted-forest invariant: what a durable boundary will accept.
//
// Kept beside the heap rather than inside it because every caller is a wire
// boundary — snapshot decode and encode, continuation decode, resume and
// encode — and because the rule is a statement about the whole graph, not about
// any one operation on it.

use std::collections::{BTreeMap, BTreeSet};

use super::{Heap, HeapId, Value, value_refs};

/// The roots a persisted heap is validated against.
///
/// The split is the whole point: durable roots own what they name, transient
/// roots only borrow it.
#[derive(Default)]
pub(crate) struct PersistedRoots<'a> {
    durable: Vec<(String, &'a Value)>,
    transient: Vec<&'a Value>,
}

impl<'a> PersistedRoots<'a> {
    /// A root whose value survives the boundary: a slot, a global, or a parked
    /// loop binding waiting to be restored into its slot.
    pub(crate) fn durable(&mut self, name: impl Into<String>, value: &'a Value) -> &mut Self {
        self.durable.push((name.into(), value));
        self
    }

    pub(crate) fn durable_all(
        &mut self,
        roots: impl IntoIterator<Item = (impl Into<String>, &'a Value)>,
    ) -> &mut Self {
        for (name, value) in roots {
            self.durable(name, value);
        }
        self
    }

    /// A root that only borrows: an operand, the last value, or an iterator's
    /// captured elements.
    pub(crate) fn transient(&mut self, value: &'a Value) -> &mut Self {
        self.transient.push(value);
        self
    }
}

/// What holds a heap object: a named durable root, or a member of one object.
#[derive(Clone, Debug)]
enum OwnerName {
    Root(String),
    Member(HeapId),
}

impl OwnerName {
    fn describe(&self) -> String {
        match self {
            Self::Root(name) => format!("root `{name}`"),
            Self::Member(id) => format!("object {}", id.get()),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Owner<'a> {
    Root(&'a str),
    Member(HeapId),
}

impl Owner<'_> {
    fn to_owned_name(self) -> OwnerName {
        match self {
            Self::Root(name) => OwnerName::Root(name.to_string()),
            Self::Member(id) => OwnerName::Member(id),
        }
    }

    fn describe(self) -> String {
        self.to_owned_name().describe()
    }
}

impl Heap {
    /// The deepest value any root can materialize into.
    ///
    /// The MessagePack structure guard cannot see this for the heap form: an
    /// object's members are scalars or references, so a chain of a thousand
    /// objects is a flat wire that materializes into a thousand-deep tree. The
    /// depth that matters is the chain of objects, and it is what decides
    /// whether exporting the value can be done at all.
    ///
    /// Computed without recursion, so measuring a wire that is too deep cannot
    /// itself overflow the stack, and terminating on a cyclic graph, so it
    /// cannot hang either. Its one caller validates the forest first, which
    /// rules cycles out — but a measurement that depends on call order for
    /// termination is one edit away from a decode-time hang, and this one runs
    /// on attacker-supplied bytes.
    pub(crate) fn max_value_depth(&self, roots: &PersistedRoots<'_>) -> usize {
        let mut object_depth = BTreeMap::<HeapId, usize>::new();
        for start in self.id_to_slot.keys().copied() {
            if object_depth.contains_key(&start) {
                continue;
            }
            let mut visiting = BTreeSet::new();
            let mut stack = vec![(start, false)];
            while let Some((id, children_done)) = stack.pop() {
                if object_depth.contains_key(&id) {
                    continue;
                }
                let Ok(object) = self.get(id) else {
                    continue;
                };
                let children = object.child_refs();
                if children_done {
                    let deepest = children
                        .iter()
                        .filter_map(|child| object_depth.get(child).copied())
                        .max()
                        .unwrap_or(0);
                    object_depth.insert(id, deepest + 1);
                    visiting.remove(&id);
                    continue;
                }
                visiting.insert(id);
                stack.push((id, true));
                for child in children {
                    // A child already on the path back to the root closes a
                    // cycle. Its depth contributes nothing beyond what the
                    // path already counted, and following it would not end.
                    if !object_depth.contains_key(&child) && !visiting.contains(&child) {
                        stack.push((child, false));
                    }
                }
            }
        }

        let mut deepest = 0;
        for value in roots
            .durable
            .iter()
            .map(|(_, value)| *value)
            .chain(roots.transient.iter().copied())
        {
            let mut pending = vec![(value, 1_usize)];
            while let Some((value, depth)) = pending.pop() {
                match value {
                    Value::Ref(id) => {
                        let chain = object_depth.get(id).copied().unwrap_or(0);
                        deepest = deepest.max(depth.saturating_add(chain).saturating_sub(1));
                    }
                    Value::Tuple(values) | Value::List(values) => {
                        deepest = deepest.max(depth);
                        pending.extend(values.iter().map(|value| (value, depth + 1)));
                    }
                    Value::Record(record) => {
                        deepest = deepest.max(depth);
                        pending.extend(record.values().map(|value| (value, depth + 1)));
                    }
                    _ => deepest = deepest.max(depth),
                }
            }
        }
        deepest
    }

    /// Checks that the persisted heap is a forest of exclusively owned trees.
    ///
    /// This is the one validator for the round-3 invariant, and it runs in
    /// release builds at every durable boundary: snapshot decode and encode, and
    /// continuation decode, resume and encode. Sharing is not merely discouraged
    /// — a wire that expresses it is refused, so nothing downstream has to
    /// decide how shared ownership would restore.
    ///
    /// An *ownership edge* is a reference held by a durable root or by an object
    /// member. Every reachable object must have at most one, and objects must
    /// not form a cycle through those edges. Transient roots — the operand
    /// stack, the last-value register, and an iterator's captured cursor — may
    /// hold duplicate references, because a VM legitimately has the same object
    /// on the stack and in the slot it was just stored into, and a loop cursor
    /// holds the elements it is handing out. Those handles confer no ownership,
    /// so they can never make a second owner.
    ///
    /// Objects must also all be reachable: a wire that carries an object no root
    /// can name is refused rather than silently collected later.
    pub(crate) fn validate_persisted_forest(
        &self,
        roots: &PersistedRoots<'_>,
    ) -> Result<(), String> {
        let mut owners = BTreeMap::<HeapId, OwnerName>::new();
        for (name, root) in &roots.durable {
            for id in value_refs(root) {
                self.claim_ownership(&mut owners, id, Owner::Root(name))?;
            }
        }
        for (parent, object) in self.objects_in_id_order() {
            for id in object.child_refs() {
                self.claim_ownership(&mut owners, id, Owner::Member(parent))?;
            }
        }

        // Every object that no other object holds starts a tree — whether a
        // root names it or nothing does. Walking down the member edges from all
        // of them has to reach every live object: whatever is left over can only
        // be held up by itself, which is a cycle.
        let mut pending = self
            .id_to_slot
            .keys()
            .filter(|id| !matches!(owners.get(id), Some(OwnerName::Member(_))))
            .copied()
            .collect::<Vec<_>>();
        let mut visited = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !visited.insert(id) {
                continue;
            }
            let object = self
                .get(id)
                .map_err(|_| format!("dangling heap reference {}", id.get()))?;
            pending.extend(object.child_refs());
        }
        if visited.len() != self.id_to_slot.len() {
            let orphan = self
                .id_to_slot
                .keys()
                .find(|id| !visited.contains(id))
                .map_or(0, |id| id.get());
            return Err(format!(
                "heap object graph must be acyclic; object {orphan} is only reachable through itself"
            ));
        }

        // Reachability from the roots the wire actually carries, which is a
        // stricter statement than "has an owner": an owned subtree whose top is
        // unreachable is still garbage on the wire.
        let mut reachable = BTreeSet::new();
        let mut pending = roots
            .durable
            .iter()
            .flat_map(|(_, root)| value_refs(root))
            .chain(roots.transient.iter().flat_map(|root| value_refs(root)))
            .collect::<Vec<_>>();
        while let Some(id) = pending.pop() {
            if !reachable.insert(id) {
                continue;
            }
            let object = self
                .get(id)
                .map_err(|_| format!("dangling heap reference {}", id.get()))?;
            pending.extend(object.child_refs());
        }
        if reachable.len() != self.id_to_slot.len() {
            return Err("heap wire must not contain unreachable objects".to_string());
        }
        Ok(())
    }

    fn claim_ownership(
        &self,
        owners: &mut BTreeMap<HeapId, OwnerName>,
        id: HeapId,
        owner: Owner<'_>,
    ) -> Result<(), String> {
        self.get(id)
            .map_err(|_| format!("dangling heap reference {}", id.get()))?;
        match owners.entry(id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(owner.to_owned_name());
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(entry) => Err(format!(
                "heap object {} must have one owner, but {} and {} both hold it",
                id.get(),
                entry.get().describe(),
                owner.describe()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::HeapObject;

    /// Depth measurement terminates on a cyclic graph.
    ///
    /// The forest validator rejects cycles before this runs today. This asserts
    /// the property directly, so the guarantee survives a change to what runs
    /// first.
    #[test]
    fn depth_measurement_terminates_on_a_cycle() {
        let mut heap = Heap::default();
        let Value::Ref(first) = heap
            .allocate(HeapObject::List(Vec::new()))
            .expect("allocate first")
        else {
            unreachable!()
        };
        let Value::Ref(second) = heap
            .allocate(HeapObject::List(vec![Value::Ref(first)]))
            .expect("allocate second")
        else {
            unreachable!()
        };
        heap.replace_object(first, HeapObject::List(vec![Value::Ref(second)]))
            .expect("close the cycle");

        let root = Value::Ref(first);
        let mut roots = PersistedRoots::default();
        roots.durable("root", &root);
        // The measurement is finite and bounded by the number of objects; the
        // exact value is not meaningful for a graph that cannot be persisted.
        assert!(heap.max_value_depth(&roots) <= 2);
        assert!(heap.validate_persisted_forest(&roots).is_err());
    }
}
