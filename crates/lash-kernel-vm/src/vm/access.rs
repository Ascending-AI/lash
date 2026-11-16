//! Source-certified resident reads and stores. Every guard is read-only;
//! publication starts only after the original frame, fee and growth fit.

use lash_kernel_doc::Value;

use super::{Run, Vm};
use crate::bytecode::program::{Access, Checked, UnitId};
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
            self.set_index(target, key, value)?;
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
