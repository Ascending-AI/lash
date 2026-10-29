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

/// The source verifier and physical charge verifier select this exact path.
/// It is bound into the checked prefix, including every close and unpaid head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Allowance {
    pub(crate) occurrences: Vec<usize>,
    pub(crate) before: u64,
    pub(crate) units: u64,
    pub(crate) close: Option<Close>,
    pub(crate) strict: bool,
    pub(crate) depth: u64,
    pub(crate) safe_point: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResourceFacts {
    window: Window,
    depth: u64,
    limit: u64,
    safe_point: u64,
    counts: Vec<(usize, u64)>,
}
/// Exclusive source resource owner. Its borrow prevents charge/counter changes
/// between acceptance and publication; fallible counter staging precedes edits.
pub(crate) struct SourceResources<'a> {
    pub(crate) charged: &'a mut u64,
    pub(crate) counters: &'a mut crate::machine::tasks::Occurrences,
    pub(crate) bound: u64,
    pub(crate) slice: u64,
    pub(crate) depth: u64,
    pub(crate) limit: u64,
    pub(crate) safe_point: u64,
}
pub(crate) struct SourceAccepted<'a, C> {
    resources: SourceResources<'a>,
    accepted: Accepted<C>,
    occurrences: Vec<usize>,
}
impl SourceResources<'_> {
    pub(crate) fn facts(&self, need: &Allowance) -> Option<ResourceFacts> {
        Some(ResourceFacts {
            window: Window::new(*self.charged, self.bound, self.slice),
            depth: self.depth,
            limit: self.limit,
            safe_point: self.safe_point,
            counts: need
                .occurrences
                .iter()
                .map(|index| Some((*index, self.counters.current(*index)?)))
                .collect::<Option<_>>()?,
        })
    }
}
impl<'a> SourceResources<'a> {
    pub(crate) fn admit_interval<C>(
        self,
        facts: &ResourceFacts,
        need: &Allowance,
        request: C,
    ) -> Result<Option<SourceAccepted<'a, C>>, lash_kernel_doc::heap::HeapFault> {
        if self.facts(need).as_ref() != Some(facts)
            || need.safe_point != self.safe_point
            || self
                .depth
                .checked_add(need.depth)
                .is_none_or(|depth| depth > self.limit)
        {
            return Ok(None);
        }
        if !self.counters.prepare_publication(&need.occurrences)? {
            return Ok(None);
        }
        let Some(accepted) = facts.window.admit_counted(
            &*self.counters,
            request,
            &Requirements {
                occurrences: &need.occurrences,
                before: need.before,
                units: need.units,
                close: need.close,
                strict: need.strict,
            },
        ) else {
            return Ok(None);
        };
        Ok(Some(SourceAccepted {
            resources: self,
            accepted,
            occurrences: need.occurrences.clone(),
        }))
    }
}
impl<'a, C> SourceAccepted<'a, C> {
    pub(crate) fn try_map<D, E>(
        self,
        map: impl FnOnce(C) -> Result<D, E>,
    ) -> Result<SourceAccepted<'a, D>, E> {
        let request = map(self.accepted.request)?;
        Ok(SourceAccepted {
            resources: self.resources,
            accepted: Accepted {
                request,
                end: self.accepted.end,
            },
            occurrences: self.occurrences,
        })
    }
    pub(crate) fn commit<R, E>(self, commit: impl FnOnce(C) -> Result<R, E>) -> Result<R, E> {
        let result = commit(self.accepted.request)?;
        *self.resources.charged = self.accepted.end;
        self.resources.counters.publish_admitted(&self.occurrences);
        Ok(result)
    }
}
