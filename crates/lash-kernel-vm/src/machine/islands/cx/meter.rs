//! Ordered formula, branch and helper charges an instruction collects.

use lash_kernel_exec::Weight;

/// What a compiled expression charges as it runs.
pub(crate) struct Meter {
    /// Its formulas' units, and the fixed units of the conditional arms
    /// it took.
    pub(crate) units: u64,
    /// Each formula's units and each arm taken, in order, when a charge
    /// tape records them.
    pub(crate) taken: Vec<Weight>,
    record: bool,
}

impl Meter {
    pub(super) fn recording(&self) -> bool {
        self.record
    }

    pub(crate) fn new(record: bool) -> Self {
        Self {
            units: 0,
            taken: Vec::new(),
            record,
        }
    }

    /// Keep a helper's final fee after its body events without building a
    /// temporary event vector when no charge tape is being recorded.
    pub(crate) fn charges(
        &mut self,
        charges: crate::compile::MeteredCharges,
        exact: bool,
        final_fee: Option<u64>,
    ) {
        let units = if exact {
            charges
                .measurement
                .units
                .saturating_add(final_fee.unwrap_or(0))
        } else {
            charges.measurement.pace
        };
        self.units = self.units.saturating_add(units);
        if self.record {
            self.taken.extend(
                charges
                    .events
                    .into_iter()
                    .map(|event| event.units)
                    .chain(final_fee)
                    .map(Weight::Atomic),
            );
        }
    }
}
