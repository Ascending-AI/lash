//! The admitted executions of a run's effects on one activation (ADR 0132
//! §5, §8).
//!
//! Every effect a run performs is its own admitted execution: the park that
//! requested it admits it under its declared policy, limit and completion
//! wait, and it runs through the one admitted-execution [`Lifecycle`] a
//! turn's round and a process's steps run through. Its body starts only
//! once that admission committed; a `Once` member started without an outcome
//! records `Interrupted`, a `Repeatable` one reruns at its ordinal, and a
//! parked one races its waits. Outcomes are read from the committed records
//! alone, so a restore answers the same way without entering a settled body
//! again.
//!
//! A member outlives the wait that admitted it when its task was cancelled
//! or a `join` answered without it: the ledger keeps its reference, and it
//! settles on its own, at a later pass or when the run ends.

use std::sync::Arc;
use std::time::Duration;

use lash_core_execution::ActorContext;
use lash_core_execution::runtime::actor::round::lifecycle::{Act, Lifecycle, MemberBodies};
use lash_core_execution::runtime::actor::round::{
    AdmittedExecution, PinnedWaits, PolicyView, RoundError, RunFold, RunRecords,
};
use lash_core_execution::runtime::turn_control;
use lash_durable::domain::{OwnerKey, RunFacts, RunSeq};
use lash_durable::{CommitLabel, DueSource, DurableError, DurableInstant};
use tokio_util::sync::CancellationToken;

/// What a host decides from its operation's members.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decide<T> {
    /// The operation is answered.
    Answer(T),
    /// Not yet: the members go on, and the host decides again once one
    /// settles or at `until` (an aggregate's timer).
    Wait {
        /// When the host's own answer may change without a member settling.
        until: Option<DurableInstant>,
    },
}

/// How driving an operation's members ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Driven<T> {
    /// The host answered the operation.
    Answered(T),
    /// Nothing runs: every open member waits on a row (a parked wait, a
    /// retry's due time) or a timer, and the execution stayed hot for
    /// `idle_evict`. The earliest due is noted on the actor's context; the
    /// execution suspends on its committed quiet point, and a restore onto
    /// it decides again.
    Suspended,
}

/// A decision over the owner's records, their fold and the store's time.
pub(crate) type DecideFolded<'a, T> =
    dyn FnMut(&RunFacts, &RunFold, DurableInstant) -> Decide<T> + Send + 'a;

/// The members of one execution's owner on one activation: the lifecycle
/// that runs their bodies and the records it reads. Never a grant: a new
/// activation builds it again from the rows.
pub(crate) struct Members {
    cx: ActorContext,
    owner: OwnerKey,
    policies: PolicyView,
    bodies: Option<Arc<dyn MemberBodies>>,
    lifecycle: Option<Lifecycle>,
    /// The owner's records as last read and appended; `None` once a commit
    /// this cache does not see may have changed them.
    rows: Option<RunRecords>,
    /// The completion waits the admissions among `rows` pinned, read with
    /// them: a row a commit appends is never an admission.
    waits: PinnedWaits,
}

/// Why driving an owner's members failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Undriven {
    /// Ownership lost or another store failure, records that do not fold,
    /// or the activation stopping.
    #[error(transparent)]
    Round(#[from] RoundError),
    /// The host bound no member bodies before it drove its members.
    #[error("the execution's host runs no member bodies")]
    Unbound,
    /// Every member is settled or waits on nothing, and the host's decision
    /// still waits: nothing can ever answer it. The records are sound; the
    /// broker's own invariant failed.
    #[error("the operation can never be answered: none of its members is open")]
    Unanswerable,
}

impl From<DurableError> for Undriven {
    fn from(error: DurableError) -> Self {
        Self::Round(error.into())
    }
}

impl Members {
    pub(crate) fn new(cx: &ActorContext, owner: OwnerKey) -> Self {
        Self {
            cx: cx.clone(),
            owner,
            policies: PolicyView::default(),
            bodies: None,
            lifecycle: None,
            rows: None,
            waits: PinnedWaits::default(),
        }
    }

    /// Run member bodies from `bodies`, vetoing a stored repeat against the
    /// `policies` the host declares now.
    pub(crate) fn with_bodies(&mut self, bodies: Arc<dyn MemberBodies>, policies: PolicyView) {
        self.bodies = Some(bodies);
        self.policies = policies;
    }

    /// The member bodies the host runs, once bound.
    pub(crate) fn bodies(&self) -> Option<Arc<dyn MemberBodies>> {
        self.bodies.clone()
    }

    /// Forget the cached records: a commit changed them.
    pub(crate) fn forget(&mut self) {
        self.rows = None;
    }

    fn lifecycle(&mut self) -> Result<&mut Lifecycle, Undriven> {
        let lifecycle = match self.lifecycle.take() {
            Some(lifecycle) => lifecycle,
            // A turn's cell answers its calls under the turn's accepted
            // stop, as its round does: an answer that lands after an
            // `Immediate` cancel was accepted, before the owner saw it, is
            // the stop's, never a call the cell completed (FIG-6082).
            None => Lifecycle::new(
                &self.cx,
                self.policies.clone(),
                self.bodies.clone().ok_or(Undriven::Unbound)?,
                CommitLabel::ROUND_OUTCOME,
            )
            .with_settlement_label(CommitLabel::CELL_INJECT)
            .fenced_by(Arc::new(turn_control::immediate_in)),
        };
        Ok(self.lifecycle.insert(lifecycle))
    }

    /// `members`, admitted by this activation's own quiet point, which
    /// committed: each one's body may run once.
    pub(crate) fn admitted(&mut self, members: &[AdmittedExecution]) -> Result<(), Undriven> {
        self.lifecycle()?.admitted(members);
        self.rows = None;
        Ok(())
    }

    async fn load_rows(&mut self) -> Result<(), RoundError> {
        if self.rows.is_some() {
            return Ok(());
        }
        let reads = self.cx.durable_reads()?;
        let rows = reads.run_facts(&self.owner).await?;
        self.waits = PinnedWaits::read(reads, &rows).await?;
        self.rows = Some(RunRecords::new(rows));
        Ok(())
    }

    /// The owner's records, folded under the declared policies.
    #[expect(
        clippy::expect_used,
        reason = "load_rows installs the activation's records"
    )]
    pub(crate) async fn fold(&mut self) -> Result<RunFold, RoundError> {
        self.load_rows().await?;
        Ok(self
            .rows
            .as_mut()
            .expect("records were loaded")
            .fold(&self.policies, &self.waits)?)
    }

    /// Act once on the folded records: `None` when it committed or its
    /// commit was refused (fold again), else what it waits on.
    async fn act(
        &mut self,
        folded: &RunFold,
    ) -> Result<Option<lash_core_execution::runtime::actor::round::lifecycle::Idle>, Undriven> {
        let now = self.cx.durable_now().await?;
        match self.lifecycle()?.act(folded, now).await? {
            Act::Committed(committed) => {
                if let Some(rows) = self.rows.as_mut() {
                    rows.extend(committed.appended, committed.staged);
                }
                Ok(None)
            }
            Act::Refused => {
                self.rows = None;
                Ok(None)
            }
            Act::Idle(idle) => Ok(Some(idle)),
        }
    }

    /// Run the members of every admission until `decide` answers from the
    /// owner's folded records, or until nothing runs and the execution
    /// stayed hot for `idle_evict`; `quiet` is notified when that hot wait
    /// begins. Once `cancel` fires, every open member is cancelled.
    ///
    /// # Errors
    ///
    /// [`Undriven`]: ownership lost or another store failure, records that
    /// do not fold, the activation stopping, no member bodies bound, or a
    /// decision nothing can answer.
    #[expect(
        clippy::expect_used,
        reason = "load_rows installs the activation's records"
    )]
    pub(crate) async fn drive_by<T>(
        &mut self,
        cancel: &CancellationToken,
        quiet: Option<&tokio::sync::Notify>,
        decide: &mut DecideFolded<'_, T>,
    ) -> Result<Driven<T>, Undriven> {
        let clock = Arc::clone(self.cx.clock());
        let idle_evict = self.cx.backend().owner_retention().idle;
        // Since when the operation has run nothing and waited only on rows.
        let mut quiet_since: Option<std::time::Instant> = None;
        loop {
            let cancelled = cancel.is_cancelled();
            if cancelled {
                self.lifecycle()?.cancel_runs_before(RunSeq(u64::MAX));
            }
            let folded = self.fold().await?;
            let Some(idle) = self.act(&folded).await? else {
                quiet_since = None;
                continue;
            };
            self.load_rows().await?;
            let now = self.cx.durable_now().await?;
            let rows = self.rows.as_ref().expect("records were loaded").facts();
            let until = match decide(rows, &folded, now) {
                Decide::Answer(answer) => return Ok(Driven::Answered(answer)),
                Decide::Wait { until } => until,
            };
            if idle.quiet() && until.is_none() {
                return Err(Undriven::Unanswerable);
            }
            // Only rows are left to wait on: stay hot for `idle_evict`,
            // then suspend until the earliest due.
            let hot = if idle.suspendable() {
                if quiet_since.is_none()
                    && let Some(quiet) = quiet
                {
                    quiet.notify_one();
                }
                let since = *quiet_since.get_or_insert_with(|| clock.now());
                let held = clock.now().saturating_duration_since(since);
                if held >= idle_evict {
                    let lifecycle = self.lifecycle()?;
                    let due = lifecycle.due(&idle).await?;
                    if let Some(due) = due.into_iter().chain(until).min() {
                        self.cx.note_due(DueSource::WaitDeadline, due);
                    }
                    return Ok(Driven::Suspended);
                }
                Some(idle_evict - held)
            } else {
                quiet_since = None;
                None
            };
            let timer = until.map(|at| {
                Duration::from_millis(u64::try_from(at.0.saturating_sub(now.0)).unwrap_or(0))
            });
            let lifecycle = self.lifecycle()?;
            tokio::select! {
                woke = lifecycle.wake(&idle) => woke?,
                () = clock.sleep(hot.unwrap_or_default()), if hot.is_some() => {}
                () = clock.sleep(timer.unwrap_or_default()), if timer.is_some() => {}
                () = cancel.cancelled(), if !cancelled => {}
            }
        }
    }

    /// Settle every open member: each is cancelled for the closing
    /// ([`MemberBodies::closing`]), and runs to its own outcome (a running
    /// body within its stop grace). What an execution
    /// does before it records its end, so no member outlives the snapshot
    /// that records it.
    ///
    /// # Errors
    ///
    /// As [`drive_by`](Self::drive_by).
    pub(crate) async fn close(&mut self) -> Result<(), Undriven> {
        // The bodies learn of the closing first: a stop their members meet
        // is the closing's own.
        if let Some(bodies) = &self.bodies {
            bodies.closing();
        }
        self.lifecycle()?.cancel_runs_before(RunSeq(u64::MAX));
        loop {
            let folded = self.fold().await?;
            let Some(idle) = self.act(&folded).await? else {
                continue;
            };
            if folded.rounds().all(|view| view.settled()) && idle.quiet() {
                return Ok(());
            }
            self.lifecycle()?.wake(&idle).await?;
        }
    }
}
