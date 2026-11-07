//! A statement's original charge program, admitted as one statement debt.
use super::*;

/// One statement's debt under its original charge program.
pub(crate) enum StatementDebt<'a> {
    Scalar(ChargeDebt<'a>),
}
impl StatementDebt<'_> {
    pub(crate) fn units(&self) -> Option<u64> {
        match self {
            Self::Scalar(debt) => Some(debt.units()),
        }
    }
    pub(crate) fn matches(&self, source: &Arc<Source>, first: &Point, through: &Point) -> bool {
        match self {
            Self::Scalar(debt) => debt.matches(source, first, through),
        }
    }
    pub(crate) fn record(&self, tape: &crate::observe::ChargeTape) {
        match self {
            Self::Scalar(debt) => debt.record(tape),
        }
    }
}
impl<'a> crate::admission::SourceResources<'a> {
    pub(crate) fn admit_statement<C>(
        self,
        debt: &StatementDebt<'_>,
        occurrences: &[usize],
        depth: u64,
        request: C,
    ) -> Result<Option<crate::admission::SourceAccepted<'a, C>>, doc::heap::HeapFault> {
        let Some(units) = debt.units() else {
            return Ok(None);
        };
        let need = crate::admission::Allowance {
            occurrences: occurrences.to_vec(),
            before: 0,
            units,
            close: None,
            strict: true,
            depth,
            safe_point: self.safe_point,
        };
        let Some(facts) = self.facts(&need) else {
            return Ok(None);
        };
        self.admit_interval(&facts, &need, request)
    }
}
