//! Copying a decoded datum into a run (`K-EFF-003`): every mutable object
//! a fresh one, each admitted under the memory schedule.
//!
//! What is built is held until its parent holds it: an admission may
//! collect, and a member built before it is reachable from nothing else.
//! Each compound drops its members from the held values once built.

use lash_kernel_doc::arena::native::NewObject;
use lash_kernel_doc::{Datum, Value};

use super::{Run, Vm, fault};
use crate::heap::Heap;

/// Builds `datum`, already decoded by its type, in `vm`'s heap.
pub(crate) fn copy_in<H: Heap>(vm: &mut Vm<H>, datum: &Datum) -> Run<Value> {
    let mark = vm.held.len();
    let built = build(vm, datum);
    vm.held.truncate(mark);
    built
}

fn build<H: Heap>(vm: &mut Vm<H>, datum: &Datum) -> Run<Value> {
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
            let from = held(vm, members.iter())?;
            let members = vm.held[from..].to_vec();
            let built = vm.alloc(NewObject::Tuple(members));
            vm.held.truncate(from);
            built?
        }
        Datum::List(items) => {
            let from = held(vm, items.iter())?;
            let items = vm.held[from..].to_vec();
            let built = vm.alloc(NewObject::List(items));
            vm.held.truncate(from);
            built?
        }
        Datum::Set(members) => {
            let from = held(vm, members.iter())?;
            let members = vm.held[from..].to_vec();
            let built = vm.alloc(NewObject::Set(members));
            vm.held.truncate(from);
            built?
        }
        Datum::Map(entries) => {
            let from = held(vm, entries.iter().flat_map(|(key, value)| [key, value]))?;
            let pairs = pairs(&vm.held[from..]);
            let built = vm.alloc(NewObject::Map(pairs));
            vm.held.truncate(from);
            built?
        }
        Datum::Record(fields) => {
            let from = vm.held.len();
            for (name, value) in fields {
                let name = vm.heap.arena_mut().text(name);
                vm.held.push(name);
                let value = build(vm, value)?;
                vm.held.push(value);
            }
            let pairs = pairs(&vm.held[from..]);
            let built = vm.alloc(NewObject::Record(pairs));
            vm.held.truncate(from);
            built?
        }
        Datum::Error(error) => {
            let from = vm.held.len();
            let data = build(vm, error.data())?;
            vm.held.push(data);
            let admitted = vm.admit(crate::heap::slots::ERROR);
            vm.held.truncate(from);
            admitted?;
            vm.heap
                .error(&error.kind, &error.message, data)
                .map_err(|error| fault(error.to_string()))?
        }
        Datum::Number(_) => return Err(fault("an undecoded number reached the run")),
        Datum::FunctionValue(_) => return Err(fault("a function value never enters a run")),
    })
}

/// Builds each of `data` onto the held values, giving where they start.
fn held<'d, H: Heap>(vm: &mut Vm<H>, data: impl Iterator<Item = &'d Datum>) -> Run<usize> {
    let from = vm.held.len();
    for datum in data {
        let value = build(vm, datum)?;
        vm.held.push(value);
    }
    Ok(from)
}

fn pairs(flat: &[Value]) -> Vec<(Value, Value)> {
    flat.as_chunks::<2>()
        .0
        .iter()
        .map(|&[key, value]| (key, value))
        .collect()
}
