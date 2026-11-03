//! Fixed-arity scalar operations, with no operand window on their hot path.
//! Only a declining kernel invokes its existing source semantic handler.

use super::super::compiled::{Destination, Operand};
use super::*;

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    #[inline(always)]
    pub(super) fn operand(
        &mut self,
        frame: &mut Values<'_, 'e>,
        arg: &Operand,
    ) -> Result<Value, Stop> {
        let (value, miss) = match arg {
            Operand::Register(reg) => return Ok(frame.regs[*reg as usize]),
            Operand::Literal { value, .. } => (
                Some(*value),
                arg.source().map_or(0, |source| source.unpaid()),
            ),
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
        let value = value.ok_or_else(|| self.at(miss))?;
        if let Some(source) = arg.source() {
            self.source_read(frame, source, arg.reg(), value)?;
        }
        Ok(value)
    }

    #[inline(always)]
    pub(super) fn numeric_unary(
        &mut self,
        frame: &mut Values<'_, 'e>,
        site: &OperatorSite,
        arg: &Operand,
        _covered: bool,
        to: &Destination,
    ) -> Result<Next, Stop> {
        let x = self.operand(frame, arg)?;
        self.scalar_destination(frame, site, &[x], to)
    }

    #[inline(always)]
    pub(super) fn numeric_binary(
        &mut self,
        frame: &mut Values<'_, 'e>,
        site: &OperatorSite,
        (left, right): (&Operand, &Operand),
        _covered: bool,
        to: &Destination,
    ) -> Result<Next, Stop> {
        let x = self.operand(frame, left)?;
        let y = self.operand(frame, right).inspect_err(|_| {
            self.scratch.exit.regs.push((left.reg(), x));
        })?;
        self.scalar_destination(frame, site, &[x, y], to)
    }

    fn scalar_destination(
        &mut self,
        frame: &mut Values<'_, 'e>,
        site: &OperatorSite,
        args: &[Value],
        to: &Destination,
    ) -> Result<Next, Stop> {
        match self
            .publish_prepared_value(frame, site, args)
            .map_err(|error| self.halt(error.into()))?
        {
            source::ValuePublication::Published(value) => {
                witnessed!(Scalar, Commit);
                if let Some(tape) = self.recorder {
                    tape.typed_packet();
                }
                return self.destination(frame, site.reg, value, to);
            }
            source::ValuePublication::Declined => witnessed!(Scalar, Prefix),
            source::ValuePublication::Unavailable => witnessed!(Scalar, Guard),
        }
        if frame
            .state
            .source
            .as_ref()
            .is_none_or(|source| source.original.is_none())
            && let Some(close) = &site.close
            && !self.close(close)
        {
            return Err(self.at_operands(close.head, site.reg, args));
        }
        Err(self.at_operands(site.pc, site.reg, args))
    }

    pub(super) fn prepared_destination(
        &mut self,
        frame: &mut Values<'_, 'e>,
        site: &OperatorSite,
        args: &[Value],
        to: &Destination,
    ) -> Result<Option<Next>, Stop> {
        let function = self.exe.lib(site.lib);
        let original = frame
            .state
            .source
            .as_ref()
            .is_some_and(|source| source.original.is_some());
        if site.concat(self.exe).is_some() || function.kernels(self.exe).concat.is_some() {
            let native = NativeSite {
                lib: site.lib,
                close: site.close,
                charges: site.charges.clone(),
                pc: site.pc,
                reg: site.reg,
                depth: site.depth,
            };
            match self
                .publish_text_value(frame, &native, args, false)
                .map_err(|error| self.halt(error.into()))?
            {
                source::ValuePublication::Published(value) => {
                    #[cfg(test)]
                    if matches!(to, Destination::Write(_)) {
                        super::super::ACCUMULATED.with(|n| n.set(n.get() + 1));
                    }
                    return self.destination(frame, site.reg, value, to).map(Some);
                }
                miss @ (source::ValuePublication::Declined
                | source::ValuePublication::Unavailable) => {
                    match miss {
                        source::ValuePublication::Unavailable => witnessed!(TextAppend, Guard),
                        _ => witnessed!(TextAppend, Prefix),
                    }
                    if original {
                        return Err(self.at_operands(site.pc, site.reg, args));
                    }
                    if let Some(close) = &site.close
                        && !self.close(close)
                    {
                        return Err(self.at_operands(close.head, site.reg, args));
                    }
                    return Err(self.at_operands(site.pc, site.reg, args));
                }
            }
        }
        match self
            .publish_prepared_value(frame, site, args)
            .map_err(|error| self.halt(error.into()))?
        {
            source::ValuePublication::Published(value) => {
                self.destination(frame, site.reg, value, to).map(Some)
            }
            source::ValuePublication::Declined => {
                if original {
                    return Err(self.at_operands(site.pc, site.reg, args));
                }
                if let Some(close) = &site.close
                    && !self.close(close)
                {
                    return Err(self.at_operands(close.head, site.reg, args));
                }
                Err(self.at_operands(site.pc, site.reg, args))
            }
            source::ValuePublication::Unavailable if original => {
                Err(self.at_operands(site.pc, site.reg, args))
            }
            source::ValuePublication::Unavailable => Ok(None),
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
        if self
            .publish_binding(frame, site, value)
            .map_err(|error| self.halt(error.into()))?
            == Some(true)
        {
            return self.finish_statement(frame, site.leaves, None);
        }
        // A non-admitted write keeps its evaluated RHS and original unpaid
        // close. Only precise source execution may mutate the destination.
        if frame
            .state
            .source
            .as_ref()
            .is_none_or(|source| source.original.is_none())
            && let Some(close) = &site.close
            && !self.close_in(close, site.covered)
        {
            return Err(self.at_held(close.head, site.reg, value));
        }
        Err(self.at_held(site.pc, site.reg, value))
    }
}
