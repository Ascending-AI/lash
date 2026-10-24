//! Ordered formula, branch and helper charges collected by a region statement.

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
    /// The events of its program a charge tape recorded before its guest
    /// call ran, which its commit records on from and clears.
    pub(crate) recorded: usize,
}

/// What a statement's run chose of its static program: a formula's units,
/// or the arm a conditional took.
#[derive(Clone)]
pub(crate) enum Taken {
    Charges(Vec<Weight>),
    Formula(u64),
    Arm(bool),
}

impl Meter {
    pub(crate) fn new(record: bool) -> Self {
        Self {
            units: 0,
            taken: Vec::new(),
            record,
            recorded: 0,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.units = 0;
        self.taken.clear();
    }

    /// A scalar table's formula: its units are bounded, and the plan's
    /// iteration maximum, which admitted them, is a sum that fits.
    #[inline(always)]
    pub(super) fn formula(&mut self, units: u64) {
        self.units += units;
        if self.record {
            self.taken.push(Taken::Formula(units));
        }
    }

    /// A formula measured on the values it is given, which no maximum
    /// bounds: the sum saturates, so that the bound refuses it.
    #[inline(always)]
    pub(in crate::compile::regions) fn measured(&mut self, units: u64) {
        self.units = self.units.saturating_add(units);
        if self.record {
            self.taken.push(Taken::Formula(units));
        }
    }

    pub(in crate::compile::regions) fn charges(
        &mut self,
        charges: crate::compile::MeteredCharges,
        exact: bool,
    ) {
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

    /// A conditional took its arm `taken`, whose fixed units are `fixed`:
    /// its summary's most bounds them with the dearer arm's.
    #[inline(always)]
    pub(super) fn arm(&mut self, taken: bool, fixed: u64) {
        self.units = self.units.saturating_add(fixed);
        if self.record {
            self.taken.push(Taken::Arm(taken));
        }
    }
}
