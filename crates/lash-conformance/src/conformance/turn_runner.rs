//! Where a turn-executing law runs its turn.
//!
//! A law that executes a real turn takes the turn as a
//! [`ConformanceTurnAttempt`] and hands it to the tier's
//! [`ConformanceTurnRunner`], which supplies the [`ActorContext`] the turn
//! runs on, so one law body states the contract on every store tier. The
//! durable tiers run it in process ([`HostTurnRunner`]) over the durable
//! backend of the store set under test (ADR 0132 §14).
//!
//! # Crashing a turn
//!
//! A crash law picks *where* a turn dies; the runner owns *how* it dies and is
//! recovered, with the tier's own mechanism. Two ways to name the point, each
//! crossed with any runner:
//!
//! - **A panic inside the attempt**
//!   ([`ConformanceTurnRunner::run_crashed_then_redriven_turn`]): the attempt
//!   itself panics before its turn commits, at a point it reaches in its own
//!   task.
//! - **A trigger fired from outside the attempt**
//!   ([`ConformanceTurnRunner::run_turn_until_crash`] with a
//!   [`ConformanceCrash`]): the law parks the turn at any point it observes — a
//!   seam of the crash matrix's `SeamControl`, a store write, a journal entry —
//!   and fires the trigger. The runner then kills the execution where it
//!   stands, the way the process running it dies, and leaves the turn open.
//!   The law can inspect or change durable state before its next
//!   [`ConformanceTurnRunner::run_turn`] of the same scope, which is the tier's
//!   recovery of the crashed turn.
//!
//! The recovery is the tier's own: in process it is a fresh driver over the
//! same backend and store, which restores the turn from committed state. The
//! law asserts only the outcome.

use crate::ActorContext;
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
    /// The turn settled: it finished, or its failure or refusal was recorded.
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

/// One attempt at a turn, run on the scoped controller the tier supplies.
/// A recovered turn runs its attempt again, so an attempt is a factory: every
/// run of it builds its turn afresh from inputs that outlive the run (the
/// runtime is rebuilt; the durable store and host are shared). It reports
/// what it observed through its own channel — only a run that ends reports —
/// and answers how its turn ended.
pub type ConformanceTurnAttempt = Arc<
    dyn Fn(crate::ActorContext) -> Pin<Box<dyn Future<Output = ConformanceTurnEnd> + Send>>
        + Send
        + Sync,
>;

/// The instant a crash law kills a turn's execution: fired from outside the
/// attempt, at a point the law chose (see the module docs).
///
/// Cloning shares the trigger, and it fires once. Every execution of the
/// crashing attempt races the same trigger, because a tier may run an
/// attempt more than once before it dies.
#[derive(Clone, Debug, Default)]
pub struct ConformanceCrash {
    fired: tokio_util::sync::CancellationToken,
}

impl ConformanceCrash {
    /// A trigger that has not fired.
    pub fn new() -> Self {
        Self::default()
    }

    /// Kill the execution the trigger was handed to.
    pub fn fire(&self) {
        self.fired.cancel();
    }

    /// Whether the trigger fired.
    pub fn has_fired(&self) -> bool {
        self.fired.is_cancelled()
    }

    /// Resolves once the trigger fires.
    pub async fn fired(&self) {
        self.fired.cancelled().await;
    }
}

/// Runs a [`ConformanceTurnAttempt`] where the tier runs turns.
#[async_trait::async_trait]
pub trait ConformanceTurnRunner: Send + Sync {
    /// Runs `attempt` to its end on a controller admitted for `admitted`,
    /// once per execution the tier gives it.
    async fn run_turn(&self, admitted: crate::AdmittedScope, attempt: ConformanceTurnAttempt);

    /// Runs one turn across a crash: `crashing` must panic before its turn
    /// commits, and the tier then redelivers the same turn to `redrive` the
    /// way it recovers a crashed turn: a fresh driver over the same host and
    /// store.
    async fn run_crashed_then_redriven_turn(
        &self,
        admitted: crate::AdmittedScope,
        crashing: ConformanceTurnAttempt,
        redrive: ConformanceTurnAttempt,
    );

    /// Runs `attempt` until `crash` fires, then kills that execution where it
    /// stands, the way the process running it dies, and leaves the turn to
    /// the tier's recovery: the law's next [`run_turn`](Self::run_turn) of the
    /// same scope recovers it with a fresh driver over the same host and
    /// store.
    /// Panics when the attempt ended before the crash fired. A runner that
    /// cannot crash a turn says so by panicking.
    async fn run_turn_until_crash(
        &self,
        _admitted: crate::AdmittedScope,
        _attempt: ConformanceTurnAttempt,
        _crash: ConformanceCrash,
    ) {
        panic!("this tier's turn runner cannot crash a turn from outside its attempt");
    }

    /// The law finished one scenario and reads nothing of it again. A tier
    /// that retains per-scenario weight it no longer needs — a server
    /// double's journals of completed invocations — sheds it here, so a law
    /// of many scenarios costs the largest scenario, not their sum
    /// (FIG-4068). Nothing a later scenario observes may change.
    async fn scenario_finished(&self) {}

    /// The replay keys of every effect the tier journaled for `scope`'s
    /// turn, or `None` when this runner cannot read them.
    async fn recorded_replay_keys(&self, _scope: &crate::ExecutionScope) -> Option<Vec<String>> {
        None
    }

    /// Kills every execution of a process segment the tier is running now,
    /// the way the worker running it dies, and leaves each to the tier's
    /// recovery, which resumes it from its committed state. Answers how many
    /// executions it killed; a segment that is waiting on its engine has no
    /// execution to kill, and resumes from its committed state all the same.
    /// A runner that cannot kill a process's worker says so by panicking.
    async fn kill_process_workers(&self) -> usize {
        panic!("this tier's turn runner cannot kill a process's worker");
    }

    /// The process-work wiring for a runtime whose process segments run on
    /// `worker`: the tier's engine serves them
    /// with `worker` and hands back a port that only observes `watched`. A
    /// runner that cannot run process segments says so by panicking.
    fn process_work(
        &self,
        _watched: crate::WatchedRegistry,
        _worker: lash_core_worker::DurableProcessWorker,
    ) -> crate::ProcessWorkWiring {
        panic!("this tier's turn runner cannot run process segments");
    }
}

/// The in-process tiers' runner: the turn is scoped on the host the runtime
/// runs on and executed in the calling task.
pub struct HostTurnRunner {
    host: ActorContext,
}

impl HostTurnRunner {
    /// A runner over `host`, which must be the effect host the law's runtime
    /// is built on: group children route through the executors that host
    /// registered.
    pub fn shared(host: ActorContext) -> Arc<dyn ConformanceTurnRunner> {
        Arc::new(Self { host })
    }
}

#[async_trait::async_trait]
impl ConformanceTurnRunner for HostTurnRunner {
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: an unscoped host is a fixture defect"
    )]
    async fn run_turn(&self, admitted: crate::AdmittedScope, attempt: ConformanceTurnAttempt) {
        let scoped = self
            .host
            .scoped(admitted)
            .expect("scope the conformance turn on its host");
        attempt(scoped).await;
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
    async fn run_turn_until_crash(
        &self,
        admitted: crate::AdmittedScope,
        attempt: ConformanceTurnAttempt,
        crash: ConformanceCrash,
    ) {
        let scoped = self
            .host
            .scoped(admitted)
            .expect("scope the crashing conformance turn on its host");
        // Dropping the attempt's future mid-poll is the in-process tier's
        // crash: its task dies where it stands, as an aborted worker's does.
        tokio::select! {
            biased;
            () = crash.fired() => {}
            end = attempt(scoped) => {
                panic!("the crashing attempt ended ({end:?}) before its crash fired")
            }
        }
    }
}
