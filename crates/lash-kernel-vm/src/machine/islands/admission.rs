//! Admission precedes publication. Storage owners retain their own discard,
//! snapshot and moved-storage rollback implementations.

use super::*;

#[derive(Clone, Copy)]
pub(super) enum Prefix {
    Access,
    Complete,
}

#[derive(Clone, Copy)]
pub(super) enum DepthNeed {
    Compiled(Depth),
    Exact(u32),
}

/// Resource eligibility remains valid across resident reads, which neither
/// publish effects nor grow the arena. Charge admission still follows them.
pub(super) struct Resources;

/// The source call's resource requirements. Its immutable compiled site
/// retains the effect and occurrence metadata across arena work.
#[derive(Clone, Copy)]
pub(super) struct Request {
    pub(super) depth: DepthNeed,
    pub(super) room: u64,
    pub(super) occurrence: Option<usize>,
}

/// The source charge window, read before publishing speculative work.
/// All charge additions and both slice inequalities have one owner.
#[derive(Clone, Copy)]
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

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    #[inline(always)]
    pub(super) fn admission_window(&self) -> Window {
        Window::new(self.charged, self.bound, self.slice_end)
    }

    #[inline(always)]
    pub(super) fn direct_resources(&self, depth: DepthNeed, room: u64) -> Option<Resources> {
        if OBSERVING
            || !self.swift
            || self.deferred_end.is_some()
            || self.arena.any_cold()
            || !match depth {
                DepthNeed::Compiled(depth) => self.depth_fits(depth),
                DepthNeed::Exact(depth) => {
                    self.base + u64::from(self.depth) + u64::from(depth) <= self.call_depth
                }
            }
            || !self.resident_room(room)
        {
            return None;
        }
        Some(Resources)
    }

    #[inline(always)]
    pub(super) fn admit_direct_source(
        &self,
        _: Resources,
        occurrence: Option<usize>,
        before: u64,
        units: u64,
        close: Option<Close>,
    ) -> Option<Accepted> {
        self.source_admission(occurrence, before, units, close, true)
    }

    #[inline(always)]
    pub(super) fn source_admission(
        &self,
        occurrence: Option<usize>,
        before: u64,
        units: u64,
        close: Option<Close>,
        strict: bool,
    ) -> Option<Accepted> {
        if occurrence.is_some_and(|at| !self.counters.can_advance(&[at])) {
            return None;
        }
        self.admission_window()
            .admit((), before, units, close, strict)
    }

    #[inline(always)]
    pub(super) fn direct_admission(
        &self,
        request: Request,
        before: u64,
        units: u64,
        close: Option<Close>,
    ) -> Option<Accepted> {
        let resources = self.direct_resources(request.depth, request.room)?;
        self.admit_direct_source(resources, request.occurrence, before, units, close)
    }

    /// The occurrence is the same immutable source edge checked at admission.
    /// Keep it in the compiled site rather than copying it across arena work.
    #[inline(always)]
    pub(super) fn commit_admission(
        &mut self,
        accepted: Accepted,
        occurrence: Option<usize>,
    ) -> Result<(), Stop> {
        if let Some(at) = occurrence {
            self.count(at)?;
        }
        self.charged = accepted.end;
        self.covered = false;
        Ok(())
    }
}
