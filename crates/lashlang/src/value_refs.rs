//! The module artifacts a value references (ADR 0113 §3.1).
//!
//! A process definition value (the record
//! [`ProcessDefinitionIdentity::to_process_value`](crate::ProcessDefinitionIdentity::to_process_value)
//! encodes) names the module that holds its code by reference. A host that
//! keeps such values alive in a referrer's scope must keep the named modules
//! alive under the same referrer, and this is how it finds them.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde::de::IntoDeserializer;

use crate::artifact::ModuleRef;
use crate::runtime::{
    Heap, HeapObject, LASH_MODULE_REF_KEY, LASH_PROCESS_VALUE_KEY, Record, State, Value,
};

/// Every module a process definition anywhere inside `value` references.
///
/// The walk reads a detached value: records, lists and tuples are descended.
/// A heap reference is not followed, because a detached value has no heap to
/// resolve it in ([`State::referenced_module_refs`] follows them), and a
/// projected value is not materialized. A record marked as a process value
/// whose `module_ref` is not a module reference names nothing.
#[must_use]
pub fn referenced_module_refs(value: &Value) -> BTreeSet<ModuleRef> {
    module_refs_reachable(std::iter::once(value), None)
}

impl State {
    /// Every module a process definition held by any binding references,
    /// however it is held (ADR 0113 §3.1).
    ///
    /// The walk starts from the records that own the bindings, not the host
    /// view, and follows heap references with each object visited once: a
    /// definition inside a `Map`, a `Set` or an `Error`, which the host view
    /// omits, is found as well as one in a plain record.
    #[must_use]
    pub fn referenced_module_refs(&self) -> BTreeSet<ModuleRef> {
        let (roots, heap) = self.owning_roots();
        module_refs_reachable(roots.values(), heap)
    }
}

fn module_refs_reachable<'a>(
    roots: impl Iterator<Item = &'a Value>,
    heap: Option<&'a Heap>,
) -> BTreeSet<ModuleRef> {
    let mut refs = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut pending = roots.collect::<Vec<_>>();
    while let Some(value) = pending.pop() {
        match value {
            Value::Record(record) => {
                refs.extend(process_value_module_ref(record));
                pending.extend(record.values());
            }
            Value::List(values) | Value::Tuple(values) => pending.extend(values.iter()),
            Value::Ref(id) => {
                let Some(heap) = heap else { continue };
                if !visited.insert(*id) {
                    continue;
                }
                // A reference the heap cannot resolve holds nothing.
                let Ok(object) = heap.get(*id) else { continue };
                if let HeapObject::Record(record) = object {
                    refs.extend(process_value_module_ref(record));
                }
                pending.extend(object.values());
            }
            Value::Null
            | Value::Undefined
            | Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Image(_)
            | Value::Resource(_)
            | Value::Projected(_) => {}
        }
    }
    refs
}

/// The module a process definition record names, if `record` is one.
fn process_value_module_ref(record: &Record) -> Option<ModuleRef> {
    if !matches!(record.get(LASH_PROCESS_VALUE_KEY), Some(Value::Bool(true))) {
        return None;
    }
    let Some(Value::String(text)) = record.get(LASH_MODULE_REF_KEY) else {
        return None;
    };
    let text: serde::de::value::StrDeserializer<'_, serde::de::value::Error> =
        text.as_str().into_deserializer();
    let module_ref = ModuleRef::deserialize(text).ok()?;
    module_ref
        .hash_hex()
        .is_some_and(|hash| !hash.is_empty())
        .then_some(module_ref)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{ContentHash, HostRequirementsRef, ProcessDefinitionIdentity, ProcessRef};

    fn definition(source: &str) -> (ModuleRef, Value) {
        let module_ref = ModuleRef::new(&ContentHash::new(source));
        let identity = ProcessDefinitionIdentity::new(
            module_ref.clone(),
            HostRequirementsRef::new(&ContentHash::new("host")),
            ProcessRef::new(ContentHash::new("component"), 0),
            "run",
        );
        (module_ref, crate::from_json(identity.to_process_value()))
    }

    fn record(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
        let mut record = Record::new();
        for (name, value) in entries {
            record.insert(name.to_string(), value);
        }
        Value::Record(Arc::new(record))
    }

    #[test]
    fn a_definition_names_its_module() {
        let (module_ref, value) = definition("a");
        assert_eq!(referenced_module_refs(&value), BTreeSet::from([module_ref]));
    }

    #[test]
    fn definitions_nested_in_records_and_lists_are_all_found() {
        let (first, first_value) = definition("a");
        let (second, second_value) = definition("b");
        let value = record([
            ("x", Value::Number(1.0)),
            (
                "nested",
                Value::List(vec![Value::Null, record([("p", second_value)])].into()),
            ),
            ("p", first_value.clone()),
            ("again", Value::Tuple(vec![first_value].into())),
        ]);
        assert_eq!(
            referenced_module_refs(&value),
            BTreeSet::from([first, second])
        );
    }

    #[test]
    fn plain_values_and_unmarked_records_name_nothing() {
        let (module_ref, _) = definition("a");
        let unmarked = record([(
            LASH_MODULE_REF_KEY,
            Value::String(module_ref.as_str().into()),
        )]);
        let malformed = record([
            (LASH_PROCESS_VALUE_KEY, Value::Bool(true)),
            (LASH_MODULE_REF_KEY, Value::String("not-a-module".into())),
        ]);
        for value in [Value::String("module_ref".into()), unmarked, malformed] {
            assert!(referenced_module_refs(&value).is_empty(), "{value:?}");
        }
    }

    #[test]
    fn a_state_finds_definitions_the_host_view_omits() {
        let (plain, plain_value) = definition("plain");
        let (in_map, in_map_value) = definition("in-map");
        let mut state = State::new();
        state
            .insert_global("p", plain_value)
            .expect("bind a definition");
        assert_eq!(
            state.referenced_module_refs(),
            BTreeSet::from([plain.clone()]),
            "a plain state is walked through its own record"
        );

        // A `Map` has no host view, so only a heap-aware walk finds the
        // definition inside it; the map refers to itself so the walk must
        // stop at an object it already visited.
        let mut heap = Heap::with_limit(u64::MAX);
        let map = heap
            .allocate(HeapObject::Map(crate::runtime::MapObject {
                entries: Vec::new(),
            }))
            .expect("allocate a map");
        let Value::Ref(map_id) = map else {
            panic!("a heap object is referenced: {map:?}");
        };
        heap.map_set(map_id, Value::String("q".into()), in_map_value)
            .expect("store the definition");
        heap.map_set(map_id, Value::String("self".into()), Value::Ref(map_id))
            .expect("store the map in itself");
        let mut roots = Record::new();
        roots.insert("m".to_string(), Value::Ref(map_id));
        assert_eq!(
            module_refs_reachable(roots.values(), Some(&heap)),
            BTreeSet::from([in_map])
        );
        assert!(
            module_refs_reachable(roots.values(), None).is_empty(),
            "a detached walk does not follow references"
        );
    }
}
