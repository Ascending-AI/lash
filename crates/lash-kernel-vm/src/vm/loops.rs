//! `for` loops (`K-ITER-001` … `K-ITER-005`). A loop's state is in hidden
//! locals from its site's `slot`: what it walks and its cursor; a protocol
//! loop also keeps the size and what remains of a checked map or set, the
//! raw item a step gave and whether it gave one.
//!
//! A list or tuple cursor is the next position; a map or set cursor the
//! sequence of the last entry given (absent before the first); a text
//! cursor the byte where the next scalar starts. What a protocol's `open`
//! gives that the loop cannot walk itself is a driver, which its `next`
//! steps.

use lash_kernel_doc::Value;

use super::{Run, Vm, fault};
use crate::bytecode::program::{LoopSite, Unit};
use crate::heap::Heap;

const NOT_ITERABLE: &str = "`for` iterates a list, a tuple, a map or a set";

const WALKED: usize = 0;
const CURSOR: usize = 1;
const SIZE: usize = 2;
const REMAINING: usize = 3;
const RAW: usize = 4;
const MORE: usize = 5;

/// A count as a value.
fn counted(count: u64) -> Value {
    Value::Int(i64::try_from(count).unwrap_or(i64::MAX))
}

fn walkable(value: &Value, text: bool) -> bool {
    match value {
        Value::List(_) | Value::Tuple(_) | Value::Map(_) | Value::Set(_) => true,
        Value::Text(_) | Value::LongText(_) => text,
        _ => false,
    }
}

impl<H: Heap> Vm<H> {
    /// Hidden local `k` of loop `site`.
    fn at(&self, site: &LoopSite, k: usize) -> usize {
        self.base() + usize::from(site.slot) + k
    }

    fn slot(&self, site: &LoopSite, k: usize) -> Value {
        self.cx.stack[self.at(site, k)]
    }

    fn set_slot(&mut self, site: &LoopSite, k: usize, value: Value) {
        let at = self.at(site, k);
        self.cx.stack[at] = value;
    }

    /// The first cursor over a walked value.
    fn first(walked: &Value) -> Value {
        match walked {
            Value::Map(_) | Value::Set(_) => Value::Absent,
            _ => Value::Int(0),
        }
    }

    /// Steps loop `site`'s own cursor: its next item, or `None` exhausted.
    fn step(&mut self, site: &LoopSite) -> Run<Option<Value>> {
        let walked = self.slot(site, WALKED);
        let cursor = self.slot(site, CURSOR);
        let (next, item) = match walked {
            Value::List(items) | Value::Tuple(items) => {
                let Value::Int(position) = cursor else {
                    return Err(fault("a sequence cursor is no position"));
                };
                let item = self
                    .heap
                    .arena()
                    .item(items, position as u64)
                    .map_err(|error| self.native(error))?;
                (Value::Int(position + i64::from(item.is_some())), item)
            }
            Value::Map(table) | Value::Set(table) => {
                let after = match cursor {
                    Value::Int(sequence) => Some(sequence as u64),
                    _ => None,
                };
                let next = self
                    .heap
                    .arena()
                    .table_next(table, after)
                    .map_err(|error| self.native(error))?;
                match next {
                    Some((sequence, key)) => (Value::Int(sequence as i64), Some(key)),
                    None => (cursor, None),
                }
            }
            Value::Text(_) | Value::LongText(_) => {
                let Value::Int(byte) = cursor else {
                    return Err(fault("a text cursor is no offset"));
                };
                let byte = byte as usize;
                let scalar = {
                    let text = self
                        .heap
                        .semantics()
                        .text(&walked)
                        .map(|text| text.get(byte..).and_then(|rest| rest.chars().next()));
                    text.map_err(|error| self.native(error))?
                };
                match scalar {
                    Some(scalar) => {
                        let mut buffer = [0; 4];
                        let item = self.heap.arena_mut().text(scalar.encode_utf8(&mut buffer));
                        (Value::Int((byte + scalar.len_utf8()) as i64), Some(item))
                    }
                    None => (cursor, None),
                }
            }
            _ => return Err(fault("a loop's own cursor steps only what it walks")),
        };
        self.set_slot(site, CURSOR, next);
        Ok(item)
    }

    pub(super) fn for_open(&mut self, unit: &Unit, n: u32) -> Run<()> {
        let site = &unit.loops[n as usize];
        let iterable = self.pop();
        if !walkable(&iterable, false) {
            return Err(self.error("type_error", NOT_ITERABLE));
        }
        self.set_slot(site, WALKED, iterable);
        self.set_slot(site, CURSOR, Self::first(&iterable));
        Ok(())
    }

    pub(super) fn for_next(&mut self, unit: &Unit, n: u32) -> Run<()> {
        let site = &unit.loops[n as usize];
        match self.step(site)? {
            Some(item) => {
                let at = self.base() + usize::from(site.binding);
                self.cx.stack[at] = item;
            }
            None => self.jump(site.exit),
        }
        Ok(())
    }

    pub(super) fn proto_open(&mut self, unit: &Unit, n: u32) -> Run<()> {
        let site = &unit.loops[n as usize];
        let opened = self.pop();
        self.set_slot(site, WALKED, opened);
        self.set_slot(site, CURSOR, Self::first(&opened));
        let size = match opened {
            Value::Map(table) | Value::Set(table) => {
                let len = self
                    .heap
                    .arena()
                    .len(table)
                    .map_err(|error| self.native(error))?;
                counted(len)
            }
            _ => Value::Absent,
        };
        self.set_slot(site, SIZE, size);
        self.set_slot(site, REMAINING, size);
        self.set_slot(site, RAW, Value::Absent);
        self.set_slot(site, MORE, Value::Bool(false));
        Ok(())
    }

    pub(super) fn proto_step(&mut self, unit: &Unit, n: u32) -> Run<()> {
        let site = &unit.loops[n as usize];
        let walked = self.slot(site, WALKED);
        if !walkable(&walked, true) {
            // The header has billed its test: a value with no `next` to
            // step it raises here (`K-ITER-005`).
            let Some(driver) = site.driver else {
                return Err(self.error(
                    "type_error",
                    "the loop's protocol opened a value it has no `next` to step",
                ));
            };
            self.jump(driver);
            return Ok(());
        }
        let item = self.step(site)?;
        self.set_slot(site, MORE, Value::Bool(item.is_some()));
        self.set_slot(site, RAW, item.unwrap_or(Value::Absent));
        Ok(())
    }

    pub(super) fn proto_guard(&mut self, unit: &Unit, n: u32) -> Run<()> {
        let site = &unit.loops[n as usize];
        let walked = self.slot(site, WALKED);
        if !matches!(walked, Value::Map(_) | Value::Set(_)) {
            let join = site
                .join
                .ok_or_else(|| fault("a checked loop has no join"))?;
            self.jump(join);
            return Ok(());
        }
        let size = self.slot(site, SIZE);
        let remaining = self.slot(site, REMAINING);
        let more = self.slot(site, MORE);
        self.push(walked);
        self.push(size);
        self.push(remaining);
        self.push(more);
        Ok(())
    }

    pub(super) fn proto_checked(&mut self, unit: &Unit, n: u32) -> Run<()> {
        let site = &unit.loops[n as usize];
        let budget = self.pop();
        // Any integer, negative or past a word, is the count retained
        // exactly for the next check (`K-ITER-005`).
        if !matches!(budget, Value::Int(_) | Value::BigInt(_)) {
            return Err(self.error(
                "type_error",
                "a loop protocol's `check` gives a count of the items still allowed",
            ));
        }
        self.set_slot(site, REMAINING, budget);
        Ok(())
    }

    pub(super) fn proto_pull(&mut self, unit: &Unit, n: u32) -> Run<()> {
        let site = &unit.loops[n as usize];
        let stepped = self.pop();
        let pulled = match stepped {
            Value::Tuple(tuple) => {
                let arena = self.heap.arena();
                let len = arena.len(tuple).map_err(|error| self.native(error))?;
                match len {
                    0 => Some(None),
                    1 => Some(
                        self.heap
                            .arena()
                            .item(tuple, 0)
                            .map_err(|error| self.native(error))?,
                    ),
                    _ => None,
                }
            }
            _ => None,
        };
        let Some(item) = pulled else {
            return Err(self.error(
                "type_error",
                "a loop protocol's `next` gives an empty tuple or a tuple of one item",
            ));
        };
        self.set_slot(site, MORE, Value::Bool(item.is_some()));
        self.set_slot(site, RAW, item.unwrap_or(Value::Absent));
        Ok(())
    }

    pub(super) fn proto_more(&mut self, unit: &Unit, n: u32) -> Run<()> {
        let site = &unit.loops[n as usize];
        if !matches!(self.slot(site, MORE), Value::Bool(true)) {
            self.jump(site.exit);
        }
        Ok(())
    }
}
