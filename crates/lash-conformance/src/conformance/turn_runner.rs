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

/// How a job's turn ended, which the tier's engine acts on. A turn that
/// aborted without an outcome — a live fault, or a park on a replay
/// divergence — leaves its execution open: the engine keeps its journal, and
/// the next run of the same scope is that execution's retry, replaying it. A
/// settled turn is done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConformanceTurnEnd {
    /// The turn settled: it finished, or it was recorded as a failed turn.
    Settled,
    /// The turn aborted without an outcome, for this cause.
    Aborted(crate::TurnFailureCause),
}

impl ConformanceTurnEnd {
    /// How a turn that returned `turn` ended.
    pub fn of<T>(turn: &Result<T, crate::RuntimeError>) -> Self {
        match turn {
            Ok(_) => Self::Settled,
            Err(error) => match error.turn_failure_cause() {
                crate::TurnFailureCause::Outcome => Self::Settled,
                cause => Self::Aborted(cause),
            },
        }
    }
}

/// One turn, run on the scoped controller the tier supplies. The job owns
/// everything it drives (the runtime and its inputs), reports what it
/// observed through its own channel, and answers how its turn ended.
pub type ConformanceTurnJob = Box<
    dyn for<'a> FnOnce(
            crate::ScopedEffectController<'a>,
        ) -> Pin<Box<dyn Future<Output = ConformanceTurnEnd> + Send + 'a>>
        + Send,
>;

/// One attempt at a turn that a tier may run more than once: Restate re-runs
/// a handler from the top whenever it replays the invocation (after a
/// suspension or a failed attempt), so an attempt is a factory, and every run
/// of it must issue the same journaled commands.
pub type ConformanceTurnAttempt = Arc<
    dyn for<'a> Fn(
            crate::ScopedEffectController<'a>,
        ) -> Pin<Box<dyn Future<Output = ConformanceTurnEnd> + Send + 'a>>
        + Send
        + Sync,
>;

/// Where a tier cuts an attempt down, named by the replay key of the effect
/// it cuts at. Every tier journals lash's effects under their replay keys,
/// whatever its journal is, so a law states the point once for all tiers and
/// each tier's runner cuts there with its own crash mechanism: a store fault
/// in process, a crashed handler on the Restate server double.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalCut {
    pub replay_key: String,
    pub at: JournalCutPoint,
}

/// Which side of the effect a [`JournalCut`] falls on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalCutPoint {
    /// The effect ran; the attempt dies before its result is durable, and
    /// the redrive runs it again.
    BeforeResult,
    /// The attempt dies before the effect is journaled at all, and the
    /// redrive issues it anew.
    BeforeEffect,
}

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

    /// The replay keys of every effect the tier journaled for `scope`'s
    /// turn, or `None` when this runner cannot read them. A law finds the
    /// key of a [`JournalCut`] here, from a probe run of the same turn.
    async fn recorded_replay_keys(&self, _scope: &crate::ExecutionScope) -> Option<Vec<String>> {
        None
    }

    /// Runs one turn across a cut the tier injects: `attempt` runs until the
    /// tier kills it at `cut`, and the tier then redelivers the same turn to
    /// `redrive` the way it recovers a crashed turn. Panics when the cut did
    /// not fire. A runner that cannot cut says so by panicking.
    async fn run_cut_then_redriven_turn(
        &self,
        _admitted: crate::AdmittedScope,
        cut: JournalCut,
        _attempt: ConformanceTurnAttempt,
        _redrive: ConformanceTurnAttempt,
    ) {
        panic!("this tier's turn runner cannot cut a turn at {cut:?}");
    }

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
    /// The host's journal fault injector, which cuts turns at a
    /// [`JournalCut`]: a failed claim before the effect, a failed finalize
    /// before its result.
    journal_faults: Option<lash_core::facade_support::effect_replay_driver::EffectJournalFaults>,
}

impl HostTurnRunner {
    /// A runner over `host`, which must be the effect host the law's runtime
    /// is built on: group children route through the executors that host
    /// registered.
    pub fn shared(host: Arc<dyn crate::EffectHost>) -> Arc<dyn ConformanceTurnRunner> {
        Arc::new(Self {
            host,
            journal_faults: None,
        })
    }

    /// [`shared`](Self::shared), cutting turns with `faults`, the journal
    /// fault injector of `host`.
    pub fn with_journal_faults(
        host: Arc<dyn crate::EffectHost>,
        faults: lash_core::facade_support::effect_replay_driver::EffectJournalFaults,
    ) -> Arc<dyn ConformanceTurnRunner> {
        Arc::new(Self {
            host,
            journal_faults: Some(faults),
        })
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

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: an unscoped host is a fixture defect"
    )]
    async fn recorded_replay_keys(&self, scope: &crate::ExecutionScope) -> Option<Vec<String>> {
        let scoped = self
            .host
            .scoped(crate::admit(scope.clone()))
            .expect("scope the recorded turn on its host");
        match scoped
            .controller()
            .read_recorded_journal(&crate::RecordedKeyRange {
                lower: String::new(),
                upper: "\u{10FFFF}".to_string(),
                group_key_prefix: String::new(),
            })
            .await
            .expect("read the recorded turn's journal")
        {
            crate::RecordedJournal::Keys(keys) => Some(keys.replay_keys),
            _ => None,
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: an unscoped host is a fixture defect"
    )]
    async fn run_cut_then_redriven_turn(
        &self,
        admitted: crate::AdmittedScope,
        cut: JournalCut,
        attempt: ConformanceTurnAttempt,
        redrive: ConformanceTurnAttempt,
    ) {
        use lash_core::facade_support::effect_replay_driver::EffectJournalFaultPoint;
        let faults = self
            .journal_faults
            .as_ref()
            .unwrap_or_else(|| panic!("this host runner has no journal faults to cut {cut:?}"));
        faults.fail_next(
            match cut.at {
                JournalCutPoint::BeforeEffect => EffectJournalFaultPoint::Claim,
                JournalCutPoint::BeforeResult => EffectJournalFaultPoint::Finalize,
            },
            &cut.replay_key,
        );
        let scoped = self
            .host
            .scoped(admitted.clone())
            .expect("scope the cut conformance turn on its host");
        let end = attempt(scoped).await;
        assert!(faults.fired(), "the journal cut at {cut:?} fired");
        assert!(
            matches!(end, ConformanceTurnEnd::Aborted(_)),
            "the cut at {cut:?} aborts the attempt: {end:?}"
        );
        let scoped = self
            .host
            .scoped(admitted)
            .expect("scope the redriven conformance turn on its host");
        redrive(scoped).await;
    }
}
