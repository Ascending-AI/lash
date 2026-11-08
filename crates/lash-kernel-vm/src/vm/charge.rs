//! The event contract's meter (cutover contract §3).
//!
//! On block entry the dispatcher tries to prepay the block's fixed cost.
//! While a block is prepaid, the true prefix at op `o` is
//! `T = used - rest(o)`, where `rest(o)` is the fixed cost the ops after
//! `o` bill (`o`'s own event excluded). Otherwise the block runs event by
//! event. Prepayment is checked, so `used <= limit` always holds and `T`
//! is exact.
//!
//! Leaving a prepaid block early (a raise, a refusal, `finish`, a bound
//! error, a limiter consultation, a cancellation) cancels the reservation
//! exactly once: the reservation is an `Option`, taken when cancelled.

/// A charge refusal: the event would pass the limit. `total` is what the
/// run would have been charged with it, saturated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Refused {
    pub total: u64,
}

/// The run's charge state.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Meter {
    /// What the run has paid, including any outstanding reservation.
    used: u64,
    limit: u64,
    /// The prepaid block's whole fixed cost, while it is reserved.
    reserved: Option<u64>,
}

impl Meter {
    pub(crate) fn new(used: u64, limit: u64) -> Self {
        Self {
            used,
            limit,
            reserved: None,
        }
    }

    /// Prepays a block of fixed `cost`, `None` when it is unrepresentable.
    /// Gives whether the block is prepaid; when it is not, the caller runs
    /// it event by event.
    #[inline]
    pub(crate) fn enter(&mut self, cost: Option<u64>) -> bool {
        debug_assert!(self.reserved.is_none(), "a block is entered once reserved");
        match cost.and_then(|cost| self.used.checked_add(cost).map(|x| (cost, x))) {
            Some((cost, used)) if used <= self.limit => {
                self.used = used;
                self.reserved = Some(cost);
                true
            }
            _ => false,
        }
    }

    /// The block ended at its last op: its reservation is spent.
    #[inline]
    pub(crate) fn leave(&mut self) {
        self.reserved = None;
    }

    #[inline]
    pub(crate) fn prepaid(&self) -> bool {
        self.reserved.is_some()
    }

    /// Cancels the reservation at an op whose block has billed `through`
    /// of its cost so far (that op's own event included): what is left,
    /// `rest(o)`, is given back. A second call does nothing.
    #[inline]
    pub(crate) fn unbill(&mut self, through: u64) {
        if let Some(cost) = self.reserved.take() {
            let rest = cost - through;
            self.used -= rest;
        }
    }

    /// Bills one event of `units` on the exact path, or refuses before it
    /// runs. Cumulative charge is checked; the refusal report saturates.
    #[inline]
    pub(crate) fn bill(&mut self, units: u64) -> Result<(), Refused> {
        debug_assert!(self.reserved.is_none(), "exact billing in a prepaid block");
        match self.used.checked_add(units) {
            Some(used) if used <= self.limit => {
                self.used = used;
                Ok(())
            }
            _ => Err(Refused {
                total: self.used.saturating_add(units),
            }),
        }
    }

    /// Bills a dynamic fee at an op that has billed `through` of its
    /// block. A fee that fits on top of the reservation leaves the block
    /// prepaid: then every later event of the block fits too, so the run
    /// refuses nowhere in it and the order of billing is unobservable.
    /// Otherwise the reservation is cancelled and the fee is billed on the
    /// exact path; the block continues event by event to its end.
    #[inline]
    pub(crate) fn fee(&mut self, units: u64, through: u64) -> Result<(), Refused> {
        if self.reserved.is_some() {
            match self.used.checked_add(units) {
                Some(used) if used <= self.limit => {
                    self.used = used;
                    return Ok(());
                }
                _ => self.unbill(through),
            }
        }
        self.bill(units)
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

    /// A limiter grew the limit: execution resumes on the exact path at
    /// the refused event, which is billed once.
    pub(crate) fn grow(&mut self, limit: u64) {
        debug_assert!(self.reserved.is_none());
        self.limit = limit;
    }
}
