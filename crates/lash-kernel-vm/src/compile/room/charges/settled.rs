//! Retain canonical formula measurements after the binding borrow ends.
use super::*;

pub(crate) struct SettledDebt {
    program: Arc<ChargeProgram>,
    formulas: Vec<u64>,
    units: u64,
}
impl ChargeProgram {
    pub(in crate::compile::room) fn through(&self) -> &Point {
        &self.through
    }

    pub(in crate::compile::room) fn belongs_to(&self, source: &Arc<Source>) -> bool {
        Arc::ptr_eq(&self.source, source)
    }
    pub(in crate::compile::room) fn overlaps(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.source, &other.source)
            && self.statement == other.statement
            && self.range.start < other.range.end()
            && other.range.start < self.range.end()
    }
    pub(in crate::compile::room) fn settle(
        self: &Arc<Self>,
        debt: ChargeDebt<'_>,
    ) -> Option<SettledDebt> {
        if !std::ptr::eq(&**self, debt.program) {
            return None;
        }
        Some(SettledDebt {
            program: Arc::clone(self),
            formulas: debt.formulas,
            units: debt.units,
        })
    }
}
impl SettledDebt {
    pub(in crate::compile::room) fn units(&self) -> u64 {
        self.units
    }
    pub(in crate::compile::room) fn record(&self, tape: &crate::observe::ChargeTape) {
        if !self.program.recording {
            return;
        }
        for close in &self.program.closes {
            for event in &close.tape {
                match event {
                    TapeEvent::Fixed(weight) => tape.push(*weight),
                    TapeEvent::Formula(index) => {
                        let units = self.formulas[*index];
                        if units != 0 {
                            tape.push(Weight::Atomic(units));
                        }
                    }
                }
            }
        }
    }
}
