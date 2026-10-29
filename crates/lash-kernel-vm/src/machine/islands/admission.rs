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

use crate::admission::{Accepted, Window};

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
