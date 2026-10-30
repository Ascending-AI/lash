//! Fixed-arity scalar operations, with no operand window on their hot path.
//! Only a declining kernel invokes its existing source semantic handler.

use super::super::compiled::{Destination, Operand};
use super::*;

enum ScalarMiss {
    Guard,
    Prefix,
}

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    #[inline(always)]
    pub(super) fn operand(&mut self, frame: &Values<'_, 'e>, arg: &Operand) -> Result<Value, Stop> {
        let (value, miss) = match arg {
            Operand::Register(reg) => return Ok(frame.regs[*reg as usize]),
            Operand::Literal { value, .. } => return Ok(*value),
            Operand::Local { at, miss, .. } => (
                match frame.slots[*at] {
                    SlotState::Value(value) => Some(value),
                    _ => None,
                },
                *miss,
            ),
            Operand::Cell { at, miss, .. } => (
                match frame.slots[*at] {
                    SlotState::Cell(cell) => self.arena.cell(cell).ok(),
                    _ => None,
                },
                *miss,
            ),
        };
        value.ok_or_else(|| self.at(miss))
    }

    #[inline(always)]
    pub(super) fn numeric_unary(
        &mut self,
        frame: &mut Values<'_, 'e>,
        site: &OperatorSite,
        arg: &Operand,
        covered: bool,
        to: &Destination,
    ) -> Result<Next, Stop> {
        let x = self.operand(frame, arg)?;
        let answer = if !OBSERVING && !self.accounting && self.admitted {
            site.entry(self.exe).call(&[x]).map(|value| Ok(Ok(value)))
        } else if !OBSERVING {
            scalar(&x)
                .then(|| site.entry(self.exe).call(&[x]))
                .flatten()
                .map(|value| {
                    let units = if scalar(&value) {
                        site.table(self.exe).units(sizes(&[x], &value))
                    } else {
                        self.unary_units(site, x, value)
                    };
                    self.numeric_charge(site, value, units, covered)
                })
        } else {
            None
        };
        let value = match answer.unwrap_or(Err(ScalarMiss::Guard)) {
            Ok(answer) => {
                let value = match answer {
                    Ok(value) => {
                        witnessed!(Scalar, Commit);
                        value
                    }
                    Err(stop) => {
                        witnessed!(Scalar, Prefix);
                        return Err(stop);
                    }
                };
                if let Some(tape) = self.recorder {
                    tape.typed_packet();
                }
                value
            }
            Err(miss) => {
                match miss {
                    ScalarMiss::Guard => witnessed!(Scalar, Guard),
                    ScalarMiss::Prefix => witnessed!(Scalar, Prefix),
                }
                self.covered = false;
                self.retaining(&mut frame.state.transient, |island| {
                    island.operate(site, &[x])
                })?
            }
        };
        self.destination(frame, site.reg, value, to)
    }

    #[inline(never)]
    fn unary_units(&self, site: &OperatorSite, x: Value, value: Value) -> Option<u64> {
        let function = self.exe.lib(site.lib);
        function.specialization.prepared.as_ref()?;
        function
            .charge
            .immediate(&[x], super::super::cx::Fresh::default(), &value)
    }

    #[inline(always)]
    pub(super) fn numeric_binary(
        &mut self,
        frame: &mut Values<'_, 'e>,
        site: &OperatorSite,
        (left, right): (&Operand, &Operand),
        covered: bool,
        to: &Destination,
    ) -> Result<Next, Stop> {
        let x = self.operand(frame, left)?;
        let y = self.operand(frame, right).inspect_err(|_| {
            self.scratch.exit.regs.push((left.reg(), x));
        })?;
        let answer = if !OBSERVING {
            let typed = site.pair(self.exe).and_then(|kernel| {
                super::super::compiled::typed(site, &kernel, x, y, self.charging, self.exe)
            });
            typed.map(|(value, units)| self.numeric_charge(site, value, units, covered))
        } else {
            None
        };
        let value = match answer.unwrap_or(Err(ScalarMiss::Guard)) {
            Ok(answer) => {
                let value = match answer {
                    Ok(value) => {
                        witnessed!(Scalar, Commit);
                        value
                    }
                    Err(stop) => {
                        witnessed!(Scalar, Prefix);
                        return Err(stop);
                    }
                };
                if let Some(tape) = self.recorder {
                    tape.typed_packet();
                }
                value
            }
            Err(miss) => {
                match miss {
                    ScalarMiss::Guard => witnessed!(Scalar, Guard),
                    ScalarMiss::Prefix => witnessed!(Scalar, Prefix),
                }
                self.covered = false;
                self.retaining(&mut frame.state.transient, |island| {
                    island.operate(site, &[x, y])
                })?
            }
        };
        self.destination(frame, site.reg, value, to)
    }

    #[inline(always)]
    fn numeric_charge(
        &mut self,
        site: &OperatorSite,
        value: Value,
        units: Option<u64>,
        covered: bool,
    ) -> Result<Answer, ScalarMiss> {
        if !self.accounting && self.admitted {
            Ok(Ok(value))
        } else if covered
            && self.covered
            && let Some(units) = units
        {
            self.charged += site.fixed;
            self.charged += units;
            Ok(Ok(value))
        } else {
            match self.typed(site, value, units) {
                Some(answer) => Ok(answer),
                None if units.is_some_and(|units| {
                    site.fixed
                        .checked_add(units)
                        .is_none_or(|units| !self.fits(units))
                }) =>
                {
                    Err(ScalarMiss::Prefix)
                }
                None => Err(ScalarMiss::Guard),
            }
        }
    }

    #[inline(always)]
    pub(super) fn destination(
        &mut self,
        frame: &mut Values<'_, 'e>,
        reg: u32,
        value: Value,
        to: &Destination,
    ) -> Result<Next, Stop> {
        match to {
            Destination::Register => {
                frame.regs[reg as usize] = value;
                Ok(Next::Continue)
            }
            Destination::Write(write) => self.write_value(frame, write, value),
        }
    }

    #[inline(always)]
    pub(super) fn write_value(
        &mut self,
        frame: &mut Values<'_, 'e>,
        site: &super::super::compiled::WriteSite,
        value: Value,
    ) -> Result<Next, Stop> {
        let made = if site.accumulates {
            self.accumulator
                .take()
                .and_then(|accumulator| accumulator.made)
        } else {
            None
        };
        let declined = match self
            .publish_binding(frame, site, value)
            .map_err(|error| self.halt(error.into()))?
        {
            Some(true) => return self.finish_statement(frame, site.leaves, None),
            Some(false) => true,
            None => false,
        };
        if let Some(close) = &site.close
            && !self.close_in(close, site.covered)
        {
            return Err(self.at_held(close.head, site.reg, value));
        }
        if declined {
            return Err(self.at_held(site.pc, site.reg, value));
        }
        if !site.bind && matches!(frame.slots[site.at], SlotState::Empty) {
            return Err(self.at_held(site.pc, site.reg, value));
        }
        if site.shared {
            if site.bind {
                let cell = self
                    .retaining(&mut frame.state.transient, |island| {
                        island.fresh_cell(value)
                    })
                    .ok_or_else(|| self.at_held(site.pc, site.reg, value))?;
                frame.slots[site.at] = SlotState::Cell(cell);
            } else {
                let SlotState::Cell(cell) = frame.slots[site.at] else {
                    return Err(self.at_held(site.pc, site.reg, value));
                };
                self.arena
                    .cell_set(cell, value)
                    .map_err(|_| self.at_held(site.pc, site.reg, value))?;
            }
        } else {
            frame.slots[site.at] = SlotState::Value(value);
        }
        if let Some(made) = made {
            self.accumulated(frame, site.at, value, made)?;
        }
        let answer = self.finish_statement(frame, site.leaves, None);
        #[cfg(any(test, feature = "perf-witness"))]
        if answer.is_err()
            && self.scratch.exit.halt.is_none()
            && self.scratch.exit.reason.is_none()
            && !self.spent()
            && self.fits(1)
        {
            self.scratch.exit.reason = Some(Reason::SourceContinuation);
        }
        answer
    }
}
