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
use crate::runtime::{LASH_MODULE_REF_KEY, LASH_PROCESS_VALUE_KEY, Record, Value};

/// Every module a process definition anywhere inside `value` references.
///
/// The walk reads detached values only: records, lists and tuples are
/// descended, and a heap reference or a projected value is not followed,
/// because neither owns what it points at. A record marked as a process
/// value whose `module_ref` is not a module reference names nothing.
#[must_use]
pub fn referenced_module_refs(value: &Value) -> BTreeSet<ModuleRef> {
    let mut refs = BTreeSet::new();
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Record(record) => {
                if let Some(module_ref) = process_value_module_ref(record) {
                    refs.insert(module_ref);
                }
                pending.extend(record.iter().map(|(_, value)| value));
            }
            Value::List(values) | Value::Tuple(values) => pending.extend(values.iter()),
            Value::Null
            | Value::Undefined
            | Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Image(_)
            | Value::Resource(_)
            | Value::Ref(_)
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
}
