//! Where a bound a run passed is named: the library call the guest made
//! and the authored statement it stands in (`BoundExceeded`).

use std::sync::Arc;

use super::{Exceeded, Vm};
use crate::Bound;
use crate::heap::Heap;

impl<H: Heap> Vm<H> {
    /// A bound passed by the running op, its frame standing past it.
    pub(super) fn exceeded(&self, bound: Bound, limit: u64) -> Exceeded {
        self.exceeded_at(bound, limit, true)
    }

    /// A bound names the nearest authored function and the statement it
    /// stands in: a library helper the guest called is not where the guest
    /// stands. A frame below the top stands past its call, the top past the
    /// op that passed the bound or, not `past`, at it.
    pub(super) fn exceeded_at(&self, bound: Bound, limit: u64, past: bool) -> Exceeded {
        let top = self.cx.frames.len();
        let authored = self
            .cx
            .frames
            .iter()
            .enumerate()
            .rev()
            .find_map(|(at, frame)| {
                let unit = self.program.unit(frame.unit);
                let pc = match at + 1 == top && !past {
                    true => Some(frame.pc as usize),
                    false => (frame.pc as usize).checked_sub(1),
                };
                unit.authored.then_some(unit).map(|unit| {
                    let site = pc.and_then(|pc| unit.statement(pc));
                    (unit, site.unwrap_or_else(|| unit.site.clone()))
                })
            });
        let (function, site) = match authored {
            Some((unit, site)) => (Some(unit.name.clone()), Some(site)),
            None => {
                let unit = self
                    .cx
                    .inline
                    .map(|activation| activation.callee)
                    .or_else(|| self.cx.frames.last().map(|frame| frame.unit))
                    .map(|unit| self.program.unit(unit));
                (
                    unit.map(|unit| unit.name.clone()),
                    unit.map(|unit| unit.site.clone()),
                )
            }
        };
        Exceeded {
            bound,
            limit,
            function,
            site,
        }
    }

    /// The library call the nearest authored frame made, for a bound
    /// `native` passed: `native` itself where the authored code called it,
    /// else the library function it called, whose body reached `native`.
    pub(super) fn library_call(&self, native: &Arc<str>) -> Arc<str> {
        let frames = &self.cx.frames;
        let authored = frames
            .iter()
            .rposition(|frame| self.program.unit(frame.unit).authored);
        let callee = match authored {
            Some(at) if at + 1 < frames.len() => Some(frames[at + 1].unit),
            Some(_) => self.cx.inline.map(|activation| activation.callee),
            None => None,
        };
        callee.map_or_else(
            || Arc::clone(native),
            |unit| Arc::clone(&self.program.unit(unit).name),
        )
    }
}
