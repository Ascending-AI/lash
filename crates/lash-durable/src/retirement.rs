//! The node's page retirement (`D-RET`): a maintenance task beside the
//! lease renewal that runs the store's bounded retirement steps, and the
//! backpressure it applies. While the store's retirement backlog is above
//! the host's limit, and from boot until the task's first backlog read
//! answers, the node claims no new actors; while it is above, the task
//! sweeps without waiting, until the backlog falls back under it.

use std::sync::Arc;

use lash_core_ids::clock::Clock;
use tokio::sync::watch;

use crate::domain::pages::{PageStore, RetirementBudget, RetirementProgress};
use crate::durable_config::RetirementSettings;

/// Whether the node's retirement is behind its backlog limit: the runner
/// claims no new actors while it is, and watches for it to catch up.
#[derive(Clone, Debug)]
pub(crate) struct Behind(Arc<watch::Sender<bool>>);

impl Behind {
    /// Never behind: a node whose store holds no heap pages.
    pub(crate) fn never() -> Self {
        Self(Arc::new(watch::Sender::new(false)))
    }

    /// Behind until the task's first backlog read answers: a node that
    /// boots over a backlog claims nothing before it knows.
    pub(crate) fn unread() -> Self {
        Self(Arc::new(watch::Sender::new(true)))
    }

    pub(crate) fn get(&self) -> bool {
        *self.0.borrow()
    }

    /// Each change, from now on.
    pub(crate) fn watch(&self) -> watch::Receiver<bool> {
        self.0.subscribe()
    }

    fn set(&self, behind: bool) {
        self.0
            .send_if_modified(|now| std::mem::replace(now, behind) != behind);
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
