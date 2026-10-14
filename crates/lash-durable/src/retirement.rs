//! The node's page retirement (`D-RET`): a maintenance task beside the
//! lease renewal that runs the store's bounded retirement steps, and the
//! backpressure it applies. While the store's retirement backlog is above
//! the host's limit the node claims no new actors and the task sweeps
//! without waiting, until the backlog falls back under it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lash_core_ids::clock::Clock;

use crate::domain::pages::{PageStore, RetirementBudget, RetirementProgress};
use crate::durable_config::RetirementSettings;

/// Whether the node's retirement is behind its backlog limit: the runner
/// claims no new actors while it is.
#[derive(Clone, Debug, Default)]
pub(crate) struct Behind(Arc<AtomicBool>);

impl Behind {
    pub(crate) fn get(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn set(&self, behind: bool) {
        self.0.store(behind, Ordering::Release);
    }
}

/// Retires `pages`' holds for as long as the node serves. A failed step or
/// backlog read is retried at the next sweep.
pub(crate) async fn retire(
    pages: Arc<dyn PageStore>,
    clock: Arc<dyn Clock>,
    settings: RetirementSettings,
    behind: Behind,
) {
    loop {
        let over = match pages.retirement_backlog().await {
            Ok(backlog) => backlog.holds > settings.backlog_limit,
            // An unread backlog keeps the last answer.
            Err(_) => behind.get(),
        };
        behind.set(over);
        let swept = sweep(pages.as_ref(), settings).await;
        // Behind and still making progress: the next sweep starts at once.
        if !(over && swept == settings.steps_per_sweep) {
            clock.sleep_until(clock.now() + settings.every).await;
        }
    }
}

/// Up to `steps_per_sweep` steps, until one finds nothing to do. Returns the
/// steps that did work.
async fn sweep(pages: &dyn PageStore, settings: RetirementSettings) -> u32 {
    let budget = RetirementBudget {
        nodes: settings.batch_nodes,
    };
    let mut steps = 0;
    while steps < settings.steps_per_sweep {
        match pages.retire_step(budget).await {
            Ok(progress) if progress != RetirementProgress::default() => steps += 1,
            _ => break,
        }
    }
    steps
}
