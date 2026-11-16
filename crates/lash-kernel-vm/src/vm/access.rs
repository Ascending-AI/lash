//! Source-certified resident reads and stores. Every guard is read-only;
//! publication starts only after the original frame, fee and growth fit.

use lash_kernel_doc::Value;
use lash_kernel_state::arena::Key;

use super::{Run, Vm, fault};
use crate::bytecode::program::{Access, CallSite, Callee, Checked, Program, UnitId};
use crate::heap::{Heap, meter};

#[cfg(any(test, feature = "testing"))]
thread_local! {
    static HITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(any(test, feature = "testing"))]
pub(crate) fn hits() -> u64 {
    HITS.with(std::cell::Cell::get)
}

impl<H: Heap> Vm<H> {
    /// Reads only an already resident map. Every miss retains the helper's
    /// read and admission order, without loading anything in the guard.
    fn python_map_read(
        &self,
        map: lash_kernel_doc::arena::ArenaHandle,
        key: Value,
    ) -> Option<Value> {
        let arena = self.heap.arena();
        if self.value_depth <= 1 || arena.entries(map).is_err() {
            return None;
        }
        arena.table_get(map, &key).ok().flatten()
    }

    pub(super) fn call_access(&mut self, program: &Program, site: &CallSite) -> Run<()> {
        let Callee::Resident { unit, charge } = site.callee else {
            unreachable!("a resident call retains its unit")
        };
        let Some(access) = program.unit(unit).access else {
            return Err(fault("a resident call has no source certificate"));
        };
        let argc = usize::from(site.argc);
        if let Some(checked) = &site.checked {
            self.check_arguments(checked, argc)?;
        }
        if self.access(access, unit, charge, argc, site.checked.as_deref())? {
            return Ok(());
        }
        let bill = self.owed(charge, argc);
        self.meter.leave();
        self.exact = true;
        let link = self.running_link();
        self.enter(unit, argc, None, bill, link)
    }

    /// The source's scalar answer still reserves its helper frame and
    /// the original guard body's full frame. Near a boundary the body runs.
    fn python_word_product(&self, callee: UnitId, left: i64, right: i64) -> Option<(Value, u64)> {
        let unit = self.program.unit(callee);
        if self.value_depth <= 1 || !unit.boxed.is_empty() || unit.cleanup.is_some() {
            return None;
        }
        // The certified prefix's first invoke is the shared numeric guard.
        // Library calls have no durable occurrence or public argument check.
        let call = unit.calls.first()?;
        let crate::bytecode::program::Callee::Unit { unit: guard, .. } = call.callee else {
            return None;
        };
        let guard_body = self.program.unit(guard);
        if call.entry.is_some()
            || call.checked.is_some()
            || !guard_body.boxed.is_empty()
            || guard_body.cleanup.is_some()
        {
            return None;
        }
        let value =
            lash_kernel_doc::scalar::ints(lash_kernel_doc::scalar::ScalarOp::Mul, left, right)?;
        Some((value, self.frame_cost(guard)))
    }

    pub(super) fn access(
        &mut self,
        access: Access,
        callee: UnitId,
        charge: Option<u32>,
        argc: usize,
        checked: Option<&Checked>,
    ) -> Run<bool> {
        if self.watch.is_some() || self.sites || self.meter.spent() {
            return Ok(false);
        }
        #[cfg(any(test, feature = "perf-witness"))]
        if self.counting {
            return Ok(false);
        }
        #[cfg(test)]
        if self.chill.is_some() || self.collecting {
            return Ok(false);
        }
        let base = self.cx.stack.len() - argc;
        let args = &self.cx.stack[base..];
        let (value, depth, growth) = match (access, args) {
            (Access::ListRead { depth }, [Value::List(list), Value::Float(index)]) => {
                let index = index.get();
                if index < 0.0
                    || index >= lash_kernel_doc::plan::ELEMENT_POSITIONS as f64
                    || index.fract() != 0.0
                {
                    return Ok(false);
                }
                let arena = self.heap.arena();
                if arena
                    .len(*list)
                    .ok()
                    .and_then(|len| i64::try_from(len).ok())
                    .is_none()
                {
                    return Ok(false);
                }
                let Ok(Some(value)) = arena.item(*list, index as u64) else {
                    return Ok(false);
                };
                // Undefined and holes keep the helper's path. A cold tuple
                // is also a miss until its hole test can be proved.
                if matches!(value, Value::Absent) {
                    return Ok(false);
                }
                if let Value::Tuple(tuple) = value
                    && self.heap.arena().len(tuple).ok().is_none_or(|len| len == 0)
                {
                    return Ok(false);
                }
                (value, depth, 0)
            }
            (Access::WordProduct { depth }, [Value::Int(left), Value::Int(right)]) => {
                let Some((value, guard_frame)) = self.python_word_product(callee, *left, *right)
                else {
                    return Ok(false);
                };
                (value, depth, guard_frame)
            }
            (Access::MapRead { depth, room }, [Value::Map(map), key @ Value::Text(_)]) => {
                let Some(value) = self.python_map_read(*map, *key) else {
                    return Ok(false);
                };
                (value, depth, u64::from(room) * meter::VALUE)
            }
            (
                Access::RecordWrite { key, depth },
                [Value::Record(record), Value::Text(_), value],
            ) => {
                if !matches!(self.heap.arena().table_get_text(*record, &key), Ok(None)) {
                    return Ok(false);
                }
                (*value, depth, 2 * meter::VALUE)
            }
            _ => return Ok(false),
        };
        if self.short_of(depth)
            || self
                .heap
                .held()
                .checked_add(self.frame_cost(callee))
                .and_then(|held| held.checked_add(growth))
                .is_none_or(|held| held > self.heap.bound())
        {
            return Ok(false);
        }
        if let Some(checked) = checked
            && !super::fits::plainly_fits(value, &checked.signature.result)
        {
            return Ok(false);
        }
        let units = match charge.filter(|_| !self.off) {
            Some(charge) => match self.formula(charge, args, Some(&value)) {
                Ok(units) => units,
                Err(_) => return Ok(false),
            },
            None => 0,
        };
        // These source paths have no intermediate charges. With their
        // completion before the slice, none of their steps can suspend.
        if !self.meter.fits_before_slice(units) {
            return Ok(false);
        }
        if matches!(access, Access::RecordWrite { .. }) {
            let [target, key, value] = self.cx.stack[base..] else {
                return Ok(false);
            };
            let (Value::Record(record), Value::Text(text)) = (target, key) else {
                return Ok(false);
            };
            let proof = self.lookups.key(text.word());
            let mut slot = proof.get();
            if slot.is_none() {
                slot = self
                    .heap
                    .arena()
                    .table_read(record, None, &Key::text(&text))
                    .ok()
                    .and_then(|(_, slot)| slot);
                proof.set(slot);
            }
            let written = match slot {
                Some(slot) => slot.write(self.heap.arena_mut(), record, &text, value),
                None => Ok(false),
            };
            match written {
                Ok(true) => self.heap.settle(record, 0),
                Ok(false) => {
                    self.lookups.key(text.word()).set(None);
                    self.set_index(target, key, value)?;
                }
                Err(error) => return Err(self.native(error)),
            }
        }
        self.meter.leave();
        self.exact = true;
        self.cx.stack.truncate(base);
        self.push(value);
        self.fee(units)?;
        #[cfg(any(test, feature = "testing"))]
        HITS.with(|hits| hits.set(hits.get() + 1));
        Ok(true)
    }
}
