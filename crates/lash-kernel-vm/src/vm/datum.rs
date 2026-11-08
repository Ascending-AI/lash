//! Copying a decoded datum into a run (`K-EFF-003`): every mutable object
//! a fresh one, each admitted under the memory schedule.

use lash_kernel_doc::arena::native::NewObject;
use lash_kernel_doc::{Datum, Value};

use super::{Run, Vm, fault};
use crate::heap::Heap;

/// Builds `datum`, already decoded by its type, in `vm`'s heap.
pub(crate) fn copy_in<H: Heap>(vm: &mut Vm<H>, datum: &Datum) -> Run<Value> {
    Ok(match datum {
        Datum::Null => Value::Null,
        Datum::Absent => Value::Absent,
        Datum::Bool(value) => Value::Bool(*value),
        Datum::Float(value) => Value::Float(*value),
        Datum::Int(value) => match value.word() {
            Some(word) => Value::Int(word),
            None => vm.alloc(NewObject::BigInt(value.clone()))?,
        },
        Datum::Text(text) => vm.heap.arena_mut().text(text),
        Datum::Bytes(bytes) => vm.heap.arena_mut().bytes(bytes.as_slice()),
        Datum::Timestamp(value) => vm.heap.arena_mut().timestamp(value.nanoseconds.clone()),
        Datum::Function(name) => vm.heap.arena_mut().function(*name),
        Datum::Handle(handle) => vm.heap.arena_mut().host_handle(handle.clone()),
        Datum::Tuple(members) => {
            let members = all(vm, members)?;
            vm.alloc(NewObject::Tuple(members))?
        }
        Datum::List(items) => {
            let items = all(vm, items)?;
            vm.alloc(NewObject::List(items))?
        }
        Datum::Set(members) => {
            let members = all(vm, members)?;
            vm.alloc(NewObject::Set(members))?
        }
        Datum::Map(entries) => {
            let mut pairs = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                pairs.push((copy_in(vm, key)?, copy_in(vm, value)?));
            }
            vm.alloc(NewObject::Map(pairs))?
        }
        Datum::Record(fields) => {
            let mut pairs = Vec::with_capacity(fields.len());
            for (name, value) in fields {
                let name = vm.heap.arena_mut().text(name);
                pairs.push((name, copy_in(vm, value)?));
            }
            vm.alloc(NewObject::Record(pairs))?
        }
        Datum::Error(error) => {
            let data = copy_in(vm, error.data())?;
            vm.admit(crate::heap::slots::ERROR)?;
            vm.heap
                .arena_mut()
                .error(&error.kind, &error.message, data)
                .map_err(|error| fault(error.to_string()))?
        }
        Datum::Number(_) => return Err(fault("an undecoded number reached the run")),
        Datum::FunctionValue(_) => return Err(fault("a function value never enters a run")),
    })
}

fn all<H: Heap>(vm: &mut Vm<H>, data: &[Datum]) -> Run<Vec<Value>> {
    data.iter().map(|datum| copy_in(vm, datum)).collect()
}
