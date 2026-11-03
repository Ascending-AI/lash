//! Heap operations borrow the existing semantic allocation and staged-write
//! handlers. They have no cursor or control stack of their own.

use super::*;
use crate::compile::{Cx, Meter};

mod answer;

/// An accepted operation retains its ordered charges until the caller commits
/// its counted action. A scalar native has one formula; a helper has its body
/// events, which must never be collapsed into that formula's total.
pub(super) struct HeapAnswer {
    pub(super) value: Value,
    events: Option<Vec<Weight>>,
    admission: crate::admission::Accepted<()>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Check {
    None,
    Full,
    Result,
}

impl From<bool> for Check {
    fn from(checked: bool) -> Self {
        if checked { Self::Full } else { Self::None }
    }
}

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    /// Keep the ordered event owner out of the scalar operator's live frame.
    #[inline(never)]
    pub(super) fn operator_heap(
        &mut self,
        site: &OperatorSite,
        args: &[Value],
        (before, counted): (u64, Option<usize>),
    ) -> Answer {
        let Some(answer) = self
            .admits(site)
            .then(|| {
                let deepest = self.exe.lib(site.lib).prepared_depth(self.exe);
                self.heap_answer(
                    site.lib,
                    args,
                    Some(deepest),
                    site.checked.into(),
                    (before, self.accounting),
                    counted,
                )
            })
            .flatten()
        else {
            return Err(self.at_operands(site.pc, site.reg, args));
        };
        self.admit_operator(
            site,
            args,
            answer.value,
            answer.admission.end - self.charged - before,
            (before, counted),
        )?;
        self.commit_heap(answer)
    }

    /// Claim another physical window only while its bytes and the nursery
    /// fit the same fixed entry allowance, before counting its call.
    #[inline(always)]
    pub(super) fn register_room(&self, compiled: &Compiled) -> bool {
        let bytes = compiled.register_bytes().saturating_add(
            if self.memory && compiled.cell_parameters != 0 {
                self.arena.sequence_bound(1) * compiled.cell_parameters as u64
            } else {
                0
            },
        );
        bytes == 0
            || bytes
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
        before: u64,
        strict: bool,
        operation: impl FnOnce(&mut Cx<'_>) -> Option<Value>,
    ) -> Option<HeapAnswer> {
        self.heap_at(None, before, strict, operation)
    }

    pub(super) fn heap_at(
        &mut self,
        occurrence: Option<usize>,
        before: u64,
        strict: bool,
        operation: impl FnOnce(&mut Cx<'_>) -> Option<Value>,
    ) -> Option<HeapAnswer> {
        if self.tight || occurrence.is_some_and(|at| !self.counters.can_advance(&[at])) {
            return None;
        }
        #[cfg(test)]
        if self.scratch.chilled {
            return None;
        }
        self.scratch.arguments.reset();
        let mut cx = Cx {
            native_observer: self
                .projection()
                .map(|watching| watching as &dyn crate::observe::NativeObservation),
            meter: Meter::new(self.tape.is_some()),
            charging: self.charging,
            args: &mut self.scratch.arguments,
            exe: self.exe,
            arena: self.arena,
            sites: self.sites,
            limits: self.limits,
            memory: self.memory,
            ceiling: self.ceiling.saturating_sub(self.register_bytes),
            transient: self.transient,
            fault: None,
            declined: false,
            body_paths: false,
            held: None,
            accumulator: self.accumulator.take(),
            keyed: None,
            absent: self.scratch.absent.take(),
        };
        let answer = operation(&mut cx);
        #[cfg(any(test, feature = "testing"))]
        assert!(
            cx.args.is_empty(),
            "a native exit must release its scratch operands"
        );
        self.accumulator = cx.accumulator.take();
        self.scratch.absent = cx.absent.take();
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
                    } // Heap operations have no compiled source arm to expand.
                      // Decline before publishing a write if one is encountered.
                }
            }
            Some(events)
        } else {
            None
        };
        // Only a body's heap paths stand where the reference pushes a frame;
        // a prepared or native answer charges its whole formula in place.
        let strict = strict && cx.body_paths;
        // Cx cannot change these source bounds. Read them after the arena
        // operation so its physical frame need not retain another window.
        let window = crate::admission::Window::new(self.charged, self.bound, self.slice_end);
        let accepted = answer.and_then(|value| {
            let admission = window.admit(
                (),
                before,
                cx.meter.units,
                None,
                strict && self.slice_end != u64::MAX,
            )?;
            cx.commit_write(&admission).then_some((value, admission))
        });
        self.transient = cx.transient;
        accepted.map(|(value, admission)| HeapAnswer {
            value,
            events,
            admission,
        })
    }

    pub(super) fn heap_answer(
        &mut self,
        lib: crate::compile::LibId,
        args: &[Value],
        deepest: Option<u32>,
        checked: Check,
        (before, strict): (u64, bool),
        occurrence: Option<usize>,
    ) -> Option<HeapAnswer> {
        if let Some(watching) = &self.projection() {
            watching.reset_facts();
        }
        let function = self.exe.lib(lib);
        let signature = &function.header.signature;
        let operator = deepest.is_some();
        if let Some(extra) = deepest
            && function.specialization.prepared.is_some()
        {
            let running = self.base + u64::from(self.depth);
            if (function.operation() && running > self.call_depth)
                || running.saturating_add(u64::from(extra)) > self.call_depth
            {
                return None;
            }
        }
        if checked == Check::Full
            && (!signature.params.iter().zip(args).all(|(param, value)| {
                (param.optional && matches!(value, Value::Absent))
                    || crate::compile::import_result_fits(&param.ty, *value)
            }) || self.arena.any_cold())
        {
            return None;
        }
        if let Some(answer) = self.lookup_answer(function, args, before, occurrence)
            && (checked == Check::None
                || crate::compile::import_result_fits(&signature.result, answer.value))
        {
            return Some(answer);
        }
        let answer = self.heap_at(occurrence, before, strict, |cx| {
            let value = if operator && function.specialization.prepared.is_some() {
                cx.operate_on(function, args, Fresh::default())
            } else {
                cx.native_on(function, args, Fresh::default())
            }?;
            (checked == Check::None || crate::compile::import_result_fits(&signature.result, value))
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

    fn lookup_answer(
        &self,
        function: &crate::compile::Lib,
        args: &[Value],
        before: u64,
        occurrence: Option<usize>,
    ) -> Option<HeapAnswer> {
        let (entry, _) = function.specialization.prepared.as_ref()?;
        use lash_kernel_doc::plan::Element;
        let element = entry.element()?.operation;
        if !matches!(element, Element::ReadInt | Element::ReadMap)
            || OBSERVING
            || self.tight
            || !self.resident_room(0)
        {
            return None;
        }
        #[cfg(test)]
        if self.scratch.chilled {
            return None;
        }
        let value = match (element, args) {
            (Element::ReadInt, [Value::List(list), Value::Int(index)]) => {
                let index = u64::try_from(*index).ok()?;
                if index >= self.arena.len(*list).ok()? {
                    return None;
                }
                self.arena.item(*list, index).ok()??
            }
            (Element::ReadMap, [Value::Map(map), Value::Text(key)]) => {
                self.arena.table_get_text(*map, key).ok()??
            }
            _ => return None,
        };
        if self.arena.summary_of(&value).depth as usize > self.limits.value_depth() {
            return None;
        }
        let units = if self.charging {
            function
                .charge
                .immediate(args, Fresh::default(), &value)
                .or_else(|| {
                    crate::compile::measured_formula(
                        self.arena,
                        &function.charge,
                        args,
                        Some(&value),
                    )
                })?
        } else {
            0
        };
        let admission = self.source_admission(occurrence, before, units, None, false)?;
        Some(HeapAnswer {
            value,
            admission,
            events: self.tape.map(|_| {
                if units == 0 {
                    Vec::new()
                } else {
                    vec![Weight::Atomic(units)]
                }
            }),
        })
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
        let (before, root_occurrence) = self.root_prefix(site.root)?;
        let counted = site.counted.or(root_occurrence);
        if site.act.is_some()
            && self
                .projection()
                .is_some_and(|watching| !watching.can_call())
        {
            return Err(self.at_operands(site.pc, site.reg, args));
        }
        let function = self.exe.lib(site.lib);
        if !self.depth_fits(site.depth)
            || (self.observed
                && (site.act.is_some() || site.root.is_some())
                && matches!(function.run, LibRun::Body(_)))
        {
            return Err(self.at(site.pc));
        }
        let Some(answer) = self.heap_answer(
            site.lib,
            args,
            // The depth its template checks, its prepared entry's deepest.
            Some(match site.depth {
                Depth::Operation(extra) | Depth::Template(extra) => extra,
                Depth::None => 0,
            }),
            site.checked.into(),
            (before, site.root.is_some() && self.accounting),
            counted,
        ) else {
            return Err(self.at(site.pc));
        };
        if let Some(at) = counted {
            self.count(at)?;
        } else if let Some(act) = site.act
            && let Some(watching) = &self.projection()
        {
            watching.action_start(self.exe, &self.stream.nested[act.ix()].site);
        }
        if let Some(root) = site.root
            && let Some(watching) = &self.projection()
        {
            watching.root_call(self.exe, root, args.len());
        }
        self.root_arguments(before);
        self.commit_heap(answer)
    }

    /// Publish only the facts belonging to this accepted operation, after its
    /// action count and all boundary checks have committed.
    pub(super) fn commit_heap(&mut self, answer: HeapAnswer) -> Answer {
        let value = self.source_result(answer.value)?;
        let units = answer.admission.end - self.charged;
        self.charged = answer.admission.end;
        if let Some(tape) = self.tape {
            match answer.events {
                Some(events) => {
                    for weight in events {
                        if weight != Weight::Atomic(0) {
                            tape.push(weight);
                        }
                    }
                }
                None if units != 0 => tape.push(Weight::Atomic(units)),
                None => {}
            }
        }
        Ok(value)
    }

    pub(super) fn source_result(&mut self, value: Value) -> Answer {
        // The source evaluator registers an output's identity when the value
        // enters its operand owner. Delaying this until frame reconstruction
        // changes durable identities when bindings are visited in name order.
        lash_kernel_state::kernel_internals::logical::Holding::from_native(
            self.arena,
            &self.pages,
            &value,
        )
        .map_err(|error| self.halt(error.into()))?;
        Ok(value)
    }

    /// The semantic heap context's room test for an operation that has
    /// no formula, temporary work or staged write. Tapes and observations
    /// retain that context as the reference; tight entries decline there.
    // FIG-7510 profiles: retain fixed room bounds in the hot access callers.
    #[inline(always)]
    pub(super) fn resident_room(&self, bytes: u64) -> bool {
        !self.tight
            && self.tape.is_none()
            && self.projection().is_none()
            && self.storage_room(bytes)
    }

    /// Physical storage eligibility for a prepared source operation. Its
    /// accepted source journal supplies semantic and logical admission.
    pub(super) fn storage_room(&self, bytes: u64) -> bool {
        #[cfg(test)]
        if self.scratch.chilled {
            return false;
        }
        !self.memory
            || bytes
                <= self
                    .ceiling
                    .saturating_sub(self.register_bytes)
                    .saturating_sub(self.arena.held())
                    .saturating_sub(self.transient)
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
}

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(super) fn cell_room(&self, cells: usize) -> bool {
        !self.memory
            || self.arena.sequence_bound(1).saturating_mul(cells as u64)
                <= self
                    .ceiling
                    .saturating_sub(self.arena.held())
                    .saturating_sub(self.register_bytes)
                    .saturating_sub(self.transient)
    }
}
