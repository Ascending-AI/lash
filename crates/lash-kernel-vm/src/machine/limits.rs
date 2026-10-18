//! Where a run reaches a bound, its host decides (FIG-6768): the
//! [`MemoryLimiter`] or [`ChargeLimiter`] the segment was given grows the
//! limit or refuses, and a run without one refuses. Both are asked at the
//! exact point the bound is reached, before the statement's effect, so a
//! run resumed from an earlier seal reaches the same point and asks again.

use crate::interface::{Bound, ChargeLimiter, LimitDecision, MemoryLimiter};

use super::{Halt, KernelMachine, bound};

/// The segment's limiters; none refuses.
#[derive(Default)]
pub(super) struct Limiters {
    pub(super) memory: Option<Box<dyn MemoryLimiter>>,
    pub(super) charge: Option<Box<dyn ChargeLimiter>>,
}

/// Asks `ask` at `used` against `limit`, and again while a grown limit
/// still leaves `used` past it. A refusal, or a growth of nothing, ends the
/// run with `which` at the limit it reached.
fn decide(
    mut ask: impl FnMut(u64, u64) -> LimitDecision,
    used: u64,
    limit: &mut u64,
    which: Bound,
) -> Result<(), Halt> {
    loop {
        match ask(used, *limit) {
            LimitDecision::Grow(by) if by > 0 => *limit = limit.saturating_add(by),
            LimitDecision::Grow(_) | LimitDecision::Refuse => return Err(bound(which, *limit)),
        }
        if used <= *limit {
            return Ok(());
        }
    }
}

impl Limiters {
    /// The charge passed `limit` at `used`.
    #[cold]
    #[inline(never)]
    pub(super) fn charge_reached(&mut self, used: u64, limit: &mut u64) -> Result<(), Halt> {
        let Some(limiter) = self.charge.as_deref_mut() else {
            return Err(bound(Bound::Charge, *limit));
        };
        decide(
            |used, limit| limiter.limit_reached(used, limit),
            used,
            limit,
            Bound::Charge,
        )
    }
}

impl KernelMachine {
    /// The memory decision at a reservation of `bytes` the conservative
    /// meter could not grant. The run first recounts what it holds exactly
    /// and frees what nothing holds; only when that still leaves no room is
    /// the limiter asked, and a grown limit is recounted against again.
    pub(super) fn memory_pressure(&mut self, bytes: u64) -> Result<(), Halt> {
        if self.bounds.memory_meter == crate::MemoryMeter::Off {
            return Ok(());
        }
        loop {
            match self.refine_resident(bytes) {
                Err(Halt::Bound(exceeded)) if exceeded.bound == Bound::Memory => {
                    self.memory_reached(bytes)?;
                }
                other => return other,
            }
        }
    }

    /// The exact count with `bytes` pending does not fit the memory bound.
    #[cold]
    #[inline(never)]
    fn memory_reached(&mut self, bytes: u64) -> Result<(), Halt> {
        let Some(limiter) = self.limiters.memory.as_deref_mut() else {
            return Err(bound(Bound::Memory, self.bounds.memory));
        };
        let used = self.heap.certificate().upper.0.saturating_add(bytes);
        decide(
            |used, limit| limiter.limit_reached(used, limit),
            used,
            &mut self.bounds.memory,
            Bound::Memory,
        )?;
        self.heap.set_memory_bound(self.bounds.memory);
        Ok(())
    }
}
