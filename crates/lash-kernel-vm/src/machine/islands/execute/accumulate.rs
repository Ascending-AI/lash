//! Binary local accumulation shares the statement's existing semantic owners.

use super::*;

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    #[inline(never)]
    pub(super) fn deferred(
        &mut self,
        frame: &mut Values<'_, 'e>,
        operation: &Deferred,
    ) -> Result<Next, Stop> {
        let (dst, constant, miss) = match operation {
            Deferred::Source(pc) => return Err(self.at(*pc)),
            Deferred::Text {
                dst,
                constant,
                miss,
            } => (*dst, *constant, *miss),
        };
        let crate::compile::Constant::Deferred(lash_kernel_doc::Literal::Text(text)) =
            &self.stream.consts[constant.ix()]
        else {
            unreachable!("a text instruction names a text literal");
        };
        if !self.resident_room(self.arena.text_install_bound(text.len() as u64)) {
            return Err(self.at(miss));
        }
        frame.regs[dst as usize] = self.arena.text(text);
        Ok(Next::Continue)
    }

    /// A certified concatenation of resident texts retains the native's
    /// output work reservation and formula without constructing a call nursery.
    #[inline(always)]
    pub(in crate::machine::islands) fn text_concatenation(
        &mut self,
        site: &OperatorSite,
        values: &[Value],
    ) -> Option<Answer> {
        let [left_value, right_value] = values else {
            return None;
        };
        if !matches!(left_value, Value::Text(_) | Value::LongText(_))
            || !matches!(right_value, Value::Text(_) | Value::LongText(_))
        {
            return None;
        }
        self.resident_concatenation(site, values, site.concat(self.exe)?)
    }

    #[inline(never)]
    fn resident_concatenation(
        &mut self,
        site: &OperatorSite,
        values: &[Value],
        charge: crate::compile::TextCharge,
    ) -> Option<Answer> {
        let [left_value, right_value] = values else {
            return None;
        };
        if OBSERVING
            || !self.swift
            || site.checked
            || site.counted.is_some()
            || site.act.is_some()
            || site.root.is_some()
            || self.deferred_end.is_some()
            || !self.admits(site)
        {
            return None;
        }
        let (left, right) = (left_value, right_value);
        if let (Value::Text(left), Value::Text(right)) = (left, right)
            && let Some(value) = left.concat(right)
        {
            let accepted = self.concatenation_admission(
                site,
                charge,
                left.as_str().len() as u64,
                right.as_str().len() as u64,
                0,
            )?;
            return Some(
                self.commit_admission(accepted, None)
                    .map(|()| Value::Text(value)),
            );
        }
        if self.accumulator.as_ref().is_some_and(
            |accumulator| matches!(left, Value::LongText(handle) if *handle == accumulator.left),
        ) {
            return self
                .text_accumulation(site, Some(charge), values)
                .map(|answer| answer.map(|made| Value::LongText(made.text)));
        }
        let (left, right) = (
            resident_text(self.arena, left)?,
            resident_text(self.arena, right)?,
        );
        let (left_bytes, right_bytes) = (left.len() as u64, right.len() as u64);
        let bytes = left_bytes.checked_add(right_bytes)?;
        if bytes <= lash_kernel_doc::arena::ShortText::MAX as u64 {
            return None;
        }
        let accepted = self.concatenation_admission(
            site,
            charge,
            left_bytes,
            right_bytes,
            self.arena.text_install_bound(bytes).checked_add(bytes)?,
        )?;
        let value = self.arena.concat_text(*left_value, *right_value)?;
        if self.memory {
            self.transient += bytes;
        }
        Some(self.commit_admission(accepted, None).map(|()| value))
    }

    #[inline(always)]
    fn concatenation_admission(
        &self,
        site: &OperatorSite,
        charge: crate::compile::TextCharge,
        left: u64,
        right: u64,
        room: u64,
    ) -> Option<crate::admission::Accepted> {
        self.direct_resources(
            super::super::admission::DepthNeed::Compiled(site.depth),
            room,
        )?;
        self.source_admission(
            None,
            0,
            if self.accounting {
                charge.units(left, right)
            } else {
                0
            },
            self.accounting.then_some(site.close).flatten(),
            false,
        )
    }

    #[inline(always)]
    pub(super) fn accumulate(&mut self, frame: &mut Values<'_, 'e>, at: usize) {
        let left = match frame.slots[at] {
            SlotState::Value(Value::LongText(text)) => Some(text),
            _ => None,
        };
        let mut owned = false;
        frame.state.texts.retain(|(slot, text)| {
            if *slot != at {
                return true;
            }
            owned |= left == Some(*text);
            false
        });
        let owned = owned && left.is_some_and(|text| self.arena.extensible(text));
        self.accumulator = left.map(|left| crate::compile::Accumulator {
            left,
            owned,
            made: None,
        });
    }

    #[inline(always)]
    pub(super) fn operator_destination(
        &mut self,
        frame: &mut Values<'_, 'e>,
        site: &OperatorSite,
        values: &[Value],
        covered: bool,
        to: &Destination,
    ) -> Result<Next, Stop> {
        if let Destination::Write(write) = to
            && self.exe.lib(site.lib).kernels(self.exe).append
        {
            let before = self.transient;
            if let Some(answer) = self.text_accumulation(site, site.concat(self.exe), values) {
                frame.state.transient += self.transient.saturating_sub(before);
                let made = match answer {
                    Ok(made) => {
                        witnessed!(TextAppend, Commit);
                        made
                    }
                    Err(stop) => {
                        witnessed!(TextAppend, Prefix);
                        return Err(stop);
                    }
                };
                frame.state.texts.push((write.at, made.text));
                self.accumulator = None;
                #[cfg(any(test, feature = "perf-witness"))]
                execution::accumulated(&self.scratch.census, self.scratch.counting);
                #[cfg(test)]
                super::super::ACCUMULATED.with(|n| n.set(n.get() + 1));
                return self.destination(frame, site.reg, Value::LongText(made.text), to);
            }
            witnessed!(TextAppend, Guard);
        }
        let value = self.retaining(&mut frame.state.transient, |island| {
            island.operator_value(site, values, covered)
        })?;
        #[cfg(test)]
        if matches!(to, Destination::Write(_)) {
            super::super::ACCUMULATED.with(|n| n.set(n.get() + 1));
        }
        self.destination(frame, site.reg, value, to)
    }

    /// The source-certified one-native text path borrows the same arena
    /// append as Cx. Its rollback owner exists before storage moves, and
    /// every later refusal exits at this operator with that owner intact.
    fn text_accumulation(
        &mut self,
        site: &OperatorSite,
        charge: Option<crate::compile::TextCharge>,
        values: &[Value],
    ) -> Option<Result<crate::compile::Made, Stop>> {
        if OBSERVING || !self.swift || self.tight || self.deferred_end.is_some() {
            return None;
        }
        let [Value::LongText(left), right] = values else {
            return None;
        };
        if !self.accumulator.as_ref().is_some_and(|accumulator| {
            accumulator.owned && accumulator.left == *left && accumulator.made.is_none()
        }) {
            return None;
        }
        let right_bytes = match right {
            Value::Text(text) => text.as_bytes().len() as u64,
            Value::LongText(_) => resident_text(self.arena, right)?.len() as u64,
            _ => return None,
        };
        self.covered = false;
        if let Some(close) = &site.close
            && !self.close(close)
        {
            return Some(Err(self.at_operands(close.head, site.reg, values)));
        }
        let free = self
            .ceiling
            .saturating_sub(self.register_bytes)
            .saturating_sub(self.arena.held())
            .saturating_sub(self.transient);
        let answer = match right {
            Value::Text(part) => {
                super::super::cx::append_text(self.arena, *left, part.as_str(), self.memory, free)
            }
            Value::LongText(other) => {
                super::super::cx::append_long_text(self.arena, *left, *other, self.memory, free)
            }
            _ => return None,
        };
        let appended = match answer {
            Some(Some(appended)) => appended,
            Some(None) => return Some(Err(self.at_operands(site.pc, site.reg, values))),
            None => return Some(Err(self.at_operands(site.pc, site.reg, values))),
        };
        self.transient += appended.held;
        let value = Value::LongText(appended.made.text);
        let Some(accumulator) = self.accumulator.as_mut() else {
            unreachable!("the owned accumulator was matched")
        };
        accumulator.made = Some(appended.made);
        let units = match charge {
            Some(charge) => Some(charge.units(appended.bytes, right_bytes)),
            None => measured_formula(
                self.arena,
                &self.exe.lib(site.lib).charge,
                values,
                Some(&value),
            ),
        };
        let Some(units) = units else {
            return Some(Err(self.at_operands(site.pc, site.reg, values)));
        };
        let units = if self.accounting { units } else { 0 };
        let Some(accepted) = self.source_admission(None, 0, units, None, false) else {
            return Some(Err(self.at_operands(site.pc, site.reg, values)));
        };
        if let Err(stop) = self.commit_admission(accepted, None) {
            return Some(Err(stop));
        }
        if units != 0
            && let Some(tape) = self.tape
        {
            tape.push(Weight::Atomic(units));
        }
        #[cfg(test)]
        super::super::TEXT_APPENDED.with(|n| n.set(n.get() + 1));
        Some(Ok(appended.made))
    }
}

fn resident_text<'a>(arena: &'a Arena, value: &'a Value) -> Option<&'a str> {
    match value {
        Value::Text(text) => Some(text.as_str()),
        Value::LongText(handle) => match arena.body(*handle).ok()? {
            lash_kernel_doc::arena::Body::LongText(text) => match &*text.chunks {
                [lash_kernel_doc::arena::Chunk::Resident(text)] => Some(text.as_str()),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}
