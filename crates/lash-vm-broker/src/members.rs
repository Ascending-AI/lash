//! The admitted members of an execution's operations on one activation
//! (ADR 0132 §5, §8).
//!
//! Every tool call a VM operation makes is its own admitted execution: the
//! operation's quiet point admits it under its declared policy, limit and
//! completion wait, and it runs through the one admitted-execution
//! [`Lifecycle`] a turn's round and a process's steps run through. Its body
//! starts only once that admission committed; a `Once` member started
//! without an outcome records `Interrupted`, a `Repeatable` one reruns at
//! its ordinal, and a parked one races its waits. The host decides its
//! operation's answer from the members' committed outcomes alone, so a
//! restore onto the operation answers it the same way without entering a
//! settled body again.
//!
//! A member outlives the operation that admitted it when the host answered
//! before it settled (a race's loser): the ledger keeps its reference, and
//! it settles on its own, at a later pass or when the execution ends.

use std::sync::Arc;
use std::time::Duration;

use lash_core_execution::ActorContext;
use lash_core_execution::runtime::actor::round::lifecycle::{Act, Lifecycle, MemberBodies};
use lash_core_execution::runtime::actor::round::{
    self, AdmittedExecution, PolicyView, RoundError, RunFold, SettledOutput, fold,
};
use lash_durable::domain::{OwnerKey, RunRecordKind, RunRecordRow, RunSeq};
use lash_durable::{CommitLabel, DueSource, DurableError, DurableInstant};
use tokio_util::sync::CancellationToken;

/// One admitted member of the operation a host performs, as its committed
/// records leave it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberEnd {
    /// Its call.
    pub call: lash_sansio::ToolCallId,
    /// Its index in its operation's admission.
    pub member: u64,
    /// Its final outcome, once it has one.
    pub settled: Option<SettledMember>,
}

/// A member's final outcome as its run records hold it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettledMember {
    /// The outcome, with the payload of the material it names.
    pub output: SettledOutput,
    /// Where its outcome record stands in its run: members settled in this
    /// order.
    pub order: u64,
}

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
    rows: Option<Vec<RunRecordRow>>,
}

fn refused(message: impl Into<String>) -> RoundError {
    RoundError::Durable(lash_durable::DurableError::Store(
        lash_durable::StoreFailure {
            kind: lash_durable::StoreFailureKind::Corrupt,
            message: message.into(),
        },
    ))
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

    /// Drop every running body and the cache: the execution opened another
    /// frame, or its owner is gone.
    pub(crate) fn reset(&mut self) {
        if let Some(lifecycle) = self.lifecycle.as_mut() {
            lifecycle.abandon();
        }
        self.lifecycle = None;
        self.rows = None;
    }

    fn lifecycle(&mut self) -> Result<&mut Lifecycle, RoundError> {
        if self.lifecycle.is_none() {
            let bodies = self
                .bodies
                .clone()
                .ok_or_else(|| refused("the execution's host runs no member bodies"))?;
            self.lifecycle = Some(
                Lifecycle::new(
                    &self.cx,
                    self.policies.clone(),
                    bodies,
                    CommitLabel::ROUND_OUTCOME,
                )
                .with_settlement_label(CommitLabel::CELL_INJECT),
            );
        }
        self.lifecycle
            .as_mut()
            .ok_or_else(|| refused("the member lifecycle is gone"))
    }

    /// `members`, admitted by this activation's own quiet point, which
    /// committed: each one's body may run once.
    pub(crate) fn admitted(&mut self, members: &[AdmittedExecution]) -> Result<(), RoundError> {
        self.lifecycle()?.admitted(members);
        self.rows = None;
        Ok(())
    }

    async fn rows(&mut self) -> Result<Vec<RunRecordRow>, RoundError> {
        if let Some(rows) = &self.rows {
            return Ok(rows.clone());
        }
        let rows = self.cx.durable_reads()?.run_records(&self.owner).await?;
        self.rows = Some(rows.clone());
        Ok(rows)
    }

    /// The owner's records, folded under the declared policies.
    pub(crate) async fn fold(&mut self) -> Result<RunFold, RoundError> {
        let rows = self.rows().await?;
        Ok(fold(&rows, &self.policies)?)
    }

    /// Discharge the trace admissions an admission of `folded` still owes
    /// ([`RoundView::owed_trace_exports`]), before any of its members'
    /// bodies runs: the bodies export them (selecting this owner's
    /// candidates, reconciling another owner's), and the export is recorded
    /// (`round.traced`). An owner that takes the execution over after that
    /// record exports none of them again; only one lost between the export
    /// and the record leaves them to be exported twice (FIG-5457). `true`
    /// when it committed or its commit was refused (fold again).
    ///
    /// [`RoundView::owed_trace_exports`]: lash_core_execution::runtime::actor::round::RoundView::owed_trace_exports
    async fn export_traces(&mut self, folded: &RunFold) -> Result<bool, RoundError> {
        let Some(bodies) = self.bodies.clone() else {
            return Ok(false);
        };
        let Some(admitted) = folded
            .rounds()
            .find(|view| !view.owed_trace_exports().is_empty())
            .and_then(|view| folded.admitted_round(view.run()))
        else {
            return Ok(false);
        };
        bodies.export_trace_admissions(admitted.members());
        let mut tx = self.cx.begin().await?;
        round::record_trace_exported(&mut tx, &admitted);
        // A refused record or a lost acknowledgement folds again: a record
        // that landed discharges the exports, and one that did not leaves
        // them owed, which this owner's adapter dedupes.
        match self.cx.commit(tx, CommitLabel::ROUND_TRACED).await {
            Ok(_) => {}
            Err(error @ DurableError::OwnershipLost(_)) => return Err(error.into()),
            Err(_) => {}
        }
        self.rows = None;
        Ok(true)
    }

    /// Act once on the folded records: `None` when it committed or its
    /// commit was refused (fold again), else what it waits on.
    async fn act(
        &mut self,
        folded: &RunFold,
    ) -> Result<Option<lash_core_execution::runtime::actor::round::lifecycle::Idle>, RoundError>
    {
        if self.export_traces(folded).await? {
            return Ok(None);
        }
        match self.lifecycle()?.act(folded).await? {
            Act::Committed(appended) => {
                if let Some(rows) = self.rows.as_mut() {
                    rows.extend(appended);
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

    /// Run the members until `decide` answers run `run`'s operation from
    /// their committed outcomes, or until nothing runs and the execution
    /// stayed hot for `idle_evict`. Once `cancel` fires (the turn's cancel),
    /// every open member is cancelled.
    ///
    /// # Errors
    ///
    /// [`RoundError`]: ownership lost or another store failure, records
    /// that do not fold, the activation stopping, or an operation nothing
    /// can answer.
    pub(crate) async fn drive<T>(
        &mut self,
        run: RunSeq,
        cancel: &CancellationToken,
        decide: &mut (dyn FnMut(&[MemberEnd], DurableInstant) -> Decide<T> + Send),
    ) -> Result<Driven<T>, RoundError> {
        let clock = Arc::clone(self.cx.clock());
        let idle_evict = self.cx.backend().config().settings().idle_evict;
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
            let rows = self.rows().await?;
            let now = self.cx.durable_now().await?;
            let until = match decide(&member_ends(&rows, &folded, run), now) {
                Decide::Answer(answer) => return Ok(Driven::Answered(answer)),
                Decide::Wait { until } => until,
            };
            if idle.quiet() && until.is_none() {
                return Err(refused(format!(
                    "operation {run:?} can never be answered: none of its members is open"
                )));
            }
            // Only rows are left to wait on: stay hot for `idle_evict`,
            // then suspend until the earliest due.
            let hot = if idle.suspendable() {
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

    /// Settle every open member: each is cancelled, and runs to its own
    /// outcome (a running body within its stop grace). What an execution
    /// does before it records its end, so no member outlives the snapshot
    /// that records it.
    ///
    /// # Errors
    ///
    /// As [`drive`](Self::drive).
    pub(crate) async fn close(&mut self) -> Result<(), RoundError> {
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

/// Run `run`'s members as `rows` and their fold leave them, in admission
/// order.
fn member_ends(rows: &[RunRecordRow], folded: &RunFold, run: RunSeq) -> Vec<MemberEnd> {
    let Some(view) = folded.round(run) else {
        return Vec::new();
    };
    view.members()
        .iter()
        .map(|member| MemberEnd {
            call: member.call().clone(),
            member: member.member(),
            settled: member.outcome().map(|output| SettledMember {
                output: output.clone(),
                order: rows
                    .iter()
                    .find(|row| {
                        row.run == run
                            && row.kind == RunRecordKind::XOutcome
                            && row.call.as_ref() == Some(member.call())
                    })
                    .map_or(u64::MAX, |row| row.ordinal.0),
            }),
        })
        .collect()
}
