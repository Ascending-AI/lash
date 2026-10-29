//! Preserve each completed source prefix, including a read before a refused
//! consumer. All scalar and member semantics stay in their canonical bodies.

use super::super::compiled::{Destination, Update};
use super::*;

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    pub(super) fn update(
        &mut self,
        frame: &mut Values<'_, 'e>,
        site: &Update,
    ) -> Result<Next, Stop> {
        let mut args = [Value::Absent; 2];
        match &site.reads {
            Some(reads) => self.inputs(frame.slots, reads.as_slice(), &mut args)?,
            None => {
                args = [
                    frame.regs[site.args[0] as usize],
                    frame.regs[site.args[1] as usize],
                ]
            }
        }
        let read = if !matches!(frame.slots[site.write.at], SlotState::Empty) {
            self.member_value(&site.read, &args, site.charge)
        } else {
            None
        };
        let value = match read {
            Some(Ok(value)) => value,
            other => {
                witnessed!(ReadUpdate, Guard);
                let mut stop = match other {
                    Some(Err(stop)) => stop,
                    _ => self.at(site.read.close.map_or(site.read.pc, |close| close.head)),
                };
                for (reg, value) in site.args.iter().zip(args) {
                    stop = self.held(stop, *reg, value);
                }
                return Err(stop);
            }
        };
        frame.regs[site.read.reg as usize] = value;
        let answer = self.numeric_binary(
            frame,
            site.pair,
            &site.scalar,
            (&site.left, &site.right),
            site.covered,
            &Destination::Register,
        );
        if let Err(stop) = answer {
            witnessed!(ReadUpdate, Partial);
            self.spill(frame, &site.consumed);
            self.scratch.exit.mapped = true;
            return Err(stop);
        }
        let value = frame.regs[site.scalar.reg as usize];
        frame.slots[site.write.at] = SlotState::Value(value);
        witnessed!(ReadUpdate, Commit);
        let answer = self.finish_statement(frame, site.write.leaves, None);
        if answer.is_err() {
            witnessed!(ReadUpdate, Boundary);
        }
        answer
    }
}
