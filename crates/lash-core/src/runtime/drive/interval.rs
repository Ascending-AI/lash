//! The recovery tick's schedule (ADR 0109 §1.8, FIG-3600 S7): a fixed grid on
//! the deployment's clock.
//!
//! An engine runs its deployment's recovery pass on a [`RecoveryInterval`]:
//! the pass fires every [`RECOVERY_TICK`] from the grid's start, whatever the
//! pass before it spent, so a pass that returns within the period never
//! moves the next one. A pass that overran its period fires the next at
//! once and the grid moves on from there: consecutive passes start at most
//! `max(period, previous pass)` apart. A pass itself is bounded by its
//! tick's wait on the obligation lanes ([`RelayLanes`](super::RelayLanes))
//! and its leader arms, never by an obligation delivery.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::Clock;

/// The recovery pass's period, `T` in ADR 0109 §1.8.
pub const RECOVERY_TICK: Duration = Duration::from_secs(10);

/// A fixed-period schedule on a [`Clock`].
pub struct RecoveryInterval {
    clock: Arc<dyn Clock>,
    period: Duration,
    next: Option<Instant>,
}

impl RecoveryInterval {
    /// A schedule of `period` on `clock`; its first tick fires at once.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>, period: Duration) -> Self {
        Self {
            clock,
            period,
            next: None,
        }
    }

    /// Wait for the next tick and answer when it was due.
    pub async fn tick(&mut self) -> Instant {
        let now = self.clock.now();
        let due = match self.next {
            None => now,
            Some(next) if next > now => {
                self.clock.sleep_until(next).await;
                next
            }
            // The pass before overran its period: fire at once.
            Some(_) => now,
        };
        self.next = Some(due + self.period);
        due
    }
}
