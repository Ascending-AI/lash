//! Re-binds projections restored from a durable wire to their live host views.
//!
//! Both durable writers carry a projection by identity — name, declared type
//! name and `projection_ref` — so everything decoded from either wire is an
//! unavailable placeholder until the host supplies the binding again. Slot
//! rebinding used to key on the *slot name* alone, which left a copy nested
//! inside a list, tuple, record or heap object holding a placeholder forever,
//! next to the live binding it was a copy of (FIG-2865).
//!
//! The walk therefore visits containers, not just slot names, and matches on the
//! projection's own `name`. A placeholder with no matching binding is left as a
//! placeholder: it refuses reads with
//! [`RuntimeError::ProjectedValueUnavailable`], which is the one honest answer
//! when the host's view is gone.
//!
//! [`RuntimeError::ProjectedValueUnavailable`]: super::RuntimeError::ProjectedValueUnavailable

use std::sync::Arc;

use super::heap::{Heap, HeapObject};
use super::{ProjectedBindings, Record, Value};

pub(crate) fn refresh_value(value: &mut Value, bindings: &ProjectedBindings) {
    match value {
        Value::Projected(projected) => {
            if projected.is_unavailable()
                && let Some(live) = bindings.get(projected.name())
            {
                *value = Value::Projected(live);
            }
        }
        Value::Tuple(values) | Value::List(values) => {
            for value in values.make_mut() {
                refresh_value(value, bindings);
            }
        }
        Value::Record(record) => refresh_record(Arc::make_mut(record), bindings),
        Value::Null
        | Value::Undefined
        | Value::Bool(_)
        | Value::Number(_)
        | Value::String(_)
        | Value::Image(_)
        | Value::Resource(_)
        | Value::Ref(_) => {}
    }
}

pub(crate) fn refresh_values(values: &mut [Value], bindings: &ProjectedBindings) {
    for value in values {
        refresh_value(value, bindings);
    }
}

pub(crate) fn refresh_optional_values(values: &mut [Option<Value>], bindings: &ProjectedBindings) {
    for value in values.iter_mut().flatten() {
        refresh_value(value, bindings);
    }
}

pub(crate) fn refresh_record(record: &mut Record, bindings: &ProjectedBindings) {
    for value in record.values_mut() {
        refresh_value(value, bindings);
    }
}

/// Heap objects are reachable state too: under reference semantics `x = [report]`
/// stores the projection inside a heap list, not inside the slot value.
pub(crate) fn refresh_heap(heap: &mut Heap, bindings: &ProjectedBindings) {
    for entry in heap.entries.values_mut() {
        match &mut entry.object {
            HeapObject::Tuple(values) | HeapObject::List(values) => {
                refresh_values(values, bindings)
            }
            HeapObject::Record(record) => refresh_record(record, bindings),
            HeapObject::Closure { captures, .. } => refresh_values(captures, bindings),
            HeapObject::Map(object) => {
                for (key, value) in &mut object.entries {
                    refresh_value(key, bindings);
                    refresh_value(value, bindings);
                }
            }
            HeapObject::Set(object) => refresh_values(&mut object.values, bindings),
            HeapObject::RegExpMatch(object) => {
                refresh_values(&mut object.items, bindings);
                refresh_value(&mut object.index, bindings);
                refresh_value(&mut object.input, bindings);
                refresh_value(&mut object.groups, bindings);
            }
            // `cause` and `errors` are ordinary values the heap encoders
            // persist, so a projection reaches a restore through them too.
            HeapObject::Error(object) => {
                if let Some(cause) = object.cause.as_mut() {
                    refresh_value(cause, bindings);
                }
                if let Some(errors) = object.errors.as_mut() {
                    refresh_value(errors, bindings);
                }
            }
            HeapObject::RegExp(_)
            | HeapObject::Date(_)
            | HeapObject::Url(_)
            | HeapObject::UrlSearchParams(_) => {}
        }
    }
}
