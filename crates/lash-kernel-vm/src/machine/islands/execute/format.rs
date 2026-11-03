//! Every source allocation and charge remains real until the ledger supplies
//! its checked publication capability. Internal exits retain their own prefix.

use super::super::compiled::Format;
use super::*;

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    pub(super) fn construct(
        &mut self,
        frame: &mut Values<'_, 'e>,
        site: &sites::Make,
    ) -> Result<Next, Stop> {
        let floor = self.construction_floor(site.make.kind(), site.members);
        self.transient = self.transient.saturating_sub(floor);
        frame.state.transient = frame.state.transient.saturating_sub(floor);
        match self
            .publish_construction(frame, site)
            .map_err(|error| self.halt(error.into()))?
        {
            source::ValuePublication::Published(_) if site.original.continues() => {
                Ok(Next::Continue)
            }
            source::ValuePublication::Published(_) => Err(self.at(site.pc + 1)),
            source::ValuePublication::Declined | source::ValuePublication::Unavailable => {
                Err(self.at(site.miss))
            }
        }
    }

    pub(super) fn construct_record(
        &mut self,
        frame: &mut Values<'_, 'e>,
        site: &sites::Record,
    ) -> Result<Next, Stop> {
        let floor = self.construction_floor(lash_kernel_doc::ValueKind::Record, site.members);
        self.transient = self.transient.saturating_sub(floor);
        frame.state.transient = frame.state.transient.saturating_sub(floor);
        match self
            .publish_record_construction(frame, site)
            .map_err(|error| self.halt(error.into()))?
        {
            source::ValuePublication::Published(_) if site.original.continues() => {
                Ok(Next::Continue)
            }
            source::ValuePublication::Published(_) => Err(self.at(site.pc + 1)),
            source::ValuePublication::Declined | source::ValuePublication::Unavailable => {
                Err(self.at(site.miss))
            }
        }
    }

    pub(super) fn construct_closure(
        &mut self,
        frame: &mut Values<'_, 'e>,
        site: &sites::Closure,
    ) -> Result<Next, Stop> {
        match self
            .publish_closure_construction(frame, site)
            .map_err(|error| self.halt(error.into()))?
        {
            source::ValuePublication::Published(_) if site.original.continues() => {
                Ok(Next::Continue)
            }
            source::ValuePublication::Published(_) => Err(self.at(site.pc + 1)),
            source::ValuePublication::Declined | source::ValuePublication::Unavailable => {
                Err(self.at(site.miss))
            }
        }
    }

    pub(super) fn format(
        &mut self,
        frame: &mut Values<'_, 'e>,
        site: &Format,
    ) -> Result<Next, Stop> {
        self.construct(frame, &site.parts).inspect_err(|_| {
            witnessed!(FormatAppend, Prefix);
        })?;
        if let Some(source) = site.separator.2 {
            if !self.source_constant(
                frame,
                source,
                site.separator.0,
                site.separator.1,
                site.separator_original.as_ref(),
            )? {
                return Err(self.at(source.unpaid()));
            }
        } else {
            frame.regs[site.separator.0 as usize] = site.separator.1;
        }
        let mut args = [Value::Absent; MAX_ARGS];
        for (value, reg) in args.iter_mut().zip(site.join_args) {
            *value = frame.regs[reg as usize];
        }
        let answer = match self
            .publish_native_value(frame, &site.join, &args[..2])
            .map_err(|error| self.halt(error.into()))?
        {
            source::ValuePublication::Published(value) => Ok(value),
            source::ValuePublication::Declined => {
                if let Some(close) = &site.join.close
                    && !self.close(close)
                {
                    return Err(self.at_operands(close.head, site.join.reg, &args[..2]));
                }
                Err(self.at_operands(site.join.pc, site.join.reg, &args[..2]))
            }
            source::ValuePublication::Unavailable => self
                .retaining(&mut frame.state.transient, |island| {
                    island.native(&site.join, &args, 2)
                }),
        };
        let value = match answer {
            Ok(value) => value,
            Err(stop) => {
                witnessed!(FormatAppend, Partial);
                self.spill(frame, &site.joined);
                self.scratch.exit.mapped = true;
                return Err(stop);
            }
        };
        frame.regs[site.join.reg as usize] = value;
        let args = site.append_args.map(|reg| frame.regs[reg as usize]);
        let native = NativeSite {
            lib: site.append.lib,
            close: site.append.close,
            charges: site.append.charges.clone(),
            pc: site.append.pc,
            reg: site.append.reg,
            depth: site.append.depth,
        };
        let answer = match self
            .publish_text_value(frame, &native, &args, false)
            .map_err(|error| self.halt(error.into()))?
        {
            source::ValuePublication::Published(value) => Ok(value),
            source::ValuePublication::Declined | source::ValuePublication::Unavailable => {
                if frame
                    .state
                    .source
                    .as_ref()
                    .is_some_and(|source| source.original.is_some())
                {
                    return Err(self.at_operands(site.append.pc, site.append.reg, &args));
                }
                if let Some(close) = &site.append.close
                    && !self.close(close)
                {
                    return Err(self.at_operands(close.head, site.append.reg, &args));
                }
                Err(self.at_operands(site.append.pc, site.append.reg, &args))
            }
        };
        let value = match answer {
            Ok(value) => value,
            Err(stop) => {
                witnessed!(FormatAppend, Partial);
                self.spill(frame, &site.appending);
                self.scratch.exit.mapped = true;
                return Err(stop);
            }
        };
        frame.regs[site.append.reg as usize] = value;
        // The write may end a legal slice after committing. Its recovery map
        // belongs to this source phase, including moved-text rollback state.
        let answer = self.write_value(frame, &site.write, value);
        if answer.is_err() {
            self.spill(frame, &site.writing);
            self.scratch.exit.mapped = true;
            witnessed!(FormatAppend, Boundary);
        }
        witnessed!(FormatAppend, Commit);
        answer
    }
}
