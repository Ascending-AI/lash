//! Candidate immutable definition IDs reachable from guest roots.

use crate::runtime::{Heap, HeapObject, Record, State, Value};
use std::collections::BTreeSet;

/// Tagged definition IDs reachable from a detached value. Digest strings alone retain nothing.
#[must_use]
pub fn referenced_definition_ids(value: &Value) -> BTreeSet<lash_sansio::ProcessDefinitionId> {
    definition_ids_reachable(std::iter::once(value), None)
}

impl State {
    /// Candidate IDs from every guest root, including maps, sets and errors.
    #[must_use]
    pub fn referenced_definition_ids(&self) -> BTreeSet<lash_sansio::ProcessDefinitionId> {
        let (roots, heap) = self.owning_roots();
        definition_ids_reachable(roots.values(), heap)
    }
}

pub(crate) fn definition_ids_reachable<'a>(
    roots: impl Iterator<Item = &'a Value>,
    heap: Option<&'a Heap>,
) -> BTreeSet<lash_sansio::ProcessDefinitionId> {
    let mut ids = BTreeSet::new();
    walk_records(roots, heap, |record| {
        if record.len() == 1
            && let Some(Value::String(text)) = record.get("$lash_definition_id")
            && let Ok(id) = lash_sansio::ProcessDefinitionId::parse(text.as_str())
        {
            ids.insert(id);
        }
    });
    ids
}

fn walk_records<'a>(
    roots: impl Iterator<Item = &'a Value>,
    heap: Option<&'a Heap>,
    mut visit: impl FnMut(&Record),
) {
    let mut visited = BTreeSet::new();
    let mut pending = roots.collect::<Vec<_>>();
    while let Some(value) = pending.pop() {
        match value {
            Value::Record(record) => {
                visit(record);
                pending.extend(record.values());
            }
            Value::List(values) | Value::Tuple(values) => pending.extend(values.iter()),
            Value::Ref(id) => {
                let Some(heap) = heap else { continue };
                if !visited.insert(*id) {
                    continue;
                }
                let Ok(object) = heap.get(*id) else { continue };
                if let HeapObject::Record(record) = object {
                    visit(record);
                }
                pending.extend(object.values());
            }
            _ => {}
        }
    }
}
