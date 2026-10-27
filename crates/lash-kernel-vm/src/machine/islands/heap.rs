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
    pub(super) units: u64,
    events: Option<Vec<Weight>>,
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
        let Some(answer) = (self.admits(site)
            && counted.is_none_or(|at| self.counters.can_advance(&[at])))
        .then(|| {
            let (exe, entry) = (self.exe, &site.entry);
            let deepest = *site
                .deepest
                .get_or_init(|| entry.deepest(&|id| exe.lib(id), &|id| exe.library_lib(id)));
            self.heap_answer(
                site.lib,
                args,
                Some(deepest),
                site.checked,
                before,
                site.root.is_some() && self.accounting,
            )
        })
        .flatten() else {
            return Err(self.at_operands(site.pc, site.reg, args));
        };
        self.admit_operator(site, args, answer.value, answer.units, (before, counted))?;
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
            None,
            false,
            0,
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
        if self.tight {
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
        let answer = answer.filter(|_| {
            before
                .checked_add(cx.meter.units)
                .and_then(|total| self.charged.checked_add(total))
                .is_some_and(|end| {
                    end <= self.bound
                        && (!strict || self.slice_end == u64::MAX || end < self.slice_end)
                })
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
        deepest: Option<u32>,
        checked: bool,
        before: u64,
        strict: bool,
    ) -> Option<HeapAnswer> {
        if let Some(watching) = &self.projection() {
            watching.reset_facts();
        }
        let function = self.exe.lib(lib);
        let signature = &function.header.signature;
        let operator = deepest.is_some();
        if let Some(extra) = deepest
            && function.prepared.is_some()
        {
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
        let answer = self.heap(before, strict, |cx| {
            let value = if operator && function.prepared.is_some() {
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
            || counted.is_some_and(|at| !self.counters.can_advance(&[at]))
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
            site.checked,
            before,
            site.root.is_some() && self.accounting,
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

    /// The semantic heap context's room test for an operation that has
    /// no formula, temporary work or staged write. Tapes and observations
    /// retain that context as the reference; tight entries decline there.
    pub(super) fn resident_room(&self, bytes: u64) -> bool {
        #[cfg(test)]
        if self.scratch.chilled {
            return false;
        }
        !self.tight
            && self.tape.is_none()
            && self.projection().is_none()
            && (!self.memory
                || bytes
                    <= self
                        .ceiling
                        .saturating_sub(self.register_bytes)
                        .saturating_sub(self.arena.held())
                        .saturating_sub(self.transient))
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
        values: impl ExactSizeIterator<Item = Value>,
    ) -> Option<Value> {
        use crate::compile::expr::Make;
        // Small argument lists already fit the register executor's operand
        // allowance. Build their inline arena sequence without a temporary Vec.
        if make == Make::List
            && values.len() <= MAX_ARGS
            && self.resident_room(self.arena.sequence_bound(values.len()))
        {
            let count = values.len();
            let mut items = [Value::Absent; MAX_ARGS];
            for (to, value) in items.iter_mut().zip(values) {
                *to = value;
            }
            #[cfg(test)]
            super::DIRECT_COLLECTIONS.with(|n| n.set(n.get() + 1));
            return Some(self.arena.create_list(&items[..count]));
        }
        let values = values.collect::<Vec<_>>();
        if matches!(make, Make::List | Make::Tuple)
            && self.resident_room(self.arena.sequence_bound(values.len()))
        {
            #[cfg(test)]
            super::DIRECT_COLLECTIONS.with(|n| n.set(n.get() + 1));
            return match make {
                Make::Tuple => self.arena.tuple(values, self.limits.value_depth()).ok(),
                Make::List => self
                    .arena
                    .create(lash_kernel_doc::NewObject::List(values))
                    .ok(),
                _ => unreachable!(),
            };
        }
        self.heap(0, false, |cx| match make {
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

    /// The empty tuple [`Self::make`] builds, without an operation's
    /// context: it charges nothing and only needs its room.
    pub(super) fn empty_tuple(&mut self) -> Option<Value> {
        let free = self
            .ceiling
            .saturating_sub(self.register_bytes)
            .saturating_sub(self.arena.held())
            .saturating_sub(self.transient);
        if self.memory && self.arena.sequence_bound(0) > free {
            return None;
        }
        self.arena.tuple(Vec::new(), self.limits.value_depth()).ok()
    }

    pub(super) fn record(
        &mut self,
        names: &[Value],
        values: impl ExactSizeIterator<Item = Value>,
    ) -> Option<Value> {
        if self.resident_room(self.arena.table_bound(values.len())) {
            #[cfg(test)]
            super::DIRECT_COLLECTIONS.with(|n| n.set(n.get() + 1));
            return self
                .arena
                .create(lash_kernel_doc::NewObject::Record(
                    names.iter().copied().zip(values).collect(),
                ))
                .ok();
        }
        self.heap(0, false, |cx| {
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
        if !self.arena.any_cold() {
            match (target, field) {
                (Value::List(list), None)
                    if self.resident_room(lash_kernel_state::arena::VALUE_BYTES) =>
                {
                    let at = crate::compile::position(&index)?;
                    if at > self.arena.len(list).ok()? {
                        return None;
                    }
                    #[cfg(test)]
                    super::DIRECT_COLLECTIONS.with(|n| n.set(n.get() + 1));
                    return self.arena.list_set(list, at, value).ok();
                }
                (Value::Record(record), Some(field))
                    if self.resident_room(2 * lash_kernel_state::arena::VALUE_BYTES) =>
                {
                    let field = self.stream.fields.get(field)?;
                    let key = lash_kernel_doc::arena::ShortText::new(&field.name)?;
                    let slot = field.site.and_then(|site| self.sites.slot(site));
                    if let Some(slot) = slot
                        && crate::compile::write_property(self.arena, record, slot, &key, value)
                            .ok()?
                    {
                        #[cfg(test)]
                        super::DIRECT_COLLECTIONS.with(|n| n.set(n.get() + 1));
                        return Some(());
                    }
                    let (_, found) = find_property(self.arena, record, slot, &field.key).ok()?;
                    if let Some(site) = field.site {
                        self.sites.keep(site, found);
                    }
                    #[cfg(test)]
                    super::DIRECT_COLLECTIONS.with(|n| n.set(n.get() + 1));
                    if let Some(slot) = found
                        && crate::compile::write_property(self.arena, record, slot, &key, value)
                            .ok()?
                    {
                        return Some(());
                    }
                    return self.arena.table_write(record, Value::Text(key), value).ok();
                }
                _ => {}
            }
        }
        let place = match field {
            Some(field) => crate::compile::Place::Field(Value::Text(
                lash_kernel_doc::arena::ShortText::new(&self.stream.fields.get(field)?.name)?,
            )),
            None => crate::compile::Place::Index,
        };
        self.heap(0, false, |cx| {
            crate::compile::store(cx, &place, target, index, value)?;
            Some(Value::Null)
        })
        .map(|_| ())
    }
}

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    /// A cell claims bytes from the same nursery as its register window.
    pub(super) fn fresh_cell(&mut self, value: Value) -> Option<ArenaHandle> {
        self.heap(0, false, |cx| {
            if !cx.fits(cx.arena.sequence_bound(1)) {
                return None;
            }
            Some(Value::ObjectRef(cx.arena.new_cell(value)))
        })
        .and_then(|answer| match answer.value {
            Value::ObjectRef(cell) => Some(cell),
            _ => None,
        })
    }

    /// A closure of `code` over the cells of the variables it shares, made
    /// as the reference makes it; `None`, having made nothing, where a
    /// captured variable has no cell yet or the room does not hold it.
    pub(super) fn fresh_closure(
        &mut self,
        callee: Callee<'_>,
        slots: &Slots,
        code: CodeId,
    ) -> Option<ArenaHandle> {
        let target = self.exe.code(code);
        let lash_kernel_value::sites::Unit::Library(function) = target.site.unit else {
            return None;
        };
        let mut cells = Vec::with_capacity(target.captures.len());
        for capture in &target.captures {
            let cell = match slots.get(capture.outer_at as usize)? {
                SlotState::Cell(cell) => *cell,
                _ if callee.captured != RESIDENT => {
                    let index = callee
                        .plan
                        .captures
                        .iter()
                        .position(|at| *at == capture.outer_at)?;
                    self.scratch.captured[callee.captured as usize + index].1
                }
                _ => return None,
            };
            cells.push(cell);
        }
        let (exe, link) = (self.exe, callee.link);
        let answer = self.heap(0, false, |cx| {
            if !cx.fits(cx.arena.sequence_bound(cells.len())) {
                return None;
            }
            let identity = std::sync::Arc::new(lash_kernel_doc::arena::ClosureIdentity {
                capture_names: target.capture_names.clone(),
                callable: lash_kernel_value::names::CallableId { link, function },
                site: lash_kernel_doc::linked_site::LinkedSite {
                    link,
                    site: target
                        .created
                        .clone()
                        .unwrap_or_else(|| target.site.clone()),
                },
            });
            let closure = cx.arena.new_closure(identity, cells);
            cx.arena
                .found_closure_code(
                    closure,
                    lash_kernel_doc::arena::LocalCode {
                        executable: exe.local(),
                        code: code.0,
                    },
                )
                .ok()?;
            Some(Value::Closure(closure))
        })?;
        match answer.value {
            Value::Closure(closure) => Some(closure),
            _ => None,
        }
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
