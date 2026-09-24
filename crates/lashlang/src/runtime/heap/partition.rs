// The durable partition: which root carries each live heap object.
//
// A persisted state is written as one fragment per root, so a small edit
// rewrites the fragments it touched rather than the whole heap. Reference
// semantics let several roots — and several members — name one object, so
// "the objects a root reaches" overlaps between roots and cannot be the unit.
// The unit is ownership by first discovery instead: the roots are walked in
// name order, each depth-first in member order, and an object belongs to the
// root whose walk reached it first. Every live object is carried exactly once,
// sharing survives because a reference is just an id, and the assignment is a
// pure function of the graph, so a reader can re-derive it and refuse a wire
// that assigns an object anywhere else.

use rustc_hash::FxHashMap;

use super::{Heap, HeapId, HeapObject, Value, value_refs};

/// The partition of a heap's live objects among its roots.
pub(crate) struct DurablePartition {
    /// The objects each root carries, in root order, each list ascending by id.
    pub(crate) owned: Vec<Vec<HeapId>>,
    /// Whether the live graph is outside Lashlang's exclusive-ownership forest:
    /// some object has more than one ownership edge, or is a TypeScript exotic.
    /// It is exactly the question `validate_persisted_forest` answers, asked of
    /// the live graph without collecting it first.
    pub(crate) reference_semantics: bool,
    /// The logical bytes of the live objects: what a collected heap would count.
    pub(crate) live_logical_bytes: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Visit {
    OnPath,
    Done,
}

impl Heap {
    /// Partitions the objects the roots reach among them, in the order given.
    ///
    /// Unreachable objects are simply never visited, so the partition of an
    /// uncollected heap is the partition of its collection. A cycle is refused
    /// with the persisted graph's own message: the durable boundary cannot
    /// carry one, and the write is where that has to surface.
    pub(crate) fn durable_partition<'a>(
        &self,
        roots: impl IntoIterator<Item = &'a Value>,
    ) -> Result<DurablePartition, String> {
        let mut visits = FxHashMap::<HeapId, Visit>::default();
        let mut ownership_edges = FxHashMap::<HeapId, u32>::default();
        let mut owned = Vec::new();
        let mut reference_semantics = false;
        let mut live_logical_bytes = 0_u64;
        for root in roots {
            let mut carried = Vec::new();
            let refs = value_refs(root);
            let mut stack = Vec::with_capacity(refs.len());
            for id in refs.into_iter().rev() {
                reference_semantics |= claim(&mut ownership_edges, id);
                stack.push((id, false));
            }
            while let Some((id, exiting)) = stack.pop() {
                if exiting {
                    visits.insert(id, Visit::Done);
                    continue;
                }
                match visits.get(&id) {
                    Some(Visit::OnPath) => {
                        return Err(format!(
                            "heap object graph must be acyclic; cycle reaches object {}",
                            id.get()
                        ));
                    }
                    Some(Visit::Done) => continue,
                    None => {}
                }
                let entry = self
                    .entries
                    .get(&id)
                    .ok_or_else(|| format!("dangling heap reference {}", id.get()))?;
                visits.insert(id, Visit::OnPath);
                carried.push(id);
                live_logical_bytes = live_logical_bytes.saturating_add(entry.logical_bytes);
                reference_semantics |= is_typescript_exotic(&entry.object);
                stack.push((id, true));
                for child in entry.object.child_refs().into_iter().rev() {
                    reference_semantics |= claim(&mut ownership_edges, child);
                    stack.push((child, false));
                }
            }
            carried.sort_unstable();
            owned.push(carried);
        }
        Ok(DurablePartition {
            owned,
            reference_semantics,
            live_logical_bytes,
        })
    }
}

/// Counts one ownership edge into `id`, answering whether it is a second one.
fn claim(ownership_edges: &mut FxHashMap<HeapId, u32>, id: HeapId) -> bool {
    let edges = ownership_edges.entry(id).or_default();
    *edges += 1;
    *edges > 1
}

fn is_typescript_exotic(object: &HeapObject) -> bool {
    matches!(
        object,
        HeapObject::RegExp(_)
            | HeapObject::RegExpMatch(_)
            | HeapObject::Map(_)
            | HeapObject::Set(_)
            | HeapObject::Date(_)
            | HeapObject::Error(_)
            | HeapObject::Url(_)
            | HeapObject::UrlSearchParams(_)
    )
}
