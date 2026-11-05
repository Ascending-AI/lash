//! Exact local resource admission from the verified current source transfer.
use super::super::{code::Register, *};
use super::{
    Running,
    flow::{Amount, Fact, Flow, Root},
    scalar::ScalarAnswer,
};
use lash_kernel_state::kernel_internals::logical::{LeaseMeasure, LeaseOp, LeaseStep, LeaseValue};

pub(super) struct Scratch {
    holds: Vec<(LeaseValue, bool)>,
    releases: Vec<(LeaseValue, bool)>,
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
    invocation: u64,
    root: Root,
    view: &mut lash_kernel_state::kernel_internals::logical::StateLease<'_>,
    answer: Option<&ScalarAnswer>,
    destination: Option<Register>,
) -> Result<LeaseValue, RegionError> {
    let value = match answer.and_then(|answer| answer.write) {
        Some((reg, value)) if Some(reg) == destination && reg == root.register => value,
        _ => registers.read(0, invocation, root.register)?,
    };
    value.bind(view).map_err(RegionError::Heap)
}
pub(super) struct PreparedResources<'a, 's> {
    step: Option<LeaseStep<'a, 's>>,
    counters: execute::Counters,
}
impl PreparedResources<'_, '_> {
    pub(super) fn accept(self) -> Result<execute::Counters, RegionError> {
        if let Some(step) = self.step {
            step.reserve().map_err(RegionError::Heap)?.accept();
        }
        Ok(self.counters)
    }
}
impl<'s> Running<'s> {
    pub(super) fn resources<'a>(
        &'a mut self,
        flow: &Flow,
        answer: &ScalarAnswer,
    ) -> Result<Option<PreparedResources<'a, 's>>, RegionError> {
        let invocation = self.reached.invocation;
        let Scratch {
            holds,
            releases,
            quote,
            quoted,
        } = &mut self.scratch;
        holds.clear();
        releases.clear();
        quoted.clear();
        let mut graph = false;
        for &root in &flow.holds {
            let value = read(
                &self.registers,
                invocation,
                root,
                &mut self.view,
                Some(answer),
                flow.destination,
            )?;
            graph |= self
                .view
                .inline(value)
                .map_err(RegionError::Heap)?
                .is_none();
            holds.push((value, root.capture));
        }
        for &root in &flow.releases {
            let value = read(
                &self.registers,
                invocation,
                root,
                &mut self.view,
                None,
                None,
            )?;
            graph |= self
                .view
                .inline(value)
                .map_err(RegionError::Heap)?
                .is_none();
            releases.push((value, root.capture));
        }
        for group in &flow.quotes {
            quote.clear();
            let mut price = 0_u64;
            let mut cascade = false;
            for &root in group {
                let value = read(
                    &self.registers,
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
                                .read(0, invocation, *register)?
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
                        Fact::Release(index) => {
                            i128::from(*quoted.get(*index).ok_or_else(|| invalid(Refused::Facts))?)
                        }
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
        if peak > self.limits.memory {
            return Ok(None);
        }
        let step = if graph {
            let phase = self
                .view
                .stage(LeaseOp::Transfer { holds, releases })
                .map_err(storage_fault)?;
            if phase.facts().delta() != delta {
                return Err(invalid(Refused::Facts));
            }
            Some(phase)
        } else {
            None
        };
        let debt = self
            .counters
            .debt
            .checked_add(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        Ok(Some(PreparedResources {
            step,
            counters: execute::Counters {
                balance,
                peak,
                debt,
                ..self.counters
            },
        }))
    }
}
