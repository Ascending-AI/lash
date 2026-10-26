//! Binary local accumulation shares the statement's existing semantic owners.

use super::super::compiled::WriteSite;
use super::*;

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    #[inline(always)]
    pub(super) fn accumulate(&mut self, frame: &mut Frame<'e>, at: usize) {
        let left = match frame.slots[at] {
            SlotState::Value(Value::LongText(text)) => Some(text),
            _ => None,
        };
        let mut owned = false;
        frame.texts.retain(|(slot, text)| {
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
    pub(super) fn accumulation(
        &mut self,
        frame: &mut Frame<'e>,
        site: &OperatorSite,
        append: Option<&super::super::compiled::Append>,
        reads: &[Read],
        covered: bool,
        write: &WriteSite,
    ) -> Result<Next, Stop> {
        if write.accumulates {
            self.accumulate(frame, write.at);
        }
        let values = self.inputs::<3>(&frame.slots, reads)?;
        if let Some(append) = append {
            let before = self.transient;
            if let Some(answer) = self.text_accumulation(site, append.charge, &values[..2]) {
                frame.transient += self.transient.saturating_sub(before);
                let made = answer?;
                frame.slots[write.at] = SlotState::Value(Value::LongText(made.text));
                frame.texts.push((write.at, made.text));
                self.accumulator = None;
                #[cfg(any(test, feature = "perf-witness"))]
                execution::accumulated(&self.scratch.census, self.scratch.counting);
                #[cfg(test)]
                super::super::ACCUMULATED.with(|n| n.set(n.get() + 1));
                return self.finish_statement(frame, write.leaves, None);
            }
        }
        frame.regs[site.reg as usize] = self.retaining(&mut frame.transient, |island| {
            island.operator_value(site, &values[..2], covered)
        })?;
        #[cfg(test)]
        super::super::ACCUMULATED.with(|n| n.set(n.get() + 1));
        let value = frame.regs[site.reg as usize];
        frame.slots[write.at] = SlotState::Value(value);
        if write.accumulates
            && let Some(made) = self
                .accumulator
                .take()
                .and_then(|accumulator| accumulator.made)
        {
            self.accumulated(frame, write.at, value, made)?;
        }
        self.finish_statement(frame, write.leaves, None)
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
        let [Value::LongText(left), Value::Text(part)] = values else {
            return None;
        };
        if !self.accumulator.as_ref().is_some_and(|accumulator| {
            accumulator.owned && accumulator.left == *left && accumulator.made.is_none()
        }) {
            return None;
        }
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
        let appended = match super::super::cx::append_text(
            self.arena,
            *left,
            part.as_str(),
            self.memory,
            free,
        ) {
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
            Some(charge) => Some(charge.units(appended.bytes, part.as_bytes().len() as u64)),
            None => measured_formula(
                self.arena,
                &self.exe.lib(site.lib).charge,
                values,
                Some(&value),
            ),
        };
        match units {
            Some(units) if self.formula(units) => {
                #[cfg(test)]
                super::super::TEXT_APPENDED.with(|n| n.set(n.get() + 1));
                Some(Ok(appended.made))
            }
            _ => Some(Err(self.at_operands(site.pc, site.reg, values))),
        }
    }
}
