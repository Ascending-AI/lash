//! Where a run reaches a bound, its host decides (FIG-6768): the
//! [`MemoryLimiter`] or [`ChargeLimiter`] the segment was given grows the
//! limit or refuses, and a run without one refuses. The charge limiter is
//! asked at the exact point the bound is reached; the memory limiter at the
//! capacity meter's verdict point, before the statement's next effect
//! (FIG-6791).

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
    /// The run's logical holdings would pass `limit` at `used`.
    #[cold]
    #[inline(never)]
    pub(super) fn memory_reached(&mut self, used: u64, limit: &mut u64) -> Result<(), Halt> {
        let Some(limiter) = self.memory.as_deref_mut() else {
            return Err(bound(Bound::Memory, *limit));
        };
        decide(
            |used, limit| limiter.limit_reached(used, limit),
            used,
            limit,
            Bound::Memory,
        )
    }

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
    /// The bytes the run holds: its owners' published logical balance
    /// (FIG-7513). Exact source admission is the only memory authority;
    /// this reads the balance it last published.
    #[inline]
    pub(super) fn capacity_held(&self) -> u64 {
        // An open source region publishes before its balance is read.
        let _ = self.publish_logical_meter();
        self.logical_memory.get()
    }

    /// What the bound leaves past what the run holds: the room a native
    /// call or a region may take before the meter decides.
    pub(super) fn capacity_headroom(&self) -> u64 {
        if self.bounds.memory_meter == crate::MemoryMeter::Off {
            return u64::MAX;
        }
        self.bounds.memory.saturating_sub(self.capacity_held())
    }

    /// A verdict point: where everything the run holds is a root a sweep
    /// visits (a statement's end, a native call's answer once its value is
    /// pinned, an effect), the run flushes its logical holdings and decides
    /// a balance past the bound.
    #[inline]
    pub(super) fn capacity_check(&mut self) -> Result<(), Halt> {
        self.publish_logical_meter()?;
        #[cfg(any(test, feature = "perf-witness"))]
        if let Some(observer) = crate::perf_witness::memory_observer() {
            observer(self.memory_ledger());
        }
        if self.bounds.memory_meter == crate::MemoryMeter::Off
            || self.logical_memory.get() <= self.bounds.memory
        {
            return Ok(());
        }
        self.memory_pressure(0)
    }

    /// The memory decision, at a verdict point, with `bytes` about to be
    /// allocated. What the run holds and `bytes` that fit the bound pass;
    /// otherwise the run sweeps what nothing holds and recounts: first
    /// keeping every ended task's result, then, if that leaves no room,
    /// retiring the results no handle reaches. Only when that still leaves
    /// no room is the limiter asked, with what the run then holds and asks
    /// for, and a grown limit is counted against again.
    pub(super) fn memory_pressure(&mut self, bytes: u64) -> Result<(), Halt> {
        if self.bounds.memory_meter == crate::MemoryMeter::Off {
            return Ok(());
        }
        let mut sweeps = [false, true].into_iter();
        loop {
            let used = self.capacity_held().saturating_add(bytes);
            if used <= self.bounds.memory {
                return Ok(());
            }
            if let Some(retires) = sweeps.next() {
                self.pressure_sweep(retires)?;
                continue;
            }
            self.memory_reached(used)?;
        }
    }

    /// Frees what nothing the run holds reaches: the census's sweep of its
    /// entities first, which warms the changed objects it reads, advances
    /// the sweep generation and, when `retires`, retires the results no
    /// handle reaches; then the arena's cycle collection and release over
    /// the run's tasks, session, registers and pins. The logical balance is
    /// flushed again after it.
    #[cold]
    #[inline(never)]
    fn pressure_sweep(&mut self, retires: bool) -> Result<(), Halt> {
        self.refine_resident(retires)?;
        let registers = self.regs.projection();
        {
            let (tasks, session, pins) = (&self.tasks, &self.session, &self.pins);
            let mut roots = |visit: &mut lash_kernel_state::arena::Visit<'_>| {
                tasks.each_value(visit);
                session.each_value(visit);
                registers.iter().for_each(&mut *visit);
                self.regs.each_source_value(visit);
                pins.values.iter().for_each(&mut *visit);
                pins.borrowed.iter().for_each(&mut *visit);
            };
            let mut arena = self.arena.borrow_mut();
            arena.collect_cycles(&mut roots);
            arena.release(&mut roots);
            arena.recount_resident();
        }
        self.publish_logical_meter()?;
        Ok(())
    }

    /// The memory bound is reached with `used` bytes held and asked for.
    #[cold]
    #[inline(never)]
    fn memory_reached(&mut self, used: u64) -> Result<(), Halt> {
        self.limiters.memory_reached(used, &mut self.bounds.memory)
    }
}

#[cfg(any(test, feature = "perf-witness"))]
impl KernelMachine {
    /// The run's published logical balance, as its memory bound counts it.
    pub fn memory_ledger(&self) -> u64 {
        self.logical_memory.get()
    }
}
