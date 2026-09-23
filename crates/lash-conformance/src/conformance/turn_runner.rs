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

/// One attempt at a turn that a tier may run more than once: Restate re-runs
/// a handler from the top whenever it replays the invocation (after a
/// suspension or a failed attempt), so an attempt is a factory, and every run
/// of it must issue the same journaled commands.
pub type ConformanceTurnAttempt = Arc<
    dyn for<'a> Fn(
            crate::ScopedEffectController<'a>,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>
        + Send
        + Sync,
>;

/// Runs a [`ConformanceTurnJob`] where the tier runs turns.
#[async_trait::async_trait]
pub trait ConformanceTurnRunner: Send + Sync {
    /// Runs `job` to completion on a controller admitted for `admitted`.
    async fn run_turn(&self, admitted: crate::AdmittedScope, job: ConformanceTurnJob);

    /// Runs one turn across a crash: `crashing` must panic before its turn
    /// commits, and the tier then redelivers the same turn to `redrive` the
    /// way it recovers a crashed turn — a fresh driver over the same host in
    /// process, a retried handler invocation replaying its journal on Restate.
    async fn run_crashed_then_redriven_turn(
        &self,
        admitted: crate::AdmittedScope,
        crashing: ConformanceTurnAttempt,
        redrive: ConformanceTurnAttempt,
    );

    /// The process-work wiring for a runtime whose process segments run on
    /// `worker`. In process the runtime's own port drives the worker; a tier
    /// that runs segments elsewhere (Restate's process workflow) serves them
    /// with `worker` there and hands back a port that only observes.
    fn process_work(
        &self,
        watched: crate::WatchedRegistry,
        worker: lash_core_worker::DurableProcessWorker,
    ) -> crate::ProcessWorkWiring {
        let port = Arc::new(crate::NativeProcessWork::new(&watched, worker));
        crate::ProcessWorkWiring::new(watched, port)
    }
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
    pub fn new(host: Arc<dyn crate::EffectHost>) -> Arc<dyn ConformanceTurnRunner> {
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

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: an unscoped host is a fixture defect"
    )]
    async fn run_crashed_then_redriven_turn(
        &self,
        admitted: crate::AdmittedScope,
        crashing: ConformanceTurnAttempt,
        redrive: ConformanceTurnAttempt,
    ) {
        let scoped = self
            .host
            .scoped(admitted.clone())
            .expect("scope the crashing conformance turn on its host");
        let crashed =
            futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(crashing(scoped)))
                .await;
        assert!(
            crashed.is_err(),
            "the crashing attempt must panic before its turn commits"
        );
        let scoped = self
            .host
            .scoped(admitted)
            .expect("scope the redriven conformance turn on its host");
        redrive(scoped).await;
    }
}
