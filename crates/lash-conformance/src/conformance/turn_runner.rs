//! Where a turn-driving law runs its turn.
//!
//! The in-process tiers scope a turn on the host the runtime runs on and drive
//! it in the test task. A Restate turn exists only inside a handler: its
//! effects journal on a `ctx`-bound controller, and the deployment-level host
//! refuses every effect that has not entered one. A law that drives a real
//! turn therefore takes the turn as a [`ConformanceTurnAttempt`] and hands it
//! to the tier's [`ConformanceTurnRunner`], which supplies the scoped
//! controller the turn runs on — the host's own on the in-process tiers, a
//! handler-bound one on Restate — so one law body states the contract on
//! every tier.
//!
//! # Crashing a turn
//!
//! A crash law picks *where* a turn dies; the runner owns *how* it dies and is
//! recovered, with the tier's own mechanism. Three ways to name the point, each
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
//! - **A journal cut**
//!   ([`ConformanceTurnRunner::run_cut_then_redriven_turn`]): the tier cuts the
//!   attempt at an effect named by its replay key.
//!
//! The recovery is the tier's own. In process it is a fresh driver over the
//! same host and store. On Restate it is a redelivery of the same invocation,
//! which replays the journal the crashed execution left. The law asserts only
//! the outcome both must reach.
//!
//! # Process segments
//!
//! A runner that runs process segments on its engine serves a law's body for a
//! process's segments ([`ConformanceTurnRunner::serve_segments`]), starts one
//! and kills its execution where the law's crash fires
//! ([`ConformanceTurnRunner::run_segment_until_crash`]), and recovers it
//! ([`ConformanceTurnRunner::recover_segment`]) one of two ways
//! ([`SegmentRecovery`]): the engine delivers the execution again over its
//! surviving record, or the record is gone and a fresh execution of the
//! segment arrives. The body is a [`ConformanceTurnAttempt`] over the
//! process-scoped controller the engine lends each execution.

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

/// One attempt at a turn, run on the scoped controller the tier supplies.
/// Restate re-runs a handler from the top whenever it replays the invocation
/// (after a suspension or a failed attempt), so an attempt is a factory: every
/// run of it builds its turn afresh from inputs that outlive the run (the
/// runtime is rebuilt; the durable store and host are shared), so every run
/// issues the same journaled commands. It reports what it observed through
/// its own channel — only a run that ends reports — and answers how its turn
/// ended.
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

/// The instant a crash law kills a turn's execution: fired from outside the
/// attempt, at a point the law chose (see the module docs).
///
/// Cloning shares the trigger, and it fires once. Every execution of the
/// crashing attempt races the same trigger, because Restate may run an
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

/// How a tier recovers a process segment whose execution a crash killed
/// ([`ConformanceTurnRunner::recover_segment`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentRecovery {
    /// The engine delivers the crashed execution again, and its record of the
    /// segment's effects survives for the redelivery to replay.
    Replay,
    /// The engine's record of the crashed execution is gone — retention
    /// purged it — and a fresh execution of the same segment arrives.
    SubstrateLost,
}

/// Runs a [`ConformanceTurnAttempt`] where the tier runs turns.
#[async_trait::async_trait]
pub trait ConformanceTurnRunner: Send + Sync {
    /// Runs `attempt` to its end on a controller admitted for `admitted`,
    /// once per execution the tier gives it: once in process, once per
    /// replay of the handler on Restate.
    async fn run_turn(&self, admitted: crate::AdmittedScope, attempt: ConformanceTurnAttempt);

    /// Runs `attempt` on every retry the tier's engine gives a turn that
    /// parks on each run, until the engine rests the turn, and returns how
    /// many times it ran. A parked turn leaves its execution open (see
    /// [`ConformanceTurnEnd`]); nothing in process retries it, so there it
    /// runs once. On Restate every retry of the open invocation re-runs it,
    /// and the turn handler's retry policy pauses the invocation after its
    /// attempt budget: the runner returns once the invocation is paused, and
    /// panics when a run settled instead of parking.
    async fn run_parking_turn_until_rested(
        &self,
        admitted: crate::AdmittedScope,
        attempt: ConformanceTurnAttempt,
    ) -> usize {
        self.run_turn(admitted, attempt).await;
        1
    }

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

    /// Runs `attempt` until `crash` fires, then kills that execution where it
    /// stands, the way the process running it dies, and leaves the turn to
    /// the tier's recovery: the law's next [`run_turn`](Self::run_turn) of the
    /// same scope recovers it — a fresh driver over the same host in process,
    /// a redelivery of the same invocation replaying its journal on Restate.
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

    /// The fault injector of the effect journal the runner's turns record
    /// into, when the tier's engine keeps one a law can fault: a law arms a
    /// journal claim, finalize or renew error through it. `None` when the
    /// engine's journal is not the law's to fault.
    fn effect_journal_faults(
        &self,
    ) -> Option<lash_core::facade_support::effect_replay_driver::EffectJournalFaults> {
        None
    }

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

    /// Serves every execution of `process_id`'s segments with `body`, in
    /// place of any body served before: the tier runs a fresh `body` on the
    /// process-scoped controller its engine lends each execution, and a body
    /// that ends [`Settled`](ConformanceTurnEnd::Settled) settles the process
    /// successfully. For a process the law starts some other way — a child
    /// that one of its effects starts. A runner that cannot run process
    /// segments says so by panicking.
    async fn serve_segments(&self, process_id: &crate::ProcessId, _body: ConformanceTurnAttempt) {
        panic!("this tier's turn runner cannot serve the segments of process `{process_id}`");
    }

    /// Starts `registration` on the tier's engine, serving its segment with
    /// `body` until `crash` fires, then kills that execution where it stands,
    /// the way the process running it dies, and leaves the segment open for
    /// [`recover_segment`](Self::recover_segment). The registration must
    /// already be recorded in the process registry the tier's engine reads.
    /// Panics when the segment ended before the crash fired. A runner that
    /// cannot run process segments says so by panicking.
    async fn run_segment_until_crash(
        &self,
        registration: crate::ProcessRegistration,
        _body: ConformanceTurnAttempt,
        _crash: ConformanceCrash,
    ) {
        panic!(
            "this tier's turn runner cannot crash a segment of process `{}`",
            registration.id
        );
    }

    /// Recovers the segment of `process_id` a crash left open, the way
    /// `recovery` names, serving any further execution of it with `body`, and
    /// returns once the process reached its terminal.
    async fn recover_segment(
        &self,
        process_id: &crate::ProcessId,
        recovery: SegmentRecovery,
        _body: ConformanceTurnAttempt,
    ) {
        panic!("this tier's turn runner cannot recover process `{process_id}` by {recovery:?}");
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

    fn effect_journal_faults(
        &self,
    ) -> Option<lash_core::facade_support::effect_replay_driver::EffectJournalFaults> {
        self.journal_faults.clone()
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
