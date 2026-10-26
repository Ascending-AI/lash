//! A guarded read runs the path its instructions take through a kind
//! test's true arms whole, once every check they make on that path is
//! proven, and otherwise runs its kind test alone (FIG-7372).

#[cfg(any(test, feature = "perf-witness"))]
use crate::perf_witness::execution::{self, Owner};

use super::super::compiled::{GuardedPath, OperatorSite, Read};
use super::*;

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    /// The guarded read `site`, in place of its head's kind test, whose
    /// statement has begun. Its path charges the units its instructions
    /// charge, in one sum: each kind test's memoized answer, each length or
    /// membership native's formula, each scalar operator's table, each
    /// statement's unit and every close. It runs only where that sum fits
    /// the bound and ends before the slice does, no deferred slice end or
    /// due collection stands, the frame is admitted and plain, and every
    /// operand, answer and branch is the path's: so no check one of those
    /// instructions makes could stop or exit, and nothing it commits
    /// differs. Otherwise the head's kind test runs, and its instructions
    /// from the next; the caches the path consulted are theirs and the
    /// registers it wrote are written again before they are read.
    pub(super) fn guarded(
        &mut self,
        frame: &mut Frame<'e>,
        site: &GuardedPath,
    ) -> Result<Next, Stop> {
        if !OBSERVING
            && self.swift
            && self.deferred_end.is_none()
            && !self.arena.boundary_due()
            && let Some(value) = self.guarded_path(frame, site)
        {
            let write = site.write;
            let (after, leaves) = match &frame.callee.compiled.ops[write].op {
                Op::Write { site: written } => {
                    frame.slots[written.at] = SlotState::Value(value);
                    (write + 1, written.leaves)
                }
                Op::Store { .. } => {
                    let Op::Done { leaves, .. } = frame.callee.compiled.ops[write + 1].op else {
                        unreachable!("a guarded store ends its statement");
                    };
                    (write + 2, leaves)
                }
                _ => unreachable!("a guarded path ends at its write or store"),
            };
            #[cfg(test)]
            super::super::GUARDED_READS.with(|n| n.set(n.get() + 1));
            #[cfg(any(test, feature = "perf-witness"))]
            execution::statements(
                &self.scratch.census,
                Owner::RegisterProgram,
                site.statements,
                self.scratch.counting,
            );
            for block in &site.arms {
                frame.control.push(Position::Block(*block, 1));
            }
            frame.pc = after;
            self.covered = false;
            return self.finish_statement(frame, leaves, None);
        }
        let [value] = self.inputs::<1>(&frame.slots, &site.reads)?;
        frame.regs[site.kind.same.reg as usize] = self
            .retaining(&mut frame.transient, |island| {
                island.kind_test(&site.kind, value)
            })?;
        Ok(Next::Continue)
    }

    /// The value the path writes, its charge committed, where every guard
    /// holds and that charge is proven to fit; `None`, having committed
    /// nothing, otherwise.
    #[inline(always)]
    fn guarded_path(&mut self, frame: &mut Frame<'e>, site: &GuardedPath) -> Option<Value> {
        if let Some(index) = &site.index {
            return self.guarded_index(frame, site, index);
        }
        if let Some(field) = &site.field {
            return self.guarded_field(frame, site, field);
        }
        let ops = &frame.callee.compiled.ops;
        let (slots, regs) = (&frame.slots, &mut frame.regs);
        let mut units =
            site.fixed + self.memo_units(&site.kind, self.pure(slots, &site.reads[0])?, regs)?;
        for at in &site.path {
            match &ops[*at].op {
                Op::Constant { dst, value } => regs[*dst as usize] = *value,
                Op::Local {
                    dst, at, shared, ..
                } => {
                    regs[*dst as usize] = match slots[*at] {
                        SlotState::Value(value) if !shared => value,
                        SlotState::Cell(cell) if *shared => self.arena.cell(cell).ok()?,
                        _ => return None,
                    };
                }
                Op::DirectKind { site, reads } => {
                    units += self.memo_units(site, self.pure(slots, &reads[0])?, regs)?;
                }
                Op::DirectNative { site, reads } => {
                    let mut padded = [Value::Absent; MAX_ARGS];
                    for (to, read) in padded.iter_mut().zip(reads.iter()) {
                        *to = self.pure(slots, read)?;
                    }
                    let (value, charge) = match (site.direct, padded[0]) {
                        (Some(Direct::Length(charge)), Value::List(list)) => {
                            (Value::Int(self.arena.len(list).ok()? as i64), charge)
                        }
                        (Some(Direct::Item(_)), _) => self.item_answer(site, &padded)?,
                        _ => self.membership(site, &padded)?,
                    };
                    units += charge;
                    regs[site.reg as usize] = value;
                }
                Op::Operator { site, args, .. } => {
                    let mut values = [Value::Absent; 3];
                    for (to, reg) in values.iter_mut().zip(args.iter()) {
                        *to = regs[*reg as usize];
                    }
                    let (value, charge) = self.scalar_operator(site, &values[..args.len()])?;
                    units += charge;
                    regs[site.reg as usize] = value;
                }
                Op::DirectOperator { site, reads, .. } => {
                    let mut values = [Value::Absent; 3];
                    for (to, read) in values.iter_mut().zip(reads.iter()) {
                        *to = self.pure(slots, read)?;
                    }
                    let (value, charge) = self.scalar_operator(site, &values[..reads.len()])?;
                    units += charge;
                    regs[site.reg as usize] = value;
                }
                Op::Field { dst, field, .. } => {
                    regs[*dst as usize] = self.field(regs[*dst as usize], *field)?;
                }
                Op::Index { dst, site, .. } => {
                    let (target, at) = (regs[*dst as usize], regs[*dst as usize + 1]);
                    if !matches!(target, Value::List(_) | Value::Tuple(_)) {
                        return None;
                    }
                    regs[*dst as usize] = self.item(*site, target, at)?;
                }
                Op::Branch { reg, .. } => {
                    if !matches!(regs[*reg as usize], Value::Bool(true)) {
                        return None;
                    }
                }
                Op::Store {
                    target,
                    index,
                    value,
                    field,
                    ..
                } => {
                    let end = self.charged.checked_add(units)?;
                    if end > self.bound || end >= self.slice_end {
                        return None;
                    }
                    let (target, index, value) = (
                        regs[*target as usize],
                        index.map_or(Value::Absent, |reg| regs[reg as usize]),
                        regs[*value as usize],
                    );
                    self.retaining(&mut frame.transient, |island| {
                        island.store(*field, target, index, value)
                    })?;
                    self.charged = end;
                    return Some(Value::Null);
                }
                Op::Write { site: write } => {
                    let value = match &write.reads {
                        Some(reads) => self.pure(slots, &reads[0])?,
                        None => regs[write.reg as usize],
                    };
                    let end = self.charged.checked_add(units)?;
                    if end > self.bound
                        || end >= self.slice_end
                        || matches!(slots[write.at], SlotState::Empty)
                    {
                        return None;
                    }
                    self.charged = end;
                    return Some(value);
                }
                _ => unreachable!("a guarded read runs only pure instructions"),
            }
        }
        unreachable!("a guarded read ends at its write")
    }

    /// A length-guarded index read, with its source native and operator
    /// formulas. The resident item is read only after both guards hold.
    fn guarded_index(
        &mut self,
        frame: &mut Frame<'e>,
        site: &GuardedPath,
        index: &super::super::compiled::GuardedIndex,
    ) -> Option<Value> {
        let value = self.pure(&frame.slots, &site.reads[0])?;
        if !site.kind.memo.get(value.kind())?.0 {
            return None;
        }
        let Value::List(list) = value else {
            return None;
        };
        let at = self.pure(&frame.slots, &index.index)?;
        let length = Value::Int(self.arena.len(list).ok()? as i64);
        let ops = &frame.callee.compiled.ops;
        let Op::DirectNative { site: len, .. } = &ops[index.length].op else {
            unreachable!()
        };
        let Some(Direct::Length(native)) = len.direct else {
            unreachable!()
        };
        let Op::Operator { site: test, .. } = &ops[index.test].op else {
            unreachable!()
        };
        let (accepted, tested) = self.scalar_operator(test, &[at, length])?;
        if !matches!(accepted, Value::Bool(true)) {
            return None;
        }
        let value = self
            .arena
            .item(list, crate::compile::position(&at)?)
            .ok()??;
        let units = site.fixed + self.memo_units(&site.kind, Value::List(list), &mut frame.regs)?;
        let Op::Write { site: write } = &ops[site.write].op else {
            unreachable!()
        };
        let end = self
            .charged
            .checked_add(units)?
            .checked_add(native)?
            .checked_add(tested)?;
        if end > self.bound
            || end >= self.slice_end
            || matches!(frame.slots[write.at], SlotState::Empty)
        {
            return None;
        }
        self.charged = end;
        #[cfg(test)]
        super::super::DIRECT_INDICES.with(|n| n.set(n.get() + 1));
        Some(value)
    }

    /// The field shape's guards and charge, read directly from the sites
    /// its producer validated. No intermediate register dispatch remains.
    fn guarded_field(
        &mut self,
        frame: &mut Frame<'e>,
        site: &GuardedPath,
        field: &super::super::compiled::GuardedField,
    ) -> Option<Value> {
        let ops = &frame.callee.compiled.ops;
        let value = self.pure(&frame.slots, &site.reads[0])?;
        let (answer, _, _) = site.kind.memo.get(value.kind())?;
        if !answer {
            return None;
        }
        let units = site.fixed + self.memo_units(&site.kind, value, &mut frame.regs)?;
        let Op::DirectNative { site: member, .. } = &ops[field.member].op else {
            unreachable!()
        };
        let (found, charge) = self.membership(member, &[value, field.key])?;
        let Op::Operator { site: negate, .. } = &ops[field.negate].op else {
            unreachable!()
        };
        let (accepted, negated) = self.scalar_operator(negate, &[found])?;
        if !matches!(accepted, Value::Bool(true)) {
            return None;
        }
        let value = self.field(value, field.field)?;
        let Op::Write { site: write } = &ops[site.write].op else {
            unreachable!()
        };
        let end = self
            .charged
            .checked_add(units)?
            .checked_add(charge)?
            .checked_add(negated)?;
        if end > self.bound
            || end >= self.slice_end
            || matches!(frame.slots[write.at], SlotState::Empty)
        {
            return None;
        }
        self.charged = end;
        #[cfg(test)]
        super::super::DIRECT_FIELDS.with(|n| n.set(n.get() + 1));
        Some(value)
    }

    /// A pure operand's resident value, without an exit.
    #[inline(always)]
    fn pure(&self, slots: &Slots, read: &Read) -> Option<Value> {
        match read {
            Read::Literal { value, .. } => Some(*value),
            Read::Local { at, shared, .. } => match slots[*at] {
                SlotState::Value(value) if !shared => Some(value),
                SlotState::Cell(cell) if *shared => self.arena.cell(cell).ok(),
                _ => None,
            },
        }
    }

    /// A kind test's memoized answer on `x` in its register, and what it
    /// charges with its closes, as [`Self::kind_test`] charges it.
    #[inline(always)]
    fn memo_units(
        &self,
        site: &super::super::compiled::KindTest,
        x: Value,
        regs: &mut [Value],
    ) -> Option<u64> {
        let total = |close: Option<Close>| close.map_or(0, |close| u64::from(close.total));
        let (answer, kind_units, same_units) = site.memo.get(x.kind())?;
        regs[site.same.reg as usize] = Value::Bool(answer);
        Some(total(site.kind.close) + kind_units + total(site.same.close) + same_units)
    }

    /// A scalar operator's answer on `args` and what its formula charges,
    /// as [`Self::operator_value`] answers and charges it on a plain
    /// admitted frame; `None` where it would take another path.
    #[inline(always)]
    fn scalar_operator(&self, site: &OperatorSite, args: &[Value]) -> Option<(Value, u64)> {
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
}
