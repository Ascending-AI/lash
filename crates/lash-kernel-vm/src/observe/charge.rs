//! The charge event program (`K-CHG-001`..`K-CHG-008`) and its oracle.
//!
//! A run's charge is the ordered sequence of its semantic charge events.
//! Passing the bound ends the run at the event that passes it, charged
//! with that event:
//!
//! - [`Weight`]: how one charge meets the bound: a run of one-unit events,
//!   which stops at the first unit that passes, or one atomic event (an
//!   operation formula, a construction's members, a deep size), charged
//!   whole;
//! - [`ReferenceCharger`]: the oracle. It charges one unit at a time, with
//!   saturating addition and the bound's strict `>`.

/// How one charge meets the bound (`K-CHG-002`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Weight {
    /// This many one-unit events, in order: a run that passes its bound
    /// inside them is charged up to the unit that passes.
    Units(u64),
    /// One event of this many units, charged whole: a formula charges
    /// atomically (`K-CHG-003`).
    Atomic(u64),
}

impl Weight {
    /// How many units the charge is.
    pub const fn units(self) -> u64 {
        match self {
            Self::Units(units) | Self::Atomic(units) => units,
        }
    }

    /// The program a bound can tell apart: every unit run as one-unit
    /// events, an atomic event of one unit as a unit, and no zero-unit
    /// event, which no bound sees.
    pub fn expand(program: impl IntoIterator<Item = Self>) -> Vec<Self> {
        let mut out = Vec::new();
        for weight in program {
            match weight {
                Self::Units(units) => {
                    out.extend((0..units).map(|_| Self::Units(1)));
                }
                Self::Atomic(0) => {}
                Self::Atomic(1) => out.push(Self::Units(1)),
                Self::Atomic(units) => out.push(Self::Atomic(units)),
            }
        }
        out
    }
}

/// Where a charge program run against a bound ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// Every event stayed within the bound.
    Within { charged: u64 },
    /// Event `event` of the program passed the bound, at its `unit`th unit
    /// (0 for an atomic event): the run ends charged `charged`.
    Crossed {
        charged: u64,
        event: usize,
        unit: u64,
    },
}

/// The per-unit reference charger: the oracle every machine's charge
/// equals (`K-CHG-002`, `docs/design/kernel-end-state.md` §B).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReferenceCharger {
    charged: u64,
    bound: u64,
}

/// The unit that passed the bound, and the charge the run ends at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crossed {
    pub charged: u64,
    /// Which unit of the charge passed the bound (0 for an atomic event).
    pub unit: u64,
}

impl ReferenceCharger {
    /// A charger at `charged`, bounded by `bound`.
    pub const fn new(charged: u64, bound: u64) -> Self {
        Self { charged, bound }
    }

    pub const fn charged(&self) -> u64 {
        self.charged
    }

    /// Charges `weight`, a unit run one unit at a time. The unit or event
    /// that passes the bound is charged, and nothing after it.
    pub fn charge(&mut self, weight: Weight) -> Result<(), Crossed> {
        match weight {
            Weight::Units(units) => {
                for unit in 0..units {
                    self.charged = self.charged.saturating_add(1);
                    if self.charged > self.bound {
                        return Err(Crossed {
                            charged: self.charged,
                            unit,
                        });
                    }
                }
                Ok(())
            }
            Weight::Atomic(units) => {
                self.charged = self.charged.saturating_add(units);
                if self.charged > self.bound {
                    return Err(Crossed {
                        charged: self.charged,
                        unit: 0,
                    });
                }
                Ok(())
            }
        }
    }

    /// Runs `program` from `charged` against `bound`.
    pub fn run(charged: u64, bound: u64, program: &[Weight]) -> Ending {
        let mut charger = Self::new(charged, bound);
        for (event, weight) in program.iter().enumerate() {
            if let Err(crossed) = charger.charge(*weight) {
                return Ending::Crossed {
                    charged: crossed.charged,
                    event,
                    unit: crossed.unit,
                };
            }
        }
        Ending::Within {
            charged: charger.charged,
        }
    }
}
