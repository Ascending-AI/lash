//! Ordered formula, branch and helper charges an instruction collects.

use lash_kernel_exec::Weight;

/// What a compiled expression charges as it runs.
pub(crate) struct Meter {
    /// Its formulas' units, and the fixed units of the conditional arms
    /// it took.
    pub(crate) units: u64,
    /// Each formula's units and each arm taken, in order, when a charge
    /// tape records them.
    pub(crate) taken: Vec<Taken>,
    record: bool,
}

/// What a statement's run chose of its static program: a formula's units,
/// or the arm a conditional took.
#[derive(Clone)]
pub(crate) enum Taken {
    Charges(Vec<Weight>),
    Formula(u64),
}

impl Meter {
    pub(crate) fn new(record: bool) -> Self {
        Self {
            units: 0,
            taken: Vec::new(),
            record,
        }
    }

    /// A formula measured on the values it is given, which no maximum
    /// bounds: the sum saturates, so that the bound refuses it.
    #[inline(always)]
    pub(crate) fn measured(&mut self, units: u64) {
        self.units = self.units.saturating_add(units);
        if self.record {
            self.taken.push(Taken::Formula(units));
        }
    }

    pub(crate) fn charges(&mut self, charges: crate::compile::MeteredCharges, exact: bool) {
        let events = charges
            .events
            .into_iter()
            .map(|(units, _)| Weight::Atomic(units))
            .collect::<Vec<_>>();
        let units = if exact {
            events
                .iter()
                .map(|weight| weight.units())
                .fold(0_u64, u64::saturating_add)
        } else {
            charges.pace
        };
        self.units = self.units.saturating_add(units);
        if self.record {
            self.taken.push(Taken::Charges(events));
        }
    }
}
