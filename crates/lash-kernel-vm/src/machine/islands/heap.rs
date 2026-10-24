//! Heap operations borrow the existing semantic allocation and staged-write
//! handlers. They have no cursor or control stack of their own.

use super::*;
use crate::compile::{Cx, Meter};

/// An accepted operation retains its ordered charges until the caller commits
/// its counted action. A scalar native has one formula; a helper has its body
/// events, which must never be collapsed into that formula's total.
pub(super) struct HeapAnswer {
    pub(super) value: Value,
    pub(super) units: u64,
    events: Option<Vec<Weight>>,
}

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    /// Keep the ordered event owner out of the scalar operator's live frame.
    #[inline(never)]
    pub(super) fn operator_heap(&mut self, site: &OperatorSite, args: &[Value]) -> Answer {
        let Some(answer) = (self.admits(site)
            && site
                .counted
                .is_none_or(|at| self.counters.can_advance(&[at])))
        .then(|| self.heap_answer(site.lib, args, true, site.checked))
        .flatten() else {
            return Err(self.at_operands(site.pc, site.reg, args));
        };
        self.admit_operator(site, args, answer.value, answer.units)?;
        Ok(self.commit_heap(answer))
    }

    #[inline(never)]
    pub(super) fn native_heap(
        &mut self,
        site: &NativeSite,
        padded: &[Value; MAX_ARGS],
        given: usize,
    ) -> Answer {
        let args = &padded[..given];
        let Some(answer) = self.heap_answer(
            site.lib,
            &padded[..self.exe.lib(site.lib).arity],
            false,
            false,
        ) else {
            return Err(self.at_operands(site.pc, site.reg, args));
        };
        if !self.admit_native(answer.units) {
            return Err(self.at_operands(site.pc, site.reg, args));
        }
        Ok(self.commit_heap(answer))
    }

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
    ) -> Option<HeapAnswer> {
        let mut cx = Cx {
            native_observer: self
                .projection()
                .map(|watching| watching as &dyn crate::observe::NativeObservation),
            meter: Meter::new(self.tape.is_some()),
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
        let mut answer = operation(&mut cx);
        let events = if self.tape.is_some() {
            let mut events = Vec::new();
            for taken in &cx.meter.taken {
                match taken {
                    crate::compile::Taken::Formula(units) => {
                        if *units != 0 {
                            events.push(Weight::Atomic(*units));
                        }
                    }
                    crate::compile::Taken::Charges(charges) => {
                        events.extend(charges.iter().copied())
                    }
                    // Heap operations have no compiled source arm to expand.
                    // Decline before publishing a write if one is encountered.
                    crate::compile::Taken::Arm(_) => answer = None,
                }
            }
            Some(events)
        } else {
            None
        };
        let answer = answer.filter(|_| {
            self.charged
                .checked_add(cx.meter.units)
                .is_some_and(|end| end <= self.bound)
                && cx.commit_write()
        });
        self.transient = cx.transient;
        answer.map(|value| HeapAnswer {
            value,
            units: cx.meter.units,
            events,
        })
    }

    pub(super) fn heap_answer(
        &mut self,
        lib: crate::compile::LibId,
        args: &[Value],
        operator: bool,
        checked: bool,
    ) -> Option<HeapAnswer> {
        if let Some(watching) = &self.projection() {
            watching.reset_facts();
        }
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
        let answer = self.heap(|cx| {
            let value = if operator {
                cx.operate_on(function, args, Fresh::default())
            } else {
                cx.native_on(function, args, Fresh::default())
            }?;
            (!checked || crate::compile::import_result_fits(&signature.result, value))
                .then_some(value)
        });
        if answer.is_some()
            && !matches!(function.run, LibRun::Native { .. })
            && let Some(watching) = &self.projection()
        {
            watching.general_formula(self.charging);
        }
        answer
    }

    pub(super) fn heap_call(&mut self, site: &HeapSite, args: &[Value]) -> Answer {
        if let Some(close) = &site.close
            && !self.close(close)
        {
            return Err(self.at_operands(close.head, site.reg, args));
        }
        self.observed_native(site.lib, |island| island.heap_call_answer(site, args))
    }

    fn heap_call_answer(&mut self, site: &HeapSite, args: &[Value]) -> Answer {
        if site.counted.is_some()
            && self
                .projection()
                .is_some_and(|watching| !watching.can_call())
        {
            return Err(self.at_operands(site.pc, site.reg, args));
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
        let Some(answer) = self.heap_answer(site.lib, args, true, site.checked) else {
            return Err(self.at(site.pc));
        };
        if let Some(at) = site.counted {
            self.count(at)?;
        }
        Ok(self.commit_heap(answer))
    }

    /// Publish only the facts belonging to this accepted operation, after its
    /// action count and all boundary checks have committed.
    pub(super) fn commit_heap(&mut self, answer: HeapAnswer) -> Value {
        self.charged += answer.units;
        if let Some(tape) = self.tape {
            match answer.events {
                Some(events) => {
                    for weight in events {
                        if weight != Weight::Atomic(0) {
                            tape.push(weight);
                        }
                    }
                }
                None if answer.units != 0 => tape.push(Weight::Atomic(answer.units)),
                None => {}
            }
        }
        answer.value
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
        .map(|answer| answer.value)
    }

    pub(super) fn record(&mut self, names: &[Value], values: Vec<Value>) -> Option<Value> {
        self.heap(|cx| {
            cx.build(lash_kernel_doc::NewObject::Record(
                names.iter().copied().zip(values).collect(),
            ))
        })
        .map(|answer| answer.value)
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
