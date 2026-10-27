//! Typed member reads admit their complete source charge before committing.

use super::super::compiled::{AccessShape, AccessSite, OperatorSite, Read, WriteSite};
use super::*;

struct ReadAnswer {
    member: Value,
    consumed: Option<Value>,
    end: u64,
    full: bool,
}

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    /// A pure operand's resident value, without an exit.
    #[inline(always)]
    pub(super) fn pure(&self, slots: &Slots, read: &Read) -> Option<Value> {
        match read {
            Read::Literal { value, .. } => Some(*value),
            Read::Local { at, shared, .. } => match slots[*at] {
                SlotState::Value(value) if !shared => Some(value),
                SlotState::Cell(cell) if *shared => self.arena.cell(cell).ok(),
                _ => None,
            },
        }
    }

    /// A scalar operator's answer on `args` and what its formula charges,
    /// as [`Self::operator_value`] answers and charges it on a plain
    /// admitted frame; `None` where it would take another path.
    #[inline(always)]
    pub(super) fn scalar_operator(
        &self,
        site: &OperatorSite,
        args: &[Value],
    ) -> Option<(Value, u64)> {
        let (value, units) = match args {
            [x] => super::super::compiled::unary(site, *x, self.exe)?,
            [x, y] => match site
                .pair
                .and_then(|pair| super::super::compiled::typed(site, &pair, *x, *y, self.exe, true))
            {
                Some(answer) => answer,
                None if args.iter().all(scalar) => {
                    let result = site.entry.call(args)?;
                    if !scalar(&result) {
                        return None;
                    }
                    (result, site.table.units(sizes(args, &result)))
                }
                None => return None,
            },
            _ => return None,
        };
        Some((value, units?))
    }
    pub(super) fn pure_call_values(
        &self,
        frame: &Frame<'e>,
        args: [&super::super::compiled::Operand; 3],
    ) -> Option<[Value; 3]> {
        use super::super::compiled::Operand;
        let read = |arg: &Operand| match arg {
            Operand::Literal { value, .. } => Some(*value),
            Operand::Register(reg) => Some(frame.regs[*reg as usize]),
            Operand::Local { at, .. } => match frame.slots[*at] {
                SlotState::Value(value) => Some(value),
                _ => None,
            },
            Operand::Cell { at, .. } => match frame.slots[*at] {
                SlotState::Cell(cell) => self.arena.cell(cell).ok(),
                _ => None,
            },
        };
        Some([read(args[0])?, read(args[1])?, read(args[2])?])
    }

    pub(super) fn property_value(
        &mut self,
        statement: Option<StmtId>,
        site: &super::super::compiled::PropertyWrite,
        close: Option<Close>,
        values: &[Value; 3],
    ) -> Option<Answer> {
        if OBSERVING
            || !self.swift
            || self.deferred_end.is_some()
            || self.arena.any_cold()
            || self.base + u64::from(self.depth) + u64::from(site.depth) > self.call_depth
            || !self.resident_room(2 * lash_kernel_state::arena::VALUE_BYTES)
            || site.values & (1 << values[2].kind() as u32) == 0
            || site
                .occurrence
                .is_some_and(|at| !self.counters.can_advance(&[at]))
        {
            return None;
        }
        let statement = statement?;
        let Value::Text(key) = values[1] else {
            return None;
        };
        let units = match &site.charge {
            super::super::compiled::PropertyCharge::Known(units) => *units,
            super::super::compiled::PropertyCharge::Text(charge) => {
                charge.units(key.as_bytes().len() as u64)
            }
        };
        let Value::Record(record) = values[0] else {
            return None;
        };
        let end = self
            .charged
            .checked_add(site.before)?
            .checked_add(close.map_or(0, |close| u64::from(close.total)))?
            .checked_add(units)?;
        if end > self.bound || end >= self.slice_end {
            return None;
        }
        let slot = self.sites.guard(statement);
        let (member, found) = find_property(self.arena, record, slot, &site.guard).ok()?;
        if found != slot {
            self.sites.keep_guard(statement, found);
        }
        if !site.accepts[usize::from(member.is_some())] {
            return None;
        }
        let mut slot = self.sites.write(statement);
        let written = match slot {
            Some(slot) => {
                crate::compile::write_property(self.arena, record, slot, &key, values[2]).ok()?
            }
            None => false,
        };
        if !written {
            let dynamic;
            let lookup = match &site.lookup {
                Some(lookup) => lookup,
                None => {
                    dynamic = lash_kernel_state::arena::Key::of_text(key.as_str());
                    &dynamic
                }
            };
            let (_, found) = find_property(self.arena, record, slot, lookup).ok()?;
            slot = found;
            self.sites.keep_write(statement, slot);
            if !match slot {
                Some(slot) => {
                    crate::compile::write_property(self.arena, record, slot, &key, values[2])
                        .ok()?
                }
                None => false,
            } {
                self.arena
                    .table_write(record, Value::Text(key), values[2])
                    .ok()?;
            }
        }
        if let Some(at) = site.occurrence
            && let Err(stop) = self.count(at)
        {
            return Some(Err(stop));
        }
        self.charged = end;
        self.covered = false;
        Some(Ok(values[2]))
    }

    pub(super) fn access(
        &mut self,
        frame: &mut Frame<'e>,
        site: &AccessSite,
    ) -> Result<Next, Stop> {
        if self.admitted
            && self.charging
            && self.tape.is_none()
            && self.projection().is_none()
            && self.deferred_end.is_none()
            && !self.arena.boundary_due()
            && let Some(answer) = self.access_value(frame, site, 0, false)
        {
            return self.commit_access(frame, site, answer);
        }
        let value = self.inputs::<1>(&frame.slots, std::slice::from_ref(&site.target))?[0];
        frame.regs[site.kind.same.reg as usize] = self
            .retaining(&mut frame.transient, |island| {
                island.kind_test(&site.kind, value)
            })?;
        Ok(Next::Continue)
    }

    pub(super) fn access_init(
        &mut self,
        frame: &mut Frame<'e>,
        head: usize,
        init: &WriteSite,
    ) -> Result<Next, Stop> {
        if let Some(begin) = &init.begin {
            self.begin(frame, begin.id, begin.index, begin.ahead)?;
        }
        let Op::Access(site) = &frame.callee.compiled.ops[head].op else {
            unreachable!()
        };
        let extra = 1 + init.close.map_or(0, |close| u64::from(close.total));
        if self.admitted
            && self.charging
            && self.tape.is_none()
            && self.projection().is_none()
            && self.deferred_end.is_none()
            && !self.arena.boundary_due()
            && let Some(answer) = self.access_value(frame, site, extra, true)
        {
            let Some(Position::Block(_, index)) = frame.control.last_mut() else {
                unreachable!()
            };
            let Some(begin) = &frame.callee.compiled.ops[head].begin else {
                unreachable!("a combined initializer precedes a source statement")
            };
            *index = begin.index + 1;
            #[cfg(any(test, feature = "perf-witness"))]
            execution::statements(
                &self.scratch.census,
                Owner::RegisterProgram,
                1,
                self.scratch.counting,
            );
            return self.commit_access(frame, site, answer);
        }
        let Some(reads) = &init.reads else {
            unreachable!("a combined initializer retains its literal operand")
        };
        let value = self.inputs::<1>(&frame.slots, reads)?[0];
        self.write_value(frame, init, value)
    }

    fn commit_access(
        &mut self,
        frame: &mut Frame<'e>,
        site: &AccessSite,
        answer: ReadAnswer,
    ) -> Result<Next, Stop> {
        self.charged = answer.end;
        frame.slots[site.at] = SlotState::Value(answer.member);
        if let (Some(consumer), Some(value)) = (&site.consumer, answer.consumed) {
            frame.slots[consumer.at] = SlotState::Value(value);
            let Some(Position::Block(_, index)) = frame.control.last_mut() else {
                unreachable!("an access consumer stays in its source block")
            };
            *index = consumer.next;
        }
        let (after, leaves, _statements, arms) = match (&site.short, answer.full) {
            (Some(short), false) => (short.after, short.leaves, short.statements, &short.arms),
            _ => (site.after, site.leaves, site.statements, &site.arms),
        };
        for block in arms {
            frame.control.push(Position::Block(*block, 1));
        }
        frame.pc = after;
        frame.statement = None;
        self.covered = false;
        #[cfg(test)]
        super::super::GUARDED_READS.with(|n| n.set(n.get() + 1));
        #[cfg(any(test, feature = "perf-witness"))]
        execution::statements(
            &self.scratch.census,
            Owner::RegisterProgram,
            _statements,
            self.scratch.counting,
        );
        if leaves == 0 {
            return Ok(Next::Continue);
        }
        self.finish_statement(frame, leaves, None)
    }

    fn access_value(
        &mut self,
        frame: &Frame<'e>,
        site: &AccessSite,
        extra: u64,
        initializing: bool,
    ) -> Option<ReadAnswer> {
        if !initializing && matches!(frame.slots[site.at], SlotState::Empty) {
            return None;
        }
        let target = self.pure(&frame.slots, &site.target)?;
        let (accepted, kind, same) = site.kind.memo.get(target.kind())?;
        if !accepted {
            return None;
        }
        let close = |close: Option<Close>| close.map_or(0, |close| u64::from(close.total));
        let fixed = site.short.as_ref().map_or(site.fixed, |short| short.fixed);
        let mut units = fixed
            .checked_add(extra)?
            .checked_add(kind)?
            .checked_add(same)?
            .checked_add(close(site.kind.kind.close))?
            .checked_add(close(site.kind.same.close))?;
        let value = match &site.shape {
            AccessShape::Length { native, convert } => {
                let Value::List(list) = target else {
                    return None;
                };
                let Op::Operator {
                    site: conversion, ..
                } = &frame.callee.compiled.ops[*convert].op
                else {
                    unreachable!()
                };
                let (value, charged) = self.scalar_operator(
                    conversion,
                    &[Value::Int(self.arena.len(list).ok()? as i64)],
                )?;
                units = units.checked_add(*native)?.checked_add(charged)?;
                value
            }
            AccessShape::Field {
                statement,
                key,
                accepts,
                charges,
                negated,
                name,
            } => {
                let Value::Record(record) = target else {
                    return None;
                };
                let slot = self.sites.guard(*statement);
                let (member, found) = find_property(self.arena, record, slot, key).ok()?;
                if found != slot {
                    self.sites.keep_guard(*statement, found);
                }
                let present = usize::from(member.is_some());
                if !accepts[present] {
                    return None;
                }
                units = units
                    .checked_add(charges[present].units(self.arena.len(record).ok()?))?
                    .checked_add(negated[present])?;
                let value = self.field(target, *name)?;
                #[cfg(test)]
                super::super::DIRECT_FIELDS.with(|n| n.set(n.get() + 1));
                value
            }
            AccessShape::Index {
                index,
                test,
                native,
                validation,
                memo,
            } => {
                let Value::List(list) = target else {
                    return None;
                };
                let at = self.pure(&frame.slots, index)?;
                let length = self.arena.len(list).ok()?;
                if let Some([left, right, compare]) = validation {
                    let scalar = |at: usize, values: &[Value]| {
                        let (Op::DirectOperator { site, .. } | Op::Operator { site, .. }) =
                            &frame.callee.compiled.ops[at].op
                        else {
                            unreachable!()
                        };
                        self.scalar_operator(site, values)
                    };
                    let (x, a) = scalar(*left, &[at])?;
                    let (y, b) = scalar(*right, &[at])?;
                    let (answer, c) = scalar(*compare, &[x, y])?;
                    if !matches!(answer, Value::Bool(true)) {
                        return None;
                    }
                    units = units.checked_add(a)?.checked_add(b)?.checked_add(c)?;
                }
                let tested = match memo
                    .as_ref()
                    .and_then(|memo| memo.get())
                    .filter(|(len, _)| *len == length)
                {
                    Some((_, units)) => *units,
                    None => {
                        let Op::Operator {
                            site: comparison, ..
                        } = &frame.callee.compiled.ops[*test].op
                        else {
                            unreachable!()
                        };
                        let (answer, units) =
                            self.scalar_operator(comparison, &[at, Value::Int(length as i64)])?;
                        if !matches!(answer, Value::Bool(true)) {
                            return None;
                        }
                        if let Some(memo) = memo {
                            let _ = memo.set((length, units));
                        }
                        units
                    }
                };
                units = units.checked_add(*native)?.checked_add(tested)?;
                let value = self
                    .arena
                    .item(list, crate::compile::position(&at)?)
                    .ok()??;
                #[cfg(test)]
                super::super::DIRECT_INDICES.with(|n| n.set(n.get() + 1));
                value
            }
        };
        if let Some(answer) = self.access_tail(frame, site, value, units) {
            return Some(answer);
        }
        let end = self.charged.checked_add(units)?;
        (site.short.is_some() && end <= self.bound && end < self.slice_end).then_some(ReadAnswer {
            member: value,
            consumed: None,
            end,
            full: false,
        })
    }

    fn access_tail(
        &self,
        frame: &Frame<'e>,
        site: &AccessSite,
        value: Value,
        mut units: u64,
    ) -> Option<ReadAnswer> {
        if let Some(short) = &site.short {
            units = units.checked_add(site.fixed - short.fixed)?;
        }
        if let Some(hole) = &site.hole {
            if matches!(value, Value::Tuple(_)) {
                return None;
            }
            if self.memory
                && (self.arena.resident_empty_tuple().is_none()
                    || !self.resident_room(self.arena.sequence_bound(0)))
            {
                return None;
            }
            let (_, fast) = self.exe.lib(hole.lib).prepared.as_ref()?;
            units = units.checked_add(fast.immediate(
                &[value, Value::Absent],
                Fresh::of(0b10),
                &Value::Bool(false),
            )?)?;
        }
        let consumed = if let Some(consumer) = &site.consumer {
            if matches!(frame.slots[consumer.at], SlotState::Empty)
                || (consumer.accumulates && (!frame.texts.is_empty() || self.accumulator.is_some()))
            {
                return None;
            }
            let read = |arg: &Read| {
                if matches!(arg, Read::Local { at, shared: false, .. } if *at == site.at) {
                    Some(value)
                } else {
                    self.pure(&frame.slots, arg)
                }
            };
            let values = [read(&consumer.reads[0])?, read(&consumer.reads[1])?];
            let (Op::Accumulation { site: operator, .. }
            | Op::DirectOperator { site: operator, .. }) =
                &frame.callee.compiled.ops[consumer.operator].op
            else {
                unreachable!()
            };
            let (answer, charged) = self.scalar_operator(operator, &values)?;
            units = units.checked_add(charged)?;
            Some(answer)
        } else {
            None
        };
        let end = self.charged.checked_add(units)?;
        (end <= self.bound && end < self.slice_end).then_some(ReadAnswer {
            member: value,
            consumed,
            end,
            full: true,
        })
    }
}
