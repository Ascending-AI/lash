//! A saved function's captures are frozen as its defining cell left them
//! (FIG-5778), and each call installs a fresh copy of what was frozen
//! (FIG-6193): no later cell's write reaches what the function reads, and
//! no call's write reaches another call's.
//!
//! A copy is deep over the mutable containers (lists, tuples, maps, sets
//! and records). Functions, closures, texts and every immutable value
//! stay shared. A member reached twice is copied once, so sharing inside
//! the captures survives; a container that reaches itself has no frozen
//! copy and raises.

use std::collections::HashMap;
use std::sync::Arc;

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::ArenaHandle;

use super::{NewObject, Run, Vm, read};
use crate::heap::Heap;

/// Each container met so far, by its handle's place: its copy, or `None`
/// while it is copied.
type Memo = HashMap<(u32, u32), Option<Value>>;

fn key(handle: ArenaHandle) -> (u32, u32) {
    (handle.index(), handle.generation())
}

impl<H: Heap> Vm<H> {
    /// Whether the closure `callee` is one of the session's saved functions: a global
    /// past the program's and the session's bindings.
    pub(super) fn saved_function(&self, callee: ArenaHandle) -> bool {
        let named = self.program.globals.len() + self.session.names.len();
        self.globals.get(named..).is_some_and(|saved| {
            saved
                .iter()
                .any(|value| matches!(value, Value::Closure(saved) if *saved == callee))
        })
    }

    /// A fresh closure of saved `closure`'s identity whose cells hold
    /// copies of what its cells hold, for one call.
    pub(super) fn fresh_closure(&mut self, closure: ArenaHandle) -> Run<ArenaHandle> {
        let (identity, frozen) = self.captured(closure)?;
        let mark = self.held.len();
        let built = self.copies(&frozen).and_then(|()| {
            let count = frozen.len();
            self.admit(
                crate::heap::meter::CELL
                    .saturating_mul(count as u64)
                    .saturating_add(crate::heap::meter::closure(count)),
            )
        });
        let built = built.map(|()| {
            let cells = self.held[mark..]
                .iter()
                .map(|value| self.heap.cell(*value))
                .collect::<Vec<_>>();
            self.heap.closure(identity, cells)
        });
        self.held.truncate(mark);
        built
    }

    /// Freezes saved `closure`'s captures in place: each of its cells
    /// holds a copy of what it held, shared with nothing else.
    pub(crate) fn freeze(&mut self, closure: ArenaHandle) -> Run<()> {
        let cells = self.heap.arena().closure(closure).map_err(read)?.1.to_vec();
        let (_, frozen) = self.captured(closure)?;
        let mark = self.held.len();
        let built = self.copies(&frozen).and_then(|()| {
            for (cell, value) in cells.iter().zip(&self.held[mark..]) {
                self.heap.cell_set(*cell, *value).map_err(read)?;
            }
            Ok(())
        });
        self.held.truncate(mark);
        built
    }

    /// `closure`'s identity and the values its cells hold.
    fn captured(
        &self,
        closure: ArenaHandle,
    ) -> Run<(Arc<lash_kernel_doc::arena::ClosureIdentity>, Vec<Value>)> {
        let (identity, cells, _) = self.heap.arena().closure(closure).map_err(read)?;
        let values = cells
            .iter()
            .map(|cell| self.heap.cell_get(*cell))
            .collect::<Result<Vec<_>, _>>()
            .map_err(read)?;
        Ok((Arc::new(identity.clone()), values))
    }

    /// Pushes a copy of each of `values` onto the held values, in order.
    /// On an error what it pushed stays for the caller to drop.
    fn copies(&mut self, values: &[Value]) -> Run<()> {
        let mut memo = Memo::new();
        for value in values {
            let copy = self.detached(*value, &mut memo)?;
            self.held.push(copy);
        }
        Ok(())
    }

    /// `value` copied over its mutable containers.
    fn detached(&mut self, value: Value, memo: &mut Memo) -> Run<Value> {
        let (handle, sequence) = match value {
            Value::List(handle) | Value::Tuple(handle) => (handle, true),
            Value::Map(handle) | Value::Set(handle) | Value::Record(handle) => (handle, false),
            _ => return Ok(value),
        };
        match memo.get(&key(handle)) {
            Some(Some(copy)) => return Ok(*copy),
            Some(None) => {
                return Err(self.error(
                    "value_error",
                    "a saved function's capture reaches itself, so it has no frozen copy",
                ));
            }
            None => {}
        }
        let arena = self.heap.arena();
        let members = if sequence {
            arena.sequence(handle)
        } else {
            arena.entries(handle)
        }
        .map_err(read)?
        .to_vec();
        memo.insert(key(handle), None);
        let mark = self.held.len();
        let built = members
            .into_iter()
            .try_for_each(|member| {
                let copy = self.detached(member, memo)?;
                self.held.push(copy);
                Ok(())
            })
            .and_then(|()| {
                let copied = self.held[mark..].to_vec();
                let pairs = || {
                    copied
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|&[key, value]| (key, value))
                        .collect()
                };
                let object = match value {
                    Value::List(_) => NewObject::List(copied.clone()),
                    Value::Tuple(_) => NewObject::Tuple(copied.clone()),
                    Value::Set(_) => NewObject::Set(copied.iter().step_by(2).copied().collect()),
                    Value::Map(_) => NewObject::Map(pairs()),
                    _ => NewObject::Record(pairs()),
                };
                self.alloc(object)
            });
        self.held.truncate(mark);
        let copy = built?;
        memo.insert(key(handle), Some(copy));
        Ok(copy)
    }
}
