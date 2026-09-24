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
//!
//! A host can also re-resolve a placeholder by its `projection_ref` rather than
//! its name ([`State::rebind_projections`]). Both go through the one walk here,
//! and both write in place: a projection nested in a heap object is replaced
//! inside that object, so every binding that reaches the object sees it, and
//! nothing is copied (FIG-3628).
//!
//! [`State::rebind_projections`]: super::State::rebind_projections

use std::sync::Arc;

use super::heap::{Heap, HeapObject};
use super::{ProjectedBindings, ProjectedValue, Record, Value};

/// Decides what an unavailable placeholder becomes; `None` leaves it be.
pub(crate) type Rebind<'a> = dyn FnMut(&ProjectedValue) -> Option<ProjectedValue> + 'a;

/// The name-keyed rebinding a restored execution applies from its host's
/// projected bindings.
fn by_name(
    bindings: &ProjectedBindings,
) -> impl FnMut(&ProjectedValue) -> Option<ProjectedValue> + '_ {
    |projected| bindings.get(projected.name())
}

/// Rebinds every unavailable placeholder in `value`, answering whether any was.
pub(crate) fn rebind_value(value: &mut Value, rebind: &mut Rebind<'_>) -> bool {
    match value {
        Value::Projected(projected) => {
            if projected.is_unavailable()
                && let Some(live) = rebind(projected)
            {
                *value = Value::Projected(live);
                return true;
            }
            false
        }
        Value::Tuple(values) | Value::List(values) => {
            if !values.iter().any(holds_placeholder) {
                return false;
            }
            rebind_values(values.make_mut(), rebind)
        }
        Value::Record(record) => {
            if !record.values().any(holds_placeholder) {
                return false;
            }
            rebind_record(Arc::make_mut(record), rebind)
        }
        Value::Null
        | Value::Undefined
        | Value::Bool(_)
        | Value::Number(_)
        | Value::String(_)
        | Value::Image(_)
        | Value::Resource(_)
        | Value::Ref(_) => false,
    }
}

/// Whether an inline value holds a placeholder anywhere, so a walk that finds
/// nothing to rebind never unshares an inline container to look.
fn holds_placeholder(value: &Value) -> bool {
    match value {
        Value::Projected(projected) => projected.is_unavailable(),
        Value::Tuple(values) | Value::List(values) => values.iter().any(holds_placeholder),
        Value::Record(record) => record.values().any(holds_placeholder),
        _ => false,
    }
}

fn rebind_values(values: &mut [Value], rebind: &mut Rebind<'_>) -> bool {
    let mut changed = false;
    for value in values {
        changed |= rebind_value(value, rebind);
    }
    changed
}

pub(crate) fn rebind_record(record: &mut Record, rebind: &mut Rebind<'_>) -> bool {
    let mut changed = false;
    for value in record.values_mut() {
        changed |= rebind_value(value, rebind);
    }
    changed
}

/// Rebinds the placeholders one heap object holds, in place.
pub(crate) fn rebind_object(object: &mut HeapObject, rebind: &mut Rebind<'_>) -> bool {
    match object {
        HeapObject::Tuple(values) | HeapObject::List(values) => rebind_values(values, rebind),
        HeapObject::Record(record) => rebind_record(record, rebind),
        HeapObject::Closure {
            captures,
            name,
            length,
            ..
        } => {
            let mut changed = rebind_values(captures, rebind);
            for value in [name, length].into_iter().flatten() {
                changed |= rebind_value(value, rebind);
            }
            changed
        }
        HeapObject::Map(object) => {
            let mut changed = false;
            for (key, value) in &mut object.entries {
                changed |= rebind_value(key, rebind);
                changed |= rebind_value(value, rebind);
            }
            changed
        }
        HeapObject::Set(object) => rebind_values(&mut object.values, rebind),
        HeapObject::RegExpMatch(object) => {
            let mut changed = rebind_values(&mut object.items, rebind);
            changed |= rebind_value(&mut object.index, rebind);
            changed |= rebind_value(&mut object.input, rebind);
            changed |= rebind_value(&mut object.groups, rebind);
            changed
        }
        // `cause` and `errors` are ordinary values the heap encoders
        // persist, so a projection reaches a restore through them too.
        HeapObject::Error(object) => {
            let mut changed = false;
            if let Some(cause) = object.cause.as_mut() {
                changed |= rebind_value(cause, rebind);
            }
            if let Some(errors) = object.errors.as_mut() {
                changed |= rebind_value(errors, rebind);
            }
            changed
        }
        HeapObject::RegExp(_)
        | HeapObject::Date(_)
        | HeapObject::Url(_)
        | HeapObject::UrlSearchParams(_) => false,
    }
}

pub(crate) fn refresh_value(value: &mut Value, bindings: &ProjectedBindings) {
    rebind_value(value, &mut by_name(bindings));
}

pub(crate) fn refresh_values(values: &mut [Value], bindings: &ProjectedBindings) {
    rebind_values(values, &mut by_name(bindings));
}

pub(crate) fn refresh_optional_values(values: &mut [Option<Value>], bindings: &ProjectedBindings) {
    let mut rebind = by_name(bindings);
    for value in values.iter_mut().flatten() {
        rebind_value(value, &mut rebind);
    }
}

pub(crate) fn refresh_record(record: &mut Record, bindings: &ProjectedBindings) {
    rebind_record(record, &mut by_name(bindings));
}

/// Heap objects are reachable state too: under reference semantics `x = [report]`
/// stores the projection inside a heap list, not inside the slot value.
pub(crate) fn refresh_heap(heap: &mut Heap, bindings: &ProjectedBindings) {
    heap.rebind_projections(&mut by_name(bindings));
}
