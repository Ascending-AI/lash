//! Where a bound a run passed is named: the library call the guest made
//! and the authored statement it stands in (`BoundExceeded`).

use std::sync::Arc;

use lash_kernel_value::sites::Site;

use super::{Exceeded, Flow, Stop, Vm};
use crate::Bound;
use crate::bytecode::program::UnitId;
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
        let authored = self.authored_statement(past);
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
                    unit.map(|unit| self.named(unit)),
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

    /// The nearest authored function and the statement it stands in: a
    /// frame below the top stands past its call, the top past its op or,
    /// not `past`, at it.
    fn authored_statement(&self, past: bool) -> Option<(&crate::bytecode::program::Unit, Site)> {
        let top = self.cx.frames.len();
        self.cx
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
            })
    }

    /// Where a raise the running op made stands in document code, for an
    /// uncaught end (`RunError::Uncaught`): the nearest authored
    /// statement, none once a run-root cleanup holds the raise.
    pub(super) fn raise_site(&self) -> Option<Site> {
        if let [root] = self.cx.frames.as_slice()
            && self.cx.inline.is_none()
            && self
                .program
                .unit(root.unit)
                .cleanup
                .is_some_and(|cleanup| root.pc > cleanup.body)
        {
            return None;
        }
        if let Some(site) = self.wait_site() {
            return Some(site);
        }
        self.authored_statement(true).map(|(_, site)| site)
    }

    /// The site of the wait action (`perform`, `join`) the running
    /// authored frame stands past, whose outcome raised: the action's own
    /// expression, not its statement.
    fn wait_site(&self) -> Option<Site> {
        use crate::bytecode::site_tables::ActionKind;
        if self.cx.inline.is_some() {
            return None;
        }
        let frame = self.cx.frames.last()?;
        let unit = self.program.unit(frame.unit);
        let pc = (frame.pc as usize).checked_sub(1)?;
        let crate::bytecode::op::Op::Action(n) = unit.original(pc)? else {
            return None;
        };
        let action = unit.actions.get(n as usize)?;
        match unit.authored && ActionKind::of(action).outcome_raises() {
            true => Some(Site::new(
                unit.site.unit.clone(),
                unit.origin.get(pc)?.to_vec(),
            )),
            false => None,
        }
    }

    /// The function a stop in `unit` names: a spawn's stub is no
    /// function, so a stop there names the function the stub calls.
    fn named(&self, unit: &crate::bytecode::program::Unit) -> Arc<str> {
        use crate::bytecode::program::{Callee, UnitKind};
        let called = match (&unit.kind, unit.calls.first().map(|call| &call.callee)) {
            (UnitKind::Spawn, Some(Callee::Native { native, .. })) => {
                Some(&self.program.native(*native).name)
            }
            (UnitKind::Spawn, Some(Callee::Unit { unit, .. })) => {
                Some(&self.program.unit(*unit).name)
            }
            _ => None,
        };
        Arc::clone(called.unwrap_or(&unit.name))
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

    /// A stop the completion bill of a call of `callee` raised, its frame
    /// gone: a charge bound names the library call that made it, as a
    /// native's does ([`Self::billed_by`]); an authored callee names itself
    /// where it stands.
    pub(super) fn billed_in(&self, flow: Flow, callee: UnitId) -> Flow {
        let unit = self.program.unit(callee);
        match flow {
            Flow::Stop(mut stop) if !unit.authored => {
                if let Stop::Bound(exceeded) = stop.as_mut() {
                    exceeded.function = Some(self.library_call(&self.named(unit)));
                }
                Flow::Stop(stop)
            }
            other => other,
        }
    }

    /// A stop native `native`'s own bill raised: a charge or memory bound
    /// names the library call that made it, not the authored function.
    pub(super) fn billed_by(&self, flow: Flow, native: u32) -> Flow {
        match flow {
            Flow::Stop(mut stop) => {
                if let Stop::Bound(exceeded) = stop.as_mut() {
                    exceeded.function = Some(self.library_call(&self.program.native(native).name));
                }
                Flow::Stop(stop)
            }
            other => other,
        }
    }
}
