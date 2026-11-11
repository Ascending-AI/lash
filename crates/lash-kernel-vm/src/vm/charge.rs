//! The event contract's meter (cutover contract §3).
//!
//! On block entry the dispatcher tries to prepay the block's fixed cost.
//! While a block is prepaid, the true prefix at op `o` is
//! `T = used - rest(o)`, where `rest(o)` is the fixed cost the ops after
//! `o` bill (`o`'s own event excluded): `rest(o) = cost - through(o)`.
//! Otherwise the block runs event by event. Prepayment is checked, so
//! `used <= limit` always holds and `T` is exact.
//!
//! A block is prepaid only when it also ends before the slice does, so
//! a prepaid block holds no step at which the slice could end: the slice
//! is tested at step ops on the exact path alone.
//!
//! A dynamic fee in a prepaid block is billed on top of its reservation
//! while the whole stays within the limit and ends before the slice: no
//! event left in the block can then be refused or end the slice, so the
//! block stays prepaid. Leaving a prepaid block early (a raise, a refusal,
//! `finish`, a bound error, a limiter consultation, a dynamic fee that
//! does not fit, a cancellation) cancels the reservation exactly once: the
//! reservation is an `Option`, taken when cancelled.

use lash_kernel_doc::{Measure, Value};

use super::{Run, Vm, read};
use crate::bytecode::charge_tables::RESULT;
use crate::heap::Heap;

/// A charge refusal: the event would pass the limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Refused {
    /// What the run would have been charged with the refused event,
    /// saturated: one past the limit for a unit event, the whole event
    /// on top of what was paid for an atomic one.
    pub total: u64,
    /// What of the event is still unbilled, retried if a limiter grows
    /// the limit.
    pub rest: Rest,
}

/// The unbilled part of a refused event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rest {
    /// One-unit events, billed one at a time.
    Units(u64),
    /// One atomic event (construction members, `metered` units, a fee).
    Atomic(u64),
}

/// The run's charge state.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Meter {
    /// What the run has paid, including any outstanding reservation.
    used: u64,
    limit: u64,
    /// The prepaid block's whole fixed cost, while it is reserved.
    reserved: Option<u64>,
    /// The charge at which the running slice ends.
    slice_end: u64,
}

impl Meter {
    pub(crate) fn new(used: u64, limit: u64) -> Self {
        Self {
            used,
            limit,
            reserved: None,
            slice_end: u64::MAX,
        }
    }

    /// Starts a slice of `slice` units from what is paid now.
    pub(crate) fn slice(&mut self, slice: u64) {
        debug_assert!(self.reserved.is_none(), "a slice starts outside a block");
        self.slice_end = self.used.saturating_add(slice);
    }

    /// Whether the slice is spent: tested at a step op on the exact path.
    #[inline]
    pub(crate) fn spent(&self) -> bool {
        self.used >= self.slice_end
    }

    #[inline]
    pub(crate) fn limit(&self) -> u64 {
        self.limit
    }

    /// Prepays a block of fixed `cost`, `None` when it is unrepresentable.
    /// Gives whether the block is prepaid; when it is not, the caller runs
    /// it event by event.
    #[inline]
    pub(crate) fn enter(&mut self, cost: Option<u64>) -> bool {
        debug_assert!(self.reserved.is_none(), "a block is entered once reserved");
        match cost.and_then(|cost| self.used.checked_add(cost).map(|used| (cost, used))) {
            Some((cost, used)) if used <= self.limit && used < self.slice_end => {
                self.used = used;
                self.reserved = Some(cost);
                true
            }
            _ => false,
        }
    }

    /// Adds a fixed event's `units` under `Charge::Off`, where they only
    /// place the slice: no limit is compared.
    #[inline]
    pub(crate) fn place(&mut self, units: u64) {
        debug_assert!(self.reserved.is_none(), "exact billing in a prepaid block");
        self.used = self.used.saturating_add(units);
    }

    /// Whether a block's reservation is outstanding.
    #[inline]
    pub(crate) fn prepaid(&self) -> bool {
        self.reserved.is_some()
    }

    /// The block ended at its last op: its reservation is spent.
    #[inline]
    pub(crate) fn leave(&mut self) {
        self.reserved = None;
    }

    /// Cancels the reservation at an op whose block has billed `through`
    /// of its cost so far (that op's own event included): what is left,
    /// `rest(o)`, is given back. A second call does nothing.
    #[inline]
    pub(crate) fn unbill(&mut self, through: u64) {
        if let Some(cost) = self.reserved.take() {
            self.used -= cost - through;
        }
    }

    /// Bills `units` one-unit events on the exact path. At a refusal the
    /// units that fit are billed and the first that passes is refused.
    #[inline]
    pub(crate) fn units(&mut self, units: u64) -> Result<(), Refused> {
        debug_assert!(self.reserved.is_none(), "exact billing in a prepaid block");
        match self.used.checked_add(units) {
            Some(used) if used <= self.limit => {
                self.used = used;
                Ok(())
            }
            _ => {
                let fit = self.limit - self.used;
                self.used = self.limit;
                Err(Refused {
                    total: self.limit.saturating_add(1),
                    rest: Rest::Units(units - fit),
                })
            }
        }
    }

    /// Bills one atomic event of `units`, whole or not at all.
    #[inline]
    pub(crate) fn atomic(&mut self, units: u64) -> Result<(), Refused> {
        debug_assert!(self.reserved.is_none(), "exact billing in a prepaid block");
        match self.used.checked_add(units) {
            Some(used) if used <= self.limit => {
                self.used = used;
                Ok(())
            }
            _ => Err(Refused {
                total: self.used.saturating_add(units),
                rest: Rest::Atomic(units),
            }),
        }
    }

    /// Retries what a refusal left unbilled, after a limiter grew the
    /// limit.
    pub(crate) fn retry(&mut self, rest: Rest) -> Result<(), Refused> {
        match rest {
            Rest::Units(units) => self.units(units),
            Rest::Atomic(units) => self.atomic(units),
        }
    }

    /// Bills a dynamic fee of `units` on top of a prepaid block's
    /// reservation, when the block and the fee fit the limit and end
    /// before the slice: then every prefix of the block's rest is within
    /// both, as on the exact path. Gives whether it was billed.
    #[inline]
    pub(crate) fn prepay(&mut self, units: u64) -> bool {
        if self.reserved.is_none() {
            return false;
        }
        match self.used.checked_add(units) {
            Some(used) if used <= self.limit && used < self.slice_end => {
                self.used = used;
                true
            }
            _ => false,
        }
    }

    /// Whether an atomic event of `units` billed now, every reservation
    /// spent, fits the limit.
    #[inline]
    pub(crate) fn fits(&self, units: u64) -> bool {
        self.used
            .checked_add(units)
            .is_some_and(|used| used <= self.limit)
    }

    /// A dynamic fee at an op that has billed `through` of its block: the
    /// reservation is cancelled and the fee billed on the exact path; the
    /// block continues event by event (contract §3).
    #[inline]
    pub(crate) fn fee(&mut self, units: u64, through: u64) -> Result<(), Refused> {
        self.unbill(through);
        self.atomic(units)
    }

    /// What the run has truly paid at an op that has billed `through` of
    /// its block.
    #[inline]
    pub(crate) fn total(&self, through: u64) -> u64 {
        match self.reserved {
            Some(cost) => self.used - (cost - through),
            None => self.used,
        }
    }

    /// A limiter grew the limit by `more`: execution resumes on the exact
    /// path at the refused event.
    pub(crate) fn grow(&mut self, more: u64) {
        debug_assert!(self.reserved.is_none());
        self.limit = self.limit.saturating_add(more);
    }
}

impl<H: Heap> Vm<H> {
    /// Evaluates formula `charge` over `args` and `result`.
    pub(super) fn formula(&self, charge: u32, args: &[Value], result: Option<&Value>) -> Run<u64> {
        #[cfg(test)]
        super::charge_work();
        let formula = self.program.charge(charge);
        if let Some(units) = formula.constant() {
            return Ok(units);
        }
        // Two arguments and a result of size 1 or 2 read their fee from
        // the formula's table.
        if let ([first, second], Some(result)) = (args, result)
            && let Some(units) = formula.pair([first, second, result])
        {
            return Ok(units);
        }
        let arena = self.heap.arena();
        // A sum of sizes over values that hold no member is billed without
        // its tree, the same amount: such a value's deep size is its size
        // (`K-CHG-005`). A leaf the arena holds (a long text, bytes, a big
        // integer) is measured from its body; one it cannot read now takes
        // the tree, which meets the same read.
        if let Some(linear) = formula.linear()
            && let Some(units) = linear.units(|position| {
                let value = if position == RESULT {
                    result
                } else {
                    args.get(usize::from(position))
                };
                match value {
                    None => Some(0),
                    Some(value) => crate::data::immediate_size(value).or_else(|| {
                        crate::data::holds_nothing(value)
                            .then(|| crate::data::size(arena, value).ok())
                            .flatten()
                    }),
                }
            })
        {
            return Ok(units);
        }
        let mut failed = None;
        let units = formula.evaluate(|position, how| {
            let value = if position == RESULT {
                result
            } else {
                args.get(usize::from(position))
            };
            let Some(value) = value else {
                return 0;
            };
            if let Some(units) = crate::data::immediate_measure(value, how) {
                return units;
            }
            let measured = match how {
                Measure::Size => crate::data::size(arena, value),
                Measure::DeepSize => crate::data::deep_size(arena, value),
                Measure::NestedSize => crate::data::nested_size(arena, value),
                Measure::Magnitude => Ok(crate::data::magnitude(arena, value)),
            };
            measured.unwrap_or_else(|error| {
                failed = Some(error);
                0
            })
        });
        match failed {
            Some(error) => Err(read(error)),
            None => Ok(units),
        }
    }
}
