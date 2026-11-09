//! Copying a value out of a run, billed by its deep size (`K-EFF-002`),
//! and a decoded datum into a heap (`K-EFF-003`): every mutable object a
//! fresh one, each admitted under the memory schedule. A run copies into
//! its own heap; a host editing a carried session copies into the
//! session's.
//!
//! What is built is held until its parent holds it: an admission may
//! collect, and a member built before it is reachable from nothing else.
//! Each compound drops its members from the held values once built.

use lash_kernel_doc::arena::native::NewObject;
use lash_kernel_doc::{Datum, Value};

use super::{Flow, Run, Vm, fault, read};
use crate::heap::Heap;

/// What a datum is built in: a heap that admits, places and holds the
/// values a build has made so far.
pub(crate) trait Build {
    /// The values built and not yet held by their parent.
    fn held(&mut self) -> &mut Vec<Value>;
    /// Admits `units` under the memory schedule.
    fn admit(&mut self, units: u64) -> Run<()>;
    /// `object`, admitted and placed.
    fn alloc(&mut self, object: NewObject) -> Run<Value>;
    /// A host handle, admitted for `units`.
    fn host_handle(&mut self, handle: &lash_kernel_doc::Handle, units: u64) -> Run<Value>;
    /// An error value of `kind` and `message` carrying `data`.
    fn new_error(&mut self, kind: &str, message: &str, data: Value) -> Run<Value>;
    /// A text value: inline when short, else a long text admitted first.
    fn text(&mut self, text: &str) -> Run<Value> {
        match lash_kernel_doc::arena::ShortText::new(text) {
            Some(short) => Ok(Value::Text(short)),
            None => self.alloc(NewObject::LongText(text.into())),
        }
    }
}

impl<H: Heap> Build for Vm<H> {
    fn held(&mut self) -> &mut Vec<Value> {
        &mut self.held
    }
    fn admit(&mut self, units: u64) -> Run<()> {
        Vm::admit(self, units)
    }
    fn alloc(&mut self, object: NewObject) -> Run<Value> {
        Vm::alloc(self, object)
    }
    fn host_handle(&mut self, handle: &lash_kernel_doc::Handle, units: u64) -> Run<Value> {
        let value = self.heap.arena_mut().host_handle(handle.clone());
        if let Some(placed) = value.handle() {
            self.heap.settle(placed, units);
        }
        Ok(value)
    }
    fn new_error(&mut self, kind: &str, message: &str, data: Value) -> Run<Value> {
        Vm::new_error(self, kind, message, data)
    }
}

/// A copy out of the run, read whole before any of it bills: its deep
/// size (none under `Charge::Off`) and the copy, or the raise it makes.
pub(crate) struct Copied {
    size: Option<u64>,
    datum: Result<Datum, crate::data::Raised>,
}

impl Copied {
    /// Whether the copy raises once billed.
    pub(crate) fn raises(&self) -> bool {
        self.datum.is_err()
    }
}

impl<H: Heap> Vm<H> {
    /// Copies `value` out of the run, billing its deep size first
    /// (`K-EFF-002`, `K-CHG-005`).
    pub(crate) fn copy_out(&mut self, value: &Value) -> Run<Datum> {
        let copied = self.copied(value)?;
        self.bill_copy(copied)
    }

    /// Every read a copy of `value` out of the run makes: its deep size
    /// and its copy, neither billed nor raised yet.
    pub(crate) fn copied(&self, value: &Value) -> Run<Copied> {
        let size = if self.paced {
            None
        } else {
            Some(crate::data::deep_size(self.heap.arena(), value).map_err(read)?)
        };
        let datum = match crate::data::copy_out(self.heap.arena(), self.value_depth, value) {
            Ok(datum) => Ok(datum),
            Err(crate::data::CopyError::Raised(raised)) => Err(raised),
            Err(crate::data::CopyError::Cold { object, chunk }) => {
                return Err(Flow::Cold { object, chunk });
            }
            Err(crate::data::CopyError::Heap(error)) => return Err(fault(error.to_string())),
        };
        Ok(Copied { size, datum })
    }

    /// Bills a copy's deep size, then gives the copy or raises its error.
    pub(crate) fn bill_copy(&mut self, copied: Copied) -> Run<Datum> {
        if let Some(size) = copied.size {
            self.fee(size)?;
        }
        copied
            .datum
            .map_err(|raised| self.error(raised.kind, &raised.message))
    }

    /// Copies `value`, which no frame holds, out of a run that has
    /// stopped, loading each chunk it reaches cold.
    pub(crate) fn copy_stopped(&mut self, value: Value) -> Result<Datum, String> {
        self.held.push(value);
        let copied = loop {
            match crate::data::copy_out(self.heap.arena(), self.value_depth, &value) {
                Err(crate::data::CopyError::Cold { object, chunk }) => {
                    if let Err(stop) = self.reload(object, chunk) {
                        break Err(format!("{stop:?}"));
                    }
                }
                Ok(datum) => break Ok(datum),
                Err(error) => break Err(format!("{error:?}")),
            }
        };
        self.held.pop();
        copied
    }
}

/// Builds `datum`, already decoded by its type, in `into`'s heap.
pub(crate) fn copy_in<B: Build>(into: &mut B, datum: &Datum) -> Run<Value> {
    let mark = into.held().len();
    let built = build(into, datum);
    into.held().truncate(mark);
    built
}

fn build<B: Build>(vm: &mut B, datum: &Datum) -> Run<Value> {
    Ok(match datum {
        Datum::Null => Value::Null,
        Datum::Absent => Value::Absent,
        Datum::Bool(value) => Value::Bool(*value),
        Datum::Float(value) => Value::Float(*value),
        Datum::Int(value) => match value.word() {
            Some(word) => Value::Int(word),
            None => vm.alloc(NewObject::BigInt(value.clone()))?,
        },
        Datum::Text(text) => vm.text(text)?,
        Datum::Bytes(bytes) => vm.alloc(NewObject::Bytes(bytes.as_slice().into()))?,
        Datum::Timestamp(value) => match value.nanoseconds.word() {
            Some(word) => Value::Timestamp(word),
            None => vm.alloc(NewObject::Timestamp(value.nanoseconds.clone()))?,
        },
        Datum::Function(name) => vm.alloc(NewObject::Function(*name))?,
        Datum::Handle(handle) => {
            let units = crate::heap::meter::OBJECT
                .saturating_add((handle.kind.len() + handle.id.len()) as u64);
            vm.admit(units)?;
            vm.host_handle(handle, units)?
        }
        Datum::Tuple(members) => {
            let from = held(vm, members.iter())?;
            let members = vm.held()[from..].to_vec();
            let built = vm.alloc(NewObject::Tuple(members));
            vm.held().truncate(from);
            built?
        }
        Datum::List(items) => {
            let from = held(vm, items.iter())?;
            let items = vm.held()[from..].to_vec();
            let built = vm.alloc(NewObject::List(items));
            vm.held().truncate(from);
            built?
        }
        Datum::Set(members) => {
            let from = held(vm, members.iter())?;
            let members = vm.held()[from..].to_vec();
            let built = vm.alloc(NewObject::Set(members));
            vm.held().truncate(from);
            built?
        }
        Datum::Map(entries) => {
            let from = held(vm, entries.iter().flat_map(|(key, value)| [key, value]))?;
            let pairs = pairs(&vm.held()[from..]);
            let built = vm.alloc(NewObject::Map(pairs));
            vm.held().truncate(from);
            built?
        }
        Datum::Record(fields) => {
            let from = vm.held().len();
            for (name, value) in fields {
                let name = vm.text(name)?;
                vm.held().push(name);
                let value = build(vm, value)?;
                vm.held().push(value);
            }
            let pairs = pairs(&vm.held()[from..]);
            let built = vm.alloc(NewObject::Record(pairs));
            vm.held().truncate(from);
            built?
        }
        Datum::Error(error) => {
            let from = vm.held().len();
            let data = build(vm, error.data())?;
            vm.held().push(data);
            let built = vm.new_error(&error.kind, &error.message, data);
            vm.held().truncate(from);
            built?
        }
        Datum::Number(_) => return Err(fault("an undecoded number reached the run")),
        Datum::FunctionValue(_) => return Err(fault("a function value never enters a run")),
    })
}

/// Builds each of `data` onto the held values, giving where they start.
fn held<'d, B: Build>(vm: &mut B, data: impl Iterator<Item = &'d Datum>) -> Run<usize> {
    let from = vm.held().len();
    for datum in data {
        let value = build(vm, datum)?;
        vm.held().push(value);
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
