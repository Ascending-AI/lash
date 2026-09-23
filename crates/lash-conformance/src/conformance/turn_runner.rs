//! Where a turn-driving law runs its turn.
//!
//! The in-process tiers scope a turn on the host the runtime runs on and drive
//! it in the test task. A Restate turn exists only inside a handler: its
//! effects journal on a `ctx`-bound controller, and the deployment-level host
//! refuses every effect that has not entered one. A law that drives a real
//! turn therefore takes the turn as a [`ConformanceTurnJob`] and hands it to
//! the tier's [`ConformanceTurnRunner`], which supplies the scoped controller
//! the turn runs on — the host's own on the in-process tiers, a handler-bound
//! one on Restate — so one law body states the contract on every tier.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// One turn, run on the scoped controller the tier supplies. The job owns
/// everything it drives (the runtime and its inputs) and reports what it
/// observed through its own channel.
pub type ConformanceTurnJob = Box<
    dyn for<'a> FnOnce(
            crate::ScopedEffectController<'a>,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>
        + Send,
>;

/// Runs a [`ConformanceTurnJob`] where the tier runs turns.
#[async_trait::async_trait]
pub trait ConformanceTurnRunner: Send + Sync {
    /// Runs `job` to completion on a controller admitted for `admitted`.
    async fn run_turn(&self, admitted: crate::AdmittedScope, job: ConformanceTurnJob);
}

/// The in-process tiers' runner: the turn is scoped on the host the runtime
/// runs on and driven in the calling task.
pub struct HostTurnRunner {
    host: Arc<dyn crate::EffectHost>,
}

impl HostTurnRunner {
    /// A runner over `host`, which must be the effect host the law's runtime
    /// is built on: group children route through the executors that host
    /// registered.
    pub fn shared(host: Arc<dyn crate::EffectHost>) -> Arc<dyn ConformanceTurnRunner> {
        Arc::new(Self { host })
    }
}

#[async_trait::async_trait]
impl ConformanceTurnRunner for HostTurnRunner {
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: an unscoped host is a fixture defect"
    )]
    async fn run_turn(&self, admitted: crate::AdmittedScope, job: ConformanceTurnJob) {
        let scoped = self
            .host
            .scoped(admitted)
            .expect("scope the conformance turn on its host");
        job(scoped).await;
    }
}
