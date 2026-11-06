//! Exact local resource admission from the verified current source transfer.
use super::super::{code::Register, *};
use super::{
    Running,
    flow::{Amount, Balance, Fact, Flow, Root},
    scalar::ScalarAnswer,
};
use lash_kernel_state::kernel_internals::logical::{LeaseMeasure, LeaseOp, LeaseStep, LeaseValue};

pub(super) struct Scratch {
    pub(super) holds: Vec<(LeaseValue, bool)>,
    pub(super) releases: Vec<(LeaseValue, bool)>,
    quote: Vec<(LeaseValue, bool)>,
    quoted: Vec<u64>,
}
impl Scratch {
    pub(super) fn new(width: usize, quotes: usize) -> Result<Self, RegionError> {
        fn bank<T>(width: usize) -> Result<Vec<T>, RegionError> {
            let mut bank = Vec::new();
            bank.try_reserve_exact(width)
                .map_err(|_| invalid(Refused::Scratch))?;
            Ok(bank)
        }
        Ok(Self {
            holds: bank(width)?,
            releases: bank(width)?,
            quote: bank(width)?,
            quoted: bank(quotes)?,
        })
    }
}
fn read(
    registers: &registers::Registers<LeaseValue>,
    frame: usize,
    invocation: u64,
    root: Root,
    view: &mut lash_kernel_state::kernel_internals::logical::StateLease<'_>,
    answer: Option<&ScalarAnswer>,
    destination: Option<Register>,
) -> Result<LeaseValue, RegionError> {
    let value = match answer.and_then(|answer| answer.write) {
        Some((reg, value)) if Some(reg) == destination && reg == root.register => value,
        _ => registers.read(frame, invocation, root.register)?,
    };
    value.bind(view).map_err(RegionError::Heap)
}
pub(super) struct PreparedResources<'a, 's> {
    step: Option<LeaseStep<'a, 's>>,
    counters: execute::Counters,
    session: Option<&'a mut super::running::SessionState>,
}
impl PreparedResources<'_, '_> {
    pub(super) fn accept(self) -> Result<execute::Counters, RegionError> {
        if let Some(step) = self.step {
            step.reserve().map_err(RegionError::Heap)?.accept();
        }
        if let Some(session) = self.session {
            *session = super::running::SessionState {
                held: true,
                written: true,
            };
        }
        Ok(self.counters)
    }
}
impl<'s> Running<'s> {
    /// A native result that stops at its pending-result phase is accepted
    /// there at its output role, beside the still live operands, so the
    /// phase a reopen persists repeats no allocation (Ruling 4). An adopted
    /// answer is balanced by its contract or measure; another is held at its
    /// price. `None` when the result does not fit the memory bound.
    pub(super) fn hold_pending(
        &mut self,
        result: LeaseValue,
        native: Option<(u64, u64, u64)>,
    ) -> Result<Option<execute::Counters>, RegionError> {
        let (retained, peak) = match native {
            Some((retained, peak, _)) => (retained, peak),
            None => {
                let price = self.view.price(result).map_err(RegionError::Heap)?;
                (price, price)
            }
        };
        let balance = self
            .counters
            .balance
            .checked_add(retained)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let peak = self
            .counters
            .balance
            .checked_add(peak)
            .ok_or_else(|| invalid(Refused::Peak))?
            .max(balance)
            .max(self.counters.peak);
        if !super::charge::fits_memory(self.limiters, &mut self.limits, peak) {
            return Ok(None);
        }
        let Scratch {
            holds, releases, ..
        } = &mut self.scratch;
        holds.clear();
        releases.clear();
        holds.push((result, false));
        let op = if native.is_some() {
            LeaseOp::Adopt { holds, releases }
        } else {
            LeaseOp::Transfer { holds, releases }
        };
        let step = self.view.stage(op).map_err(storage_fault)?;
        if step.facts().delta() != i128::from(retained) {
            return Err(invalid(Refused::Facts));
        }
        step.reserve().map_err(RegionError::Heap)?.accept();
        Ok(Some(execute::Counters {
            balance,
            peak,
            ..self.counters
        }))
    }
    pub(super) fn resources<'a>(
        &'a mut self,
        flow: &Flow,
        answer: &ScalarAnswer,
    ) -> Result<Option<PreparedResources<'a, 's>>, RegionError> {
        let invocation = self.reached.invocation;
        let frame = self.frame_index();
        let Scratch {
            holds,
            releases,
            quote,
            quoted,
        } = &mut self.scratch;
        holds.clear();
        releases.clear();
        quoted.clear();
        for &root in &flow.holds {
            let value = read(
                &self.registers,
                frame,
                invocation,
                root,
                &mut self.view,
                Some(answer),
                flow.destination,
            )?;
            holds.push((value, root.capture));
        }
        for &root in &flow.releases {
            let value = read(
                &self.registers,
                frame,
                invocation,
                root,
                &mut self.view,
                None,
                None,
            )?;
            releases.push((value, root.capture));
        }
        if let Some(index) = flow.session {
            let register = flow.destination.ok_or_else(|| invalid(Refused::Facts))?;
            let root = Root {
                register,
                capture: false,
                session: None,
            };
            if self
                .sessions
                .get(index)
                .ok_or_else(|| invalid(Refused::Facts))?
                .held
            {
                let value = read(
                    &self.registers,
                    frame,
                    invocation,
                    root,
                    &mut self.view,
                    None,
                    None,
                )?;
                releases.push((value, false));
            }
            let value = read(
                &self.registers,
                frame,
                invocation,
                root,
                &mut self.view,
                Some(answer),
                flow.destination,
            )?;
            holds.push((value, false));
        }
        for group in &flow.quotes {
            quote.clear();
            let mut price = 0_u64;
            let mut cascade = false;
            for &root in group {
                if root
                    .session
                    .is_some_and(|index| self.sessions.get(index).is_none_or(|cell| !cell.held))
                {
                    continue;
                }
                let value = read(
                    &self.registers,
                    frame,
                    invocation,
                    root,
                    &mut self.view,
                    None,
                    None,
                )?;
                if !root.capture {
                    price = price
                        .checked_add(self.view.price(value).map_err(RegionError::Heap)?)
                        .ok_or_else(|| invalid(Refused::Facts))?;
                }
                cascade |= self
                    .view
                    .inline(value)
                    .map_err(RegionError::Heap)?
                    .is_none();
                quote.push((value, root.capture));
            }
            if cascade {
                let phase = self
                    .view
                    .stage(LeaseOp::Transfer {
                        holds,
                        releases: quote,
                    })
                    .map_err(storage_fault)?;
                price = price
                    .checked_add(
                        phase
                            .facts()
                            .measure(&LeaseMeasure::RELEASED)
                            .ok_or_else(|| invalid(Refused::Facts))?,
                    )
                    .ok_or_else(|| invalid(Refused::Facts))?;
                // Quote only. No prefix or graph edge is accepted here.
                drop(phase);
            }
            quoted.push(price);
        }
        let mut evaluate = |amount: &Amount| -> Result<i128, RegionError> {
            amount
                .terms
                .iter()
                .try_fold(amount.fixed, |sum, (fact, coefficient)| {
                    let value = match fact {
                        Fact::Local(register) => {
                            let value = self
                                .registers
                                .read(frame, invocation, *register)?
                                .bind(&mut self.view)
                                .map_err(RegionError::Heap)?;
                            i128::from(self.view.price(value).map_err(RegionError::Heap)?)
                        }
                        Fact::Result => {
                            let value = answer
                                .write
                                .ok_or_else(|| invalid(Refused::Facts))?
                                .1
                                .bind(&mut self.view)
                                .map_err(RegionError::Heap)?;
                            i128::from(self.view.price(value).map_err(RegionError::Heap)?)
                        }
                        Fact::Native(balance) => match (answer.native, balance) {
                            (Some((delta, _, _)), Balance::Delta) => i128::from(delta),
                            (Some((_, peak, _)), Balance::Peak) => i128::from(peak),
                            (Some((_, _, scratch)), Balance::Scratch) => i128::from(scratch),
                            (None, Balance::Scratch) => 0,
                            (None, Balance::Peak | Balance::Delta) => {
                                let value = answer
                                    .write
                                    .ok_or_else(|| invalid(Refused::Facts))?
                                    .1
                                    .bind(&mut self.view)
                                    .map_err(RegionError::Heap)?;
                                i128::from(self.view.price(value).map_err(RegionError::Heap)?)
                            }
                        },
                        Fact::Release(index, headers) => {
                            i128::from(*quoted.get(*index).ok_or_else(|| invalid(Refused::Facts))?)
                                + i128::from(*headers)
                        }
                        Fact::Zero => 0,
                        Fact::Source(_) => return Err(invalid(Refused::Facts)),
                    };
                    sum.checked_add(
                        value
                            .checked_mul(*coefficient)
                            .ok_or_else(|| invalid(Refused::Facts))?,
                    )
                    .ok_or_else(|| invalid(Refused::Facts))
                })
        };
        let peak = u64::try_from(evaluate(&flow.peak)?).map_err(|_| invalid(Refused::Peak))?;
        let delta = evaluate(&flow.delta)?;
        let balance = i128::from(self.counters.balance)
            .checked_add(delta)
            .and_then(|value| u64::try_from(value).ok())
            .ok_or_else(|| invalid(Refused::Facts))?;
        let peak = self
            .counters
            .balance
            .checked_add(peak)
            .ok_or_else(|| invalid(Refused::Peak))?
            .max(balance)
            .max(self.counters.peak);
        if !super::charge::fits_memory(self.limiters, &mut self.limits, peak) {
            return Ok(None);
        }
        let transfer = match flow.frame_delta {
            0 if answer.native.is_some() => LeaseOp::Adopt { holds, releases },
            0 => LeaseOp::Transfer { holds, releases },
            delta if delta == -i128::from(lash_kernel_doc::LogicalHeldV1::FRAME) => {
                LeaseOp::FrameTransfer {
                    holds,
                    releases,
                    entering: false,
                }
            }
            _ => return Err(invalid(Refused::Facts)),
        };
        let step = self.view.stage(transfer).map_err(storage_fault)?;
        if step.facts().delta() != delta {
            return Err(invalid(Refused::Facts));
        }
        let debt = self
            .counters
            .debt
            .checked_add(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let session = match flow.session {
            Some(index) => Some(
                self.sessions
                    .get_mut(index)
                    .ok_or_else(|| invalid(Refused::Facts))?,
            ),
            None => None,
        };
        Ok(Some(PreparedResources {
            session,
            step: Some(step),
            counters: execute::Counters {
                balance,
                peak,
                debt,
                ..self.counters
            },
        }))
    }
}
