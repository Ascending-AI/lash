//! Shared source resource admission. All consumers use this owner for charge
//! additions and slice inequalities; selected ledger programs travel in C.
use crate::compile::expr::Close;

/// The source charge window, read before publishing speculative work.
/// All charge additions and both slice inequalities have one owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Window {
    charged: u64,
    bound: u64,
    slice: u64,
}

/// Evidence that the selected source prefix may publish its effects. A miss
/// has no token. Heap contexts keep their rollback owner until this exists.
pub(crate) struct Accepted<C = ()> {
    pub(super) request: C,
    pub(super) end: u64,
}

/// A fixed iteration suffix is admitted once; later prefixes spend its room.
pub(crate) struct RepeatBudget {
    end: u64,
    remaining: u64,
}

impl RepeatBudget {
    #[inline(always)]
    pub(crate) fn admit(&self, before: u64) -> Option<Accepted> {
        if before > self.remaining {
            return None;
        }
        Some(Accepted {
            request: (),
            // The accepted suffix and remaining allowance prove this fits.
            end: self.end + before,
        })
    }
}

impl Window {
    #[inline(always)]
    pub(crate) fn new(charged: u64, bound: u64, slice: u64) -> Self {
        Self {
            charged,
            bound,
            slice,
        }
    }

    #[inline(always)]
    pub(crate) fn admit<C>(
        self,
        request: C,
        before: u64,
        units: u64,
        close: Option<Close>,
        strict: bool,
    ) -> Option<Accepted<C>> {
        let end = self
            .charged
            .checked_add(before)?
            .checked_add(units)?
            .checked_add(close.map_or(0, |close| u64::from(close.total)))?;
        (end <= self.bound && (!strict || end < self.slice)).then_some(Accepted { request, end })
    }

    #[inline(always)]
    pub(crate) fn repeating(self, units: u64) -> Option<RepeatBudget> {
        let accepted = self.admit((), 0, units, None, true)?;
        Some(RepeatBudget {
            end: accepted.end,
            remaining: self.bound.min(self.slice.checked_sub(1)?) - accepted.end,
        })
    }
}

pub(crate) trait Counters {
    fn can_advance(&self, occurrences: &[usize]) -> bool;
}
pub(crate) struct Requirements<'a> {
    pub(crate) occurrences: &'a [usize],
    pub(crate) before: u64,
    pub(crate) units: u64,
    pub(crate) close: Option<Close>,
    pub(crate) strict: bool,
}
impl Window {
    #[inline(always)]
    pub(crate) fn admit_counted<C>(
        self,
        counters: &impl Counters,
        request: C,
        need: &Requirements<'_>,
    ) -> Option<Accepted<C>> {
        if !counters.can_advance(need.occurrences) {
            return None;
        }
        self.admit(request, need.before, need.units, need.close, need.strict)
    }
}
