//! Heap operations borrow the existing semantic allocation and staged-write
//! handlers. They have no cursor or control stack of their own.

use super::*;
use crate::compile::{Cx, Meter};

impl Island<'_> {
    /// Claim another physical window only while its bytes and the nursery
    /// fit the same fixed entry allowance, before counting its call.
    pub(super) fn register_room(&self, compiled: &Compiled) -> bool {
        compiled.register_bytes()
            <= self
                .ceiling
                .saturating_sub(self.arena.held())
                .saturating_sub(self.register_bytes)
                .saturating_sub(self.transient)
    }

    /// The entry's admitted nursery bounds all growth and outstanding work.
    /// A failed operation installs no guest write; installed temporaries are
    /// garbage until the source boundary releases them, as in a loop region.
    pub(super) fn heap(
        &mut self,
        operation: impl FnOnce(&mut Cx<'_>) -> Option<Value>,
    ) -> Option<(Value, u64)> {
        let mut cx = Cx {
            meter: Meter::new(false),
            charging: self.charging,
            args: super::super::registers::Arguments::new(),
            exe: self.exe,
            arena: self.arena,
            sites: self.sites,
            sessions: &mut [],
            written: 0,
            limits: self.limits,
            memory: self.memory,
            ceiling: self.ceiling.saturating_sub(self.register_bytes),
            transient: self.transient,
            fault: None,
            lets: Vec::new(),
            declined: false,
            held: None,
            charged: self.charged,
            counters: None,
            guests: None,
            called: None,
            accumulator: None,
            keyed: None,
            absent: None,
        };
        let answer = operation(&mut cx).filter(|_| {
            self.charged
                .checked_add(cx.meter.units)
                .is_some_and(|end| end <= self.bound)
                && cx.commit_write()
        });
        self.transient = cx.transient;
        answer.map(|value| (value, cx.meter.units))
    }

    pub(super) fn heap_answer(
        &mut self,
        lib: crate::compile::LibId,
        args: &[Value],
        operator: bool,
        checked: bool,
    ) -> Option<(Value, u64)> {
        let function = self.exe.lib(lib);
        let signature = &function.header.signature;
        if operator && let Some((entry, _)) = &function.prepared {
            let extra = entry.deepest(&|id| self.exe.lib(id), &|id| self.exe.library_lib(id));
            let running = self.base + u64::from(self.depth);
            if (function.operation() && running > self.call_depth)
                || running.saturating_add(u64::from(extra)) > self.call_depth
            {
                return None;
            }
        }
        if checked
            && (!signature.params.iter().zip(args).all(|(param, value)| {
                (param.optional && matches!(value, Value::Absent))
                    || crate::compile::import_result_fits(&param.ty, *value)
            }) || self.arena.any_cold())
        {
            return None;
        }
        self.heap(|cx| {
            let value = if operator {
                cx.operate_on(function, args, Fresh::default())
            } else {
                cx.native_on(function, args, Fresh::default())
            }?;
            (!checked || crate::compile::import_result_fits(&signature.result, value))
                .then_some(value)
        })
    }

    pub(super) fn heap_call(&mut self, site: &HeapSite, args: &[Value]) -> Answer {
        if let Some(close) = &site.close
            && !self.close(close)
        {
            return Err(self.at_operands(close.head, site.reg, args));
        }
        let function = self.exe.lib(site.lib);
        if !self.depth_fits(site.depth)
            || (self.observed
                && site.counted.is_some()
                && matches!(function.run, LibRun::DeferredPlan { .. })
                && self.exe.fallback(site.lib).is_none())
            || site
                .counted
                .is_some_and(|at| !self.counters.can_advance(&[at]))
        {
            return Err(self.at(site.pc));
        }
        let Some((value, units)) = self.heap_answer(site.lib, args, true, site.checked) else {
            return Err(self.at(site.pc));
        };
        if let Some(at) = site.counted {
            self.count(at)?;
        }
        self.formula(units);
        Ok(value)
    }

    pub(super) fn construction_floor(
        &self,
        kind: lash_kernel_doc::ValueKind,
        members: usize,
    ) -> u64 {
        if self.memory {
            self.arena.construction_floor(kind, members)
        } else {
            0
        }
    }

    pub(super) fn reserve_construction(
        &mut self,
        kind: lash_kernel_doc::ValueKind,
        members: usize,
    ) -> bool {
        let floor = self.construction_floor(kind, members);
        if floor
            > self
                .ceiling
                .saturating_sub(self.arena.held())
                .saturating_sub(self.register_bytes)
                .saturating_sub(self.transient)
        {
            return false;
        }
        self.transient += floor;
        true
    }

    pub(super) fn make(
        &mut self,
        make: crate::compile::expr::Make,
        values: Vec<Value>,
    ) -> Option<Value> {
        use crate::compile::expr::Make;
        self.heap(|cx| match make {
            Make::Tuple => {
                let bound = cx.arena.sequence_bound(values.len());
                if !cx.fits(bound) {
                    return None;
                }
                cx.arena.tuple(values, cx.limits.value_depth()).ok()
            }
            Make::List => cx.build(lash_kernel_doc::NewObject::List(values)),
            Make::Set => {
                for value in &values {
                    cx.arena.key(value).ok()?;
                }
                cx.build(lash_kernel_doc::NewObject::Set(values))
            }
            Make::Map => {
                let mut entries = Vec::with_capacity(values.len() / 2);
                for pair in values.as_chunks::<2>().0 {
                    cx.arena.key(&pair[0]).ok()?;
                    entries.push((pair[0], pair[1]));
                }
                cx.build(lash_kernel_doc::NewObject::Map(entries))
            }
        })
        .map(|(value, _)| value)
    }

    pub(super) fn record(&mut self, names: &[Value], values: Vec<Value>) -> Option<Value> {
        self.heap(|cx| {
            cx.build(lash_kernel_doc::NewObject::Record(
                names.iter().copied().zip(values).collect(),
            ))
        })
        .map(|(value, _)| value)
    }

    pub(super) fn store(
        &mut self,
        field: Option<usize>,
        target: Value,
        index: Value,
        value: Value,
    ) -> Option<()> {
        let place = match field {
            Some(field) => crate::compile::Place::Field(Value::Text(
                lash_kernel_doc::arena::ShortText::new(&self.stream.fields.get(field)?.name)?,
            )),
            None => crate::compile::Place::Index,
        };
        self.heap(|cx| {
            crate::compile::store(cx, &place, target, index, value)?;
            Some(Value::Null)
        })
        .map(|_| ())
    }
}
