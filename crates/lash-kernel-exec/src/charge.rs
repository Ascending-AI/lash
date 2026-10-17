//! The charge event program (`K-CHG-001`..`K-CHG-008`) and its oracle
//! (`docs/design/kernel-end-state.md`, seam B).
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
//!   saturating addition and the bound's strict `>`;
//! - [`RegionSummary`]: a region is a maximal run of fixed-weight events
//!   with no call, action, dynamic formula, handler entry or other
//!   observation inside. Its summary is the charge each exit commits,
//!   built by [`RegionSummary::of`] and checked by
//!   [`RegionSummary::verify`] before a region is admitted;
//! - [`RegionSummary::admit`]: fast admission. With entry charge `c0`, a
//!   region runs without internal checks only when `c0 + total` passes
//!   neither the bound nor the slice threshold; otherwise its events replay
//!   in order, as the reference charges them.
//!
//! Today's machine charges each anchor itself (`lash-kernel-vm`'s
//! `charge` and `charge_nodes`); laws compare it with the reference. The
//! instruction-set stage admits regions through [`RegionSummary`] and keeps
//! the reference as its oracle.

use crate::sem::ChargeEvent;

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

/// One event of a region: its kind and its fixed weight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RegionEvent {
    pub event: ChargeEvent,
    pub weight: Weight,
}

/// How a region is admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// The whole region fits: it runs without internal checks, and every
    /// exit commits the entry charge plus its executed prefix.
    Fast,
    /// It may pass the bound or the slice threshold inside: its events
    /// replay in order, each checked.
    Replay,
}

/// Why a region summary is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RegionError {
    #[error("event {at} is {event:?}, whose amount lowering does not know")]
    Dynamic { at: usize, event: ChargeEvent },
    #[error("the region's total overflows")]
    Overflow,
    #[error("the summary states {stated} events, the region has {events}")]
    Length { stated: usize, events: usize },
    #[error("the summary's prefix after {at} events is {stated}, the events sum to {expected}")]
    Prefix {
        at: usize,
        stated: u64,
        expected: u64,
    },
}

/// A region's summary: the units committed after each prefix of its
/// events, from none to all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionSummary {
    prefixes: Box<[u64]>,
}

impl RegionSummary {
    /// The summary of `events`, each a fixed event of `K-CHG-002`.
    pub fn of(events: &[RegionEvent]) -> Result<Self, RegionError> {
        let mut prefixes = Vec::with_capacity(events.len() + 1);
        let mut total = 0_u64;
        prefixes.push(total);
        for (at, event) in events.iter().enumerate() {
            if !event.event.is_fixed() {
                return Err(RegionError::Dynamic {
                    at,
                    event: event.event,
                });
            }
            total = total
                .checked_add(event.weight.units())
                .ok_or(RegionError::Overflow)?;
            prefixes.push(total);
        }
        Ok(Self {
            prefixes: prefixes.into(),
        })
    }

    /// Checks that this summary is `events`' own: what a region is checked
    /// against before it is admitted to the fast path.
    pub fn verify(&self, events: &[RegionEvent]) -> Result<(), RegionError> {
        let expected = Self::of(events)?;
        if expected.prefixes.len() != self.prefixes.len() {
            return Err(RegionError::Length {
                stated: self.prefixes.len().saturating_sub(1),
                events: events.len(),
            });
        }
        match expected
            .prefixes
            .iter()
            .zip(self.prefixes.iter())
            .position(|(expected, stated)| expected != stated)
        {
            Some(at) => Err(RegionError::Prefix {
                at,
                stated: self.prefixes[at],
                expected: expected.prefixes[at],
            }),
            None => Ok(()),
        }
    }

    /// How many events the region has.
    pub fn events(&self) -> usize {
        self.prefixes.len() - 1
    }

    /// The region's whole charge.
    pub fn total(&self) -> u64 {
        self.prefixes[self.prefixes.len() - 1]
    }

    /// How a region entered at charge `entry` runs, under `bound` and a
    /// slice that returns once the run is charged `slice_end`. Strictly
    /// below the threshold, no step the region contains can reach it. A
    /// running region's entry is within its bound: a run charged past it
    /// has ended.
    pub fn admit(&self, entry: u64, bound: u64, slice_end: u64) -> Admission {
        match entry.checked_add(self.total()) {
            Some(end) if end <= bound && end < slice_end => Admission::Fast,
            _ => Admission::Replay,
        }
    }

    /// What a fast region commits when it is left after `executed` of its
    /// events: the entry charge plus the executed prefix, never a suffix
    /// charged ahead and given back. `None` past the region's end.
    pub fn commit(&self, entry: u64, executed: usize) -> Option<u64> {
        entry.checked_add(*self.prefixes.get(executed)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Xorshift: the regions are the same on every run.
    struct Seeded(u64);

    impl Seeded {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    const FIXED: [ChargeEvent; 5] = [
        ChargeEvent::Statement,
        ChargeEvent::ExprNode,
        ChargeEvent::AtomArg,
        ChargeEvent::Construction,
        ChargeEvent::LoopTest,
    ];

    /// `K-CHG-002`: a region admitted to the fast path commits, at every
    /// exit, exactly what the per-unit reference has charged there and
    /// never passes the bound or the slice threshold; any region the
    /// reference would stop inside replays. Entries run up to `u64::MAX`,
    /// so saturation and overflow are on the swept edges; bounds start at
    /// the entry, since a run charged past its bound has ended.
    #[test]
    fn a_fast_region_commits_what_the_per_unit_reference_charges() {
        let mut random = Seeded(0x6691_c4a2);
        let (mut fast, mut replayed) = (0, 0);
        for _ in 0..512 {
            let events: Vec<RegionEvent> = (0..1 + random.below(8))
                .map(|_| {
                    let units = random.below(4);
                    RegionEvent {
                        event: FIXED[random.below(5) as usize],
                        weight: if random.below(2) == 0 {
                            Weight::Units(units)
                        } else {
                            Weight::Atomic(units)
                        },
                    }
                })
                .collect();
            let summary = RegionSummary::of(&events).expect("fixed events summarize");
            assert_eq!(summary.verify(&events), Ok(()));
            let total = summary.total();
            let program: Vec<Weight> = events.iter().map(|event| event.weight).collect();
            let entry = match random.below(3) {
                0 => random.below(16),
                1 => u64::MAX - random.below(16),
                _ => u64::MAX / 2,
            };
            for bound in [
                entry,
                entry.saturating_add(total.saturating_sub(1)),
                entry.saturating_add(total),
                entry.saturating_add(total + 1),
                u64::MAX,
            ] {
                for slice_end in [entry.saturating_add(total), u64::MAX] {
                    let reference = ReferenceCharger::run(entry, bound, &program);
                    match summary.admit(entry, bound, slice_end) {
                        Admission::Fast => {
                            fast += 1;
                            assert_eq!(
                                reference,
                                Ending::Within {
                                    charged: entry + total
                                }
                            );
                            assert!(entry + total < slice_end);
                            let mut charger = ReferenceCharger::new(entry, bound);
                            for executed in 0..=events.len() {
                                assert_eq!(
                                    summary.commit(entry, executed),
                                    Some(charger.charged())
                                );
                                if let Some(weight) = program.get(executed) {
                                    assert_eq!(charger.charge(*weight), Ok(()));
                                }
                            }
                        }
                        Admission::Replay => {
                            replayed += 1;
                            assert!(
                                matches!(reference, Ending::Crossed { .. })
                                    || entry.checked_add(total).is_none_or(|end| end >= slice_end),
                                "{events:?} from {entry} under {bound} replays needlessly"
                            );
                        }
                    }
                }
            }
        }
        assert!(fast > 0 && replayed > 0, "fast {fast}, replayed {replayed}");
    }

    /// The summary checked before admission is the region's own: one that
    /// states another prefix, another length, or a dynamic event is
    /// refused.
    #[test]
    fn a_region_summary_that_misstates_its_events_is_refused() {
        let event = |event, weight| RegionEvent { event, weight };
        let events = [
            event(ChargeEvent::Statement, Weight::Atomic(1)),
            event(ChargeEvent::ExprNode, Weight::Units(3)),
            event(ChargeEvent::Construction, Weight::Atomic(2)),
        ];
        let summary = RegionSummary::of(&events).expect("fixed events summarize");
        assert_eq!(summary.total(), 6);
        let mut miscounted = events;
        miscounted[1].weight = Weight::Units(2);
        assert_eq!(
            summary.verify(&miscounted),
            Err(RegionError::Prefix {
                at: 2,
                stated: 4,
                expected: 3
            })
        );
        assert_eq!(
            summary.verify(&events[..2]),
            Err(RegionError::Length {
                stated: 3,
                events: 2
            })
        );
        assert_eq!(
            RegionSummary::of(&[event(ChargeEvent::NativeFormula, Weight::Atomic(4))]),
            Err(RegionError::Dynamic {
                at: 0,
                event: ChargeEvent::NativeFormula
            })
        );
        assert_eq!(
            RegionSummary::of(&[
                event(ChargeEvent::ExprNode, Weight::Units(u64::MAX)),
                event(ChargeEvent::ExprNode, Weight::Units(1)),
            ]),
            Err(RegionError::Overflow)
        );
    }
}
