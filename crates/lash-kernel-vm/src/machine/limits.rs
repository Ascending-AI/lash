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

/// The capacity meter's chunk (FIG-6791): below the bound, the meter's
/// slow path runs once each time what the run holds crosses into another
/// chunk, so the hot path is one compare where storage or control grows.
/// It is the regions' nursery room, so a region's entry admission and one
/// chunk agree. The chunk paces the slow path only: what the run holds is
/// counted in bytes, and a bound below one chunk is checked at the bound
/// itself.
pub(super) const CAPACITY_CHUNK: u64 = 64 << 10;

impl KernelMachine {
    /// The bytes the run holds as the capacity meter counts them
    /// (FIG-6791): its arena's resident values and its counted control
    /// state, or, until its first sweep, the capacity its heap was sealed
    /// holding when that is more.
    #[inline]
    pub(super) fn capacity_held(&self) -> u64 {
        self.arena.borrow().held().max(self.capacity_floor)
    }

    /// What the bound leaves past what the run holds: the room a native
    /// call, a region or an island may take before the meter decides.
    pub(super) fn capacity_headroom(&self) -> u64 {
        if self.bounds.memory_meter == crate::MemoryMeter::Off {
            return u64::MAX;
        }
        self.bounds.memory.saturating_sub(self.capacity_held())
    }

    /// Places the arena's capacity mark past what the run holds: at the
    /// next chunk, or just past the bound when that comes first. A run
    /// that holds more than its bound is due at once, so its next verdict
    /// point decides.
    pub(super) fn capacity_counted(&mut self) {
        let mark = if self.bounds.memory_meter == crate::MemoryMeter::Off {
            u64::MAX
        } else {
            let held = self.capacity_held();
            if held > self.bounds.memory {
                0
            } else {
                let next = (held / CAPACITY_CHUNK)
                    .saturating_add(1)
                    .saturating_mul(CAPACITY_CHUNK);
                next.min(self.bounds.memory.saturating_add(1))
            }
        };
        self.arena.borrow_mut().set_capacity_mark(mark);
    }

    /// Storage or control grew to the capacity mark where the run may not
    /// sweep: a fresh value may be held only by its maker. Below the bound
    /// the meter counts the next chunk; past it the mark stays, and the
    /// run's next verdict point decides ([`Self::capacity_check`]).
    #[cold]
    #[inline(never)]
    pub(super) fn capacity_crossed(&mut self) {
        if self.capacity_held() <= self.bounds.memory {
            self.capacity_counted();
        }
    }

    /// A verdict point (FIG-6791): where everything the run holds is a
    /// root a sweep visits (a statement's end, a native call's answer once
    /// its value is pinned, an effect), the meter decides a crossing it
    /// deferred.
    #[inline]
    pub(super) fn capacity_check(&mut self) -> Result<(), Halt> {
        self.logical_flush()?;
        #[cfg(any(test, feature = "perf-witness"))]
        if let Some(observer) = crate::perf_witness::memory_observer() {
            observer(self.memory_ledger());
        }
        if !self.arena.borrow().capacity_due() {
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
                self.capacity_counted();
                return Ok(());
            }
            if let Some(retires) = sweeps.next() {
                self.capacity_sweep(retires)?;
                continue;
            }
            self.memory_reached(used)?;
        }
    }

    /// Frees what nothing the run holds reaches, and recounts: the
    /// census's sweep of its entities first, which warms the changed
    /// objects it reads and advances the sweep generation, then the
    /// arena's cycle collection and release over the run's tasks, session,
    /// registers and pins, then the control state its resident frames
    /// hold. What the run's heap was sealed holding is recounted with it.
    #[cold]
    #[inline(never)]
    fn capacity_sweep(&mut self, retires: bool) -> Result<(), Halt> {
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
        }
        let control = self
            .tasks
            .control_bytes()
            .saturating_add(self.exe.admission_bytes());
        let mut arena = self.arena.borrow_mut();
        arena.recount_resident();
        arena.recount_control(control);
        drop(arena);
        self.capacity_floor = 0;
        Ok(())
    }

    /// The memory bound is reached with `used` bytes held and asked for.
    #[cold]
    #[inline(never)]
    fn memory_reached(&mut self, used: u64) -> Result<(), Halt> {
        let Some(limiter) = self.limiters.memory.as_deref_mut() else {
            return Err(bound(Bound::Memory, self.bounds.memory));
        };
        decide(
            |used, limit| limiter.limit_reached(used, limit),
            used,
            &mut self.bounds.memory,
            Bound::Memory,
        )
    }
}

#[cfg(any(test, feature = "perf-witness"))]
impl KernelMachine {
    /// The actual capacity ledger, rather than a heap-allocation estimate.
    pub fn memory_ledger(&self) -> [u64; 4] {
        let [resident, control, layouts] = self.arena.borrow().capacity_parts();
        [
            resident,
            control,
            layouts,
            self.capacity_floor
                .saturating_sub(resident.saturating_add(control).saturating_add(layouts)),
        ]
    }
}
