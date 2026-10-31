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
        if (site.replay_guard && (!self.swift || self.tight)) || !self.fits(site.ahead) {
            return Err(self.at(site.miss));
        }
        let before = self.transient;
        let floor = self.construction_floor(site.make.kind(), site.members);
        self.transient = self.transient.saturating_sub(floor);
        frame.state.transient = frame.state.transient.saturating_sub(floor);
        match self
            .publish_construction(frame, site)
            .map_err(|error| self.halt(error.into()))?
        {
            source::ValuePublication::Published(_) => return Ok(Next::Continue),
            source::ValuePublication::Declined => return Err(self.at(site.miss)),
            source::ValuePublication::Unavailable => {}
        }
        let values = site.items.iter().map(|reg| frame.regs[*reg as usize]);
        let answer = self.make(site.make, values);
        frame.state.transient += self.transient.saturating_sub(before);
        let value = answer.ok_or_else(|| self.at(site.miss))?;
        frame.regs[site.reg as usize] = self.source_result(value)?;
        Ok(Next::Continue)
    }

    pub(super) fn construct_record(
        &mut self,
        frame: &mut Values<'_, 'e>,
        site: &sites::Record,
    ) -> Result<Next, Stop> {
        if !self.fits(site.ahead) {
            return Err(self.at(site.miss));
        }
        let before = self.transient;
        let floor = self.construction_floor(lash_kernel_doc::ValueKind::Record, site.members);
        self.transient = self.transient.saturating_sub(floor);
        frame.state.transient = frame.state.transient.saturating_sub(floor);
        match self
            .publish_record_construction(frame, site)
            .map_err(|error| self.halt(error.into()))?
        {
            source::ValuePublication::Published(_) => return Ok(Next::Continue),
            source::ValuePublication::Declined => return Err(self.at(site.miss)),
            source::ValuePublication::Unavailable => {}
        }
        let values = site.values.iter().map(|reg| frame.regs[*reg as usize]);
        let answer = self.record(&site.names, values);
        frame.state.transient += self.transient.saturating_sub(before);
        let value = answer.ok_or_else(|| self.at(site.miss))?;
        frame.regs[site.reg as usize] = self.source_result(value)?;
        Ok(Next::Continue)
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
            source::ValuePublication::Published(_) => return Ok(Next::Continue),
            source::ValuePublication::Declined => return Err(self.at(site.miss)),
            source::ValuePublication::Unavailable => {}
        }
        let closure = self.retaining(&mut frame.state.transient, |island| {
            island.fresh_closure(frame.state.callee, frame.slots, site.code)
        });
        let value = Value::Closure(closure.ok_or_else(|| self.at(site.miss))?);
        frame.regs[site.reg as usize] = self.source_result(value)?;
        Ok(Next::Continue)
    }

    pub(super) fn format(
        &mut self,
        frame: &mut Values<'_, 'e>,
        site: &Format,
    ) -> Result<Next, Stop> {
        self.construct(frame, &site.parts).inspect_err(|_| {
            witnessed!(FormatAppend, Prefix);
        })?;
        frame.regs[site.separator.0 as usize] = site.separator.1;
        let mut args = [Value::Absent; MAX_ARGS];
        for (value, reg) in args.iter_mut().zip(site.join_args) {
            *value = frame.regs[reg as usize];
        }
        let answer = self.retaining(&mut frame.state.transient, |island| {
            island.native(&site.join, &args, 2)
        });
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
        let answer = self.retaining(&mut frame.state.transient, |island| {
            island.operator_value(&site.append, &args, site.covered)
        });
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
