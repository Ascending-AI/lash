//! Ordered source charges retain the unit or atomic event crossing the bound.
use super::super::*;
use crate::compile::room::charges::region_isa::{CanonicalClose, ChargeItem, ChargeMoment};
use lash_kernel_exec::Weight;

pub(in crate::machine::region_isa) struct Crossing {
    bound: u64,
    charged: u64,
}
impl Crossing {
    pub(super) fn restore(bound: u64, charged: u64) -> Result<Self, RegionError> {
        if charged <= bound {
            return Err(invalid(Refused::Facts));
        }
        Ok(Self { bound, charged })
    }
    pub(in crate::machine::region_isa) fn authenticates(&self, bound: u64, charged: u64) -> bool {
        self.bound == bound && self.charged == charged && charged > bound
    }
}
type Limiters = crate::machine::limits::Limiters;

/// Bills under the run's charge limit. At a crossing the charge limiter
/// decides (FIG-6768), asked at the exact unit that crossed; a grown limit
/// bills the same events again under it.
pub(super) fn billed(
    limiters: &mut Limiters,
    limits: &mut Limits,
    bill: impl Fn(Limits) -> Result<Payment, RegionError>,
) -> Result<Payment, RegionError> {
    loop {
        let payment = bill(*limits)?;
        let Some(crossing) = &payment.crossing else {
            return Ok(payment);
        };
        let mut limit = limits.charge;
        if limiters
            .charge_reached(crossing.charged, &mut limit)
            .is_err()
        {
            return Ok(payment);
        }
        limits.charge = limit;
    }
}

/// Whether a phase reaching `peak` fits the memory bound, the memory limiter
/// deciding where it does not.
pub(super) fn fits_memory(limiters: &mut Limiters, limits: &mut Limits, peak: u64) -> bool {
    limiters.observe_peak(peak);
    if peak <= limits.memory {
        return true;
    }
    let mut limit = limits.memory;
    if limiters.memory_reached(peak, &mut limit).is_err() {
        return false;
    }
    limits.memory = limit;
    true
}

pub(super) struct Payment {
    pub(super) counters: Counters,
    pub(super) crossing: Option<Crossing>,
    #[cfg(any(test, feature = "testing"))]
    pub(super) events: Vec<Weight>,
}

fn weight(payment: &mut Payment, limits: Limits, weight: Weight) -> bool {
    let units = weight.units();
    let billed = if limits.charge_enabled {
        match weight {
            Weight::Units(_) => units.min(
                limits
                    .charge
                    .saturating_sub(payment.counters.charge)
                    .saturating_add(1),
            ),
            Weight::Atomic(_) => units,
        }
    } else {
        units
    };
    #[cfg(any(test, feature = "testing"))]
    if limits.charge_enabled && limits.record_charges {
        payment.events.push(match weight {
            Weight::Units(_) => Weight::Units(billed),
            Weight::Atomic(_) => Weight::Atomic(billed),
        });
    }
    payment.counters.progress = payment.counters.progress.saturating_add(billed);
    if limits.charge_enabled {
        payment.counters.charge = payment.counters.charge.saturating_add(billed);
        if payment.counters.charge > limits.charge {
            payment.crossing = Some(Crossing {
                bound: limits.charge,
                charged: payment.counters.charge,
            });
            return false;
        }
    }
    true
}

/// K-CHG-005: copying an explicit root Return uses one atomic deep-size fee.
/// The caller derives this size from the actual lease-owned result.
pub(super) fn export(counters: Counters, limits: Limits, units: u64) -> Payment {
    let mut payment = Payment {
        counters,
        crossing: None,
        #[cfg(any(test, feature = "testing"))]
        events: Vec::new(),
    };
    weight(&mut payment, limits, Weight::Atomic(units));
    payment
}

pub(super) fn bill(
    counters: Counters,
    limits: Limits,
    closes: &[CanonicalClose],
    moment: ChargeMoment,
    formula: Option<u64>,
) -> Result<Payment, RegionError> {
    let mut payment = Payment {
        counters,
        crossing: None,
        #[cfg(any(test, feature = "testing"))]
        events: Vec::new(),
    };
    for close in closes.iter().filter(|close| close.moment == moment) {
        for item in &close.tape {
            let event = match item {
                ChargeItem::Fixed { weight, .. } => *weight,
                ChargeItem::Formula { .. } if !limits.charge_enabled => continue,
                ChargeItem::Formula { .. } => {
                    Weight::Atomic(formula.ok_or_else(|| invalid(Refused::Facts))?)
                }
            };
            if !weight(&mut payment, limits, event) {
                return Ok(payment);
            }
        }
    }
    Ok(payment)
}

impl super::Running<'_> {
    pub(super) fn observe_payment(&mut self, payment: &Payment) {
        #[cfg(any(test, feature = "testing"))]
        self.observed_charges.extend_from_slice(&payment.events);
        #[cfg(not(any(test, feature = "testing")))]
        let _ = payment;
    }
}

#[cfg(test)]
mod laws {
    use super::*;
    use lash_kernel_exec::ReferenceCharger;

    /// K-CHG-002: refusal retains the crossing event, and no later event.
    #[test]
    fn tight_charge_prefix_matches_the_per_unit_source_oracle() {
        for entry in [0, 7, u64::MAX - 4, u64::MAX] {
            for allowance in [0, 1, 3, 4, 5, 17, u64::MAX] {
                let bound = entry.saturating_add(allowance);
                for program in [
                    [Weight::Units(4), Weight::Atomic(9), Weight::Units(2)],
                    [Weight::Atomic(4), Weight::Units(9), Weight::Atomic(2)],
                    [Weight::Units(0), Weight::Atomic(0), Weight::Units(5)],
                ] {
                    let limits = Limits {
                        charge: bound,
                        memory: u64::MAX,
                        depth: 8,
                        slice: u64::MAX,
                        entry_balance: 0,
                        entry_charge: entry,
                        entry_progress: entry,
                        charge_enabled: true,
                        entry_depth: 1,
                        record_charges: true,
                    };
                    let mut actual = Payment {
                        counters: Counters {
                            charge: entry,
                            progress: entry,
                            balance: 0,
                            peak: 0,
                            depth: 1,
                            debt: 0,
                        },
                        crossing: None,
                        #[cfg(any(test, feature = "testing"))]
                        events: Vec::new(),
                    };
                    let mut source = ReferenceCharger::new(entry, bound);
                    for event in program {
                        let expected = source.charge(event);
                        let accepted = weight(&mut actual, limits, event);
                        assert_eq!(accepted, expected.is_ok());
                        assert_eq!(actual.counters.charge, source.charged());
                        assert_eq!(actual.counters.progress, source.charged());
                        if expected.is_err() {
                            assert!(
                                actual
                                    .crossing
                                    .as_ref()
                                    .unwrap()
                                    .authenticates(bound, source.charged())
                            );
                            break;
                        }
                    }
                }
            }
        }
    }
}
