//! One execution's parent-owned accounting across its checkouts: attempts,
//! worker CPU and the wall time it holds workers.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::pool::{limit, lock, nanos};
use crate::{PoolConfig, PoolError};

/// Parent-owned accounting. Carry this same value across substrate redrives
/// and replacement checkouts. The pool never rewinds it or retries execution.
///
/// Its wall time is the time the execution holds workers: from each
/// checkout's admission to its release or discard, so it runs on through
/// page answers, host reads and waits that keep the worker, and stops while
/// the execution holds none, in the pool's queue or at a durable park that
/// released its worker (FIG-6786).
#[derive(Clone, Debug, Default)]
pub struct ExecutionBudget(Arc<Mutex<BudgetTotals>>);
#[derive(Debug, Default)]
struct BudgetTotals {
    attempts: u32,
    cpu_nanos: u64,
    replacement: bool,
    /// The wall time of the holds that ended, and what the execution held
    /// before this budget began.
    held_nanos: u64,
    /// The checkouts that hold a worker now, and since when the first of
    /// them has.
    holds: u32,
    held_since: Option<Instant>,
}
impl BudgetTotals {
    fn held(&self, now: Instant) -> u64 {
        self.held_nanos.saturating_add(
            self.held_since
                .map_or(0, |since| nanos(now.saturating_duration_since(since))),
        )
    }
}
impl ExecutionBudget {
    pub fn totals(&self) -> (u32, Duration) {
        let totals = lock(&self.0);
        (totals.attempts, Duration::from_nanos(totals.cpu_nanos))
    }
    /// The budget of an execution whose saved state says it already held
    /// workers for `held`: a code-mode cell resumed on another activation
    /// or owner. Its attempts and CPU count from zero.
    pub fn resumed(held: Duration) -> Self {
        Self(Arc::new(Mutex::new(BudgetTotals {
            held_nanos: nanos(held),
            ..BudgetTotals::default()
        })))
    }
    /// The wall time the execution has held workers, the hold running now
    /// included.
    pub fn held(&self) -> Duration {
        Duration::from_nanos(lock(&self.0).held(Instant::now()))
    }
    pub(crate) fn admit(&self, config: &PoolConfig) -> Result<(), PoolError> {
        let mut totals = lock(&self.0);
        if totals.replacement && totals.attempts >= config.deadlines.max_attempts {
            return Err(PoolError::RetryLimitExceeded {
                bound: config.deadlines.max_attempts,
            });
        }
        if totals.cpu_nanos >= nanos(config.deadlines.cumulative_cpu) {
            return Err(limit(config.deadlines.cpu_limit()));
        }
        if totals.held(Instant::now()) >= nanos(config.deadlines.execution_wall) {
            return Err(limit(config.deadlines.wall_limit()));
        }
        if totals.attempts == 0 || totals.replacement {
            totals.attempts += 1;
        }
        totals.replacement = false;
        Ok(())
    }
    pub(crate) fn charge(&self, delta: u64, config: &PoolConfig) -> Result<(), PoolError> {
        let mut totals = lock(&self.0);
        totals.cpu_nanos = totals.cpu_nanos.saturating_add(delta);
        if totals.cpu_nanos >= nanos(config.deadlines.cumulative_cpu) {
            return Err(limit(config.deadlines.cpu_limit()));
        }
        Ok(())
    }
    pub(crate) fn replacement(&self, replacement: bool) {
        lock(&self.0).replacement = replacement;
    }
    /// A checkout of the execution holds a worker from now.
    pub(crate) fn hold(&self) {
        let mut totals = lock(&self.0);
        totals.holds += 1;
        if totals.holds == 1 {
            totals.held_since = Some(Instant::now());
        }
    }
    /// A checkout that held a worker holds it no more.
    pub(crate) fn unhold(&self) {
        let mut totals = lock(&self.0);
        totals.holds = totals.holds.saturating_sub(1);
        if totals.holds == 0
            && let Some(since) = totals.held_since.take()
        {
            totals.held_nanos = totals.held_nanos.saturating_add(nanos(since.elapsed()));
        }
    }
    /// When the execution's held wall time reaches its bound, if it holds
    /// on until then.
    pub(crate) fn wall_deadline(&self, config: &PoolConfig) -> Instant {
        let now = Instant::now();
        let left = nanos(config.deadlines.execution_wall).saturating_sub(lock(&self.0).held(now));
        // A wall time no host could reach waits as long as a year does.
        now + Duration::from_nanos(left).min(Duration::from_secs(365 * 86_400))
    }
    /// The wall limit, once the execution has held its workers past it.
    pub(crate) fn wall(&self, config: &PoolConfig) -> Result<(), PoolError> {
        if lock(&self.0).held(Instant::now()) >= nanos(config.deadlines.execution_wall) {
            return Err(limit(config.deadlines.wall_limit()));
        }
        Ok(())
    }
}
