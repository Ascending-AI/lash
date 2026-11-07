//! The charges one boundary's publication settles.
use super::*;
pub(super) mod sealed {
    pub trait Sealed {}
}
pub(crate) trait BoundaryCharges: sealed::Sealed {
    fn units(&self) -> Option<u64>;
    fn record(&self, tape: &crate::observe::ChargeTape);
}
impl sealed::Sealed for Vec<SettledDebt> {}
impl BoundaryCharges for Vec<SettledDebt> {
    fn units(&self) -> Option<u64> {
        self.iter()
            .try_fold(0u64, |sum, debt| sum.checked_add(debt.units()))
    }
    fn record(&self, tape: &crate::observe::ChargeTape) {
        for debt in self {
            debt.record(tape);
        }
    }
}
