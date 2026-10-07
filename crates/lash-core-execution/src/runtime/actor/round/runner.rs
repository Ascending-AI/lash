//! The round runner: an admitted round's members run to their final
//! outcomes through the [admitted-execution lifecycle](super::lifecycle),
//! on the owner that holds the round.
//!
//! It folds the round's committed records, hands the fold to the lifecycle
//! and folds again after every commit. Once the turn is cancelled, every
//! unfinished member records `Cancelled`. A round that runs nothing and
//! waits only on rows (its parked members' waits and its retries' due
//! times) stays hot for at most `idle_evict`, then ends
//! [`RoundEnd::Suspended`]: suspension is a state (ADR 0132 §6), and its
//! owner releases the actor as `waiting` until the earliest due, holding
//! nothing. The wake re-claims it, and a runner resumed from the fold
//! carries on.

use std::sync::Arc;

use lash_durable::domain::{OwnerKey, RunRecordRow, RunSeq};
use lash_durable::{CommitLabel, DueSource, DurableError, DurableInstant};
use tokio_util::sync::CancellationToken;

use super::super::ActorContext;
use super::lifecycle::{Act, Lifecycle, MemberBodies};
use super::{AdmittedRound, FoldRefusal, PolicyView, RoundView, RunFold, SettleRefusal, fold};

/// How a round's run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoundEnd {
    /// Every member settled.
    Settled(SettledRound),
    /// Nothing runs: every unsettled member is parked on a wait or has a
    /// retry due, and the round stayed hot for `idle_evict`. Its owner
    /// releases the actor as `waiting` until `due`; a runner resumed from
    /// the fold carries on.
    Suspended {
        /// The earliest retry due time or parked wait deadline.
        due: Option<DurableInstant>,
    },
}

/// A round whose members all settled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettledRound {
    fold: RunFold,
    run: RunSeq,
}

impl SettledRound {
    /// The fold of the round's committed records, every member settled.
    #[must_use]
    pub fn fold(&self) -> &RunFold {
        &self.fold
    }

    /// The round.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "a settled round is built only from a fold that holds its run"
    )]
    pub fn round(&self) -> &RoundView {
        self.fold
            .round(self.run)
            .expect("a settled round's fold holds its round")
    }
}

/// Why a round's run stopped before every member settled.
#[derive(Debug, thiserror::Error)]
pub enum RoundError {
    /// The store refused or failed a read or a commit; ownership lost ends
    /// the run at once.
    #[error(transparent)]
    Durable(#[from] DurableError),
    /// The round's records do not fold.
    #[error(transparent)]
    Fold(#[from] FoldRefusal),
    /// An outcome could not be recorded as it was.
    #[error(transparent)]
    Settle(#[from] SettleRefusal),
    /// The owner has no admission under this run.
    #[error("run {0:?} is not admitted")]
    NotAdmitted(RunSeq),
    /// The activation stopped: the node is going away, and the round's
    /// unfinished members are the next owner's to recover.
    #[error("the activation stopped")]
    Stopped,
}

/// Runs one admitted round of an owner to its members' final outcomes.
pub struct RoundRunner {
    cx: ActorContext,
    owner: OwnerKey,
    run: RunSeq,
    policies: PolicyView,
    lifecycle: Lifecycle,
    cancel: CancellationToken,
}

impl RoundRunner {
    /// The runner of `round`, admitted by this activation's own commit: each
    /// member's body may run once.
    #[must_use]
    pub fn admitted(
        cx: &ActorContext,
        round: &AdmittedRound,
        policies: PolicyView,
        bodies: Arc<dyn MemberBodies>,
    ) -> Option<Self> {
        let owner = round.owner()?.clone();
        let mut runner = Self::resumed(cx, owner, round.run(), policies, bodies);
        runner.lifecycle.admitted(round.members());
        Some(runner)
    }

    /// The runner of `owner`'s run `run`, admitted by an earlier activation:
    /// a member started without an outcome recovers as its records say.
    #[must_use]
    pub fn resumed(
        cx: &ActorContext,
        owner: OwnerKey,
        run: RunSeq,
        policies: PolicyView,
        bodies: Arc<dyn MemberBodies>,
    ) -> Self {
        Self {
            cx: cx.clone(),
            owner,
            run,
            lifecycle: Lifecycle::new(cx, policies.clone(), bodies, CommitLabel::ROUND_OUTCOME),
            policies,
            cancel: CancellationToken::new(),
        }
    }

    /// Cancel the round's unfinished members when `cancel` fires: the turn's
    /// cancel. A running body gets its token cancelled and the stop grace;
    /// a member not running records `Cancelled` at once.
    #[must_use]
    pub fn cancelled_by(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Run every member to its final outcome and answer the fold of the
    /// round's committed records, or suspend once nothing runs and the
    /// round stayed hot for `idle_evict` waiting only on rows.
    ///
    /// # Errors
    ///
    /// [`RoundError`]: ownership lost or another store failure on a read,
    /// inconsistent records, or the activation stopping.
    pub async fn run(mut self) -> Result<RoundEnd, RoundError> {
        let mut rows = load(&self.cx, &self.owner, self.run).await?;
        let clock = Arc::clone(self.cx.clock());
        let idle_evict = self.cx.backend().config().settings().idle_evict;
        // Since when the round has run nothing and waited only on rows.
        let mut quiet_since: Option<std::time::Instant> = None;
        loop {
            let cancelled = self.cancel.is_cancelled();
            if cancelled {
                self.lifecycle.cancel_runs_before(RunSeq(self.run.0 + 1));
            }
            let folded = fold(&rows, &self.policies)?;
            let settled = folded
                .round(self.run)
                .ok_or(RoundError::NotAdmitted(self.run))?
                .settled();
            let idle = match self.lifecycle.act(&folded).await? {
                Act::Committed(appended) => {
                    rows.extend(appended.into_iter().filter(|row| row.run == self.run));
                    quiet_since = None;
                    continue;
                }
                Act::Refused => {
                    rows = load(&self.cx, &self.owner, self.run).await?;
                    continue;
                }
                Act::Idle(idle) => idle,
            };
            if settled && idle.quiet() {
                self.cx.clear_due(DueSource::RetryDue);
                return Ok(RoundEnd::Settled(SettledRound {
                    fold: folded,
                    run: self.run,
                }));
            }
            // Only rows are left to wait on: stay hot for `idle_evict`,
            // then suspend.
            let hot = if idle.suspendable() {
                let since = *quiet_since.get_or_insert_with(|| clock.now());
                let held = clock.now().saturating_duration_since(since);
                if held >= idle_evict {
                    return Ok(RoundEnd::Suspended {
                        due: self.lifecycle.due(&idle).await?,
                    });
                }
                Some(idle_evict - held)
            } else {
                quiet_since = None;
                None
            };
            tokio::select! {
                woke = self.lifecycle.wake(&idle) => woke?,
                () = clock.sleep(hot.unwrap_or_default()), if hot.is_some() => {}
                () = self.cancel.cancelled(), if !cancelled => {}
            }
        }
    }
}

/// `owner`'s committed records of `run`, read once.
async fn load(
    cx: &ActorContext,
    owner: &OwnerKey,
    run: RunSeq,
) -> Result<Vec<RunRecordRow>, RoundError> {
    let rows = cx.durable_reads()?.run_records(owner).await?;
    Ok(rows.into_iter().filter(|row| row.run == run).collect())
}
