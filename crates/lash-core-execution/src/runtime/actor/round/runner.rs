//! The round runner: an admitted round's members run to their final
//! outcomes, on the owner that holds the round.
//!
//! It works from the round's committed records and nothing else. Every
//! iteration folds the rows it knows are committed, decides from that fold,
//! and commits at most one transaction before it folds again:
//!
//! - a member admitted or retried by this activation runs its body; a member
//!   started by an earlier one recovers as the fold says (a `Once` records
//!   `Interrupted`, a `Repeatable` reruns at its ordinal);
//! - finished members commit in batches (`round.outcome`), one transaction
//!   per batch, bounded by [`GroupCommit`]'s size and window;
//! - a `Repeatable` failure the pinned contract repeats records a retry with
//!   its due time; once due, the next attempt's `x_start` commits
//!   (`round.start`) before its body runs;
//! - once the turn is cancelled, every unfinished member records
//!   `Cancelled`.
//!
//! A refused or unacknowledged commit drops nothing it holds: the runner
//! reads the rows again, and an outcome the store already has is not
//! written twice.
//!
//! [`GroupCommit`]: lash_durable::GroupCommit

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use lash_core_store::tool_run::{AttemptOutcome, AvailableEvidence, KnownFailure};
use lash_durable::domain::{
    AdmittedId, DomainRefusal, OwnerKey, RunRecordRow, RunRecordWrite, RunSeq,
};
use lash_durable::{CommitLabel, DomainWrite, DueSource, DurableError, DurableInstant};
use tokio_util::sync::CancellationToken;

use super::super::ActorContext;
use super::{
    AdmittedExecution, AdmittedRound, BodyOutput, FoldRefusal, PolicyView, Recovery, RoundView,
    RunFold, SettleRefusal, Stop, StoreLocalEffect, ToolBody, fold, run_bounded, settle,
    settle_retry, start_retry,
};

/// What a member's body answers: its output, and the store-local effect its
/// completion commits with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberResult {
    /// The output.
    pub output: BodyOutput,
    /// The store write that commits with a completion.
    pub store_local: Option<StoreLocalEffect>,
}

impl From<BodyOutput> for MemberResult {
    fn from(output: BodyOutput) -> Self {
        Self {
            output,
            store_local: None,
        }
    }
}

/// One attempt's body, given the cancel token it must observe.
pub type MemberBody =
    Box<dyn FnOnce(CancellationToken) -> Pin<Box<dyn Future<Output = MemberResult> + Send>> + Send>;

/// Where a round's member bodies come from: the catalog's tools.
pub trait MemberBodies: Send + Sync {
    /// The body of `execution`'s attempt, for its call and request.
    fn body(&self, execution: &AdmittedExecution) -> MemberBody;
}

/// How a round's run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundEnd {
    fold: RunFold,
    run: RunSeq,
}

impl RoundEnd {
    /// The fold of the round's committed records, every member settled.
    #[must_use]
    pub fn fold(&self) -> &RunFold {
        &self.fold
    }

    /// The round.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "a round end is built only from a fold that holds its run"
    )]
    pub fn round(&self) -> &RoundView {
        self.fold
            .round(self.run)
            .expect("a round end's fold holds its round")
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

/// A finished attempt the runner holds until its outcome commits.
struct Finished {
    id: AdmittedId,
    result: Result<MemberResult, Stop>,
}

type Running = Pin<Box<dyn Future<Output = Finished> + Send>>;

/// Runs one admitted round of an owner to its members' final outcomes.
pub struct RoundRunner {
    cx: ActorContext,
    owner: OwnerKey,
    run: RunSeq,
    policies: PolicyView,
    bodies: Arc<dyn MemberBodies>,
    cancel: CancellationToken,
    fresh: BTreeSet<AdmittedId>,
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
        Some(Self {
            cx: cx.clone(),
            owner,
            run: round.run(),
            policies,
            bodies,
            cancel: CancellationToken::new(),
            fresh: round
                .members()
                .iter()
                .map(|member| member.id().clone())
                .collect(),
        })
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
            policies,
            bodies,
            cancel: CancellationToken::new(),
            fresh: BTreeSet::new(),
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

    /// Run every member to its final outcome, and answer the fold of the
    /// round's committed records.
    ///
    /// # Errors
    ///
    /// [`RoundError`]: ownership lost or another store failure on a read,
    /// inconsistent records, or the activation stopping.
    pub async fn run(mut self) -> Result<RoundEnd, RoundError> {
        let mut rows = self.load().await?;
        let mut running: FuturesUnordered<Running> = FuturesUnordered::new();
        let mut in_flight: BTreeSet<AdmittedId> = BTreeSet::new();
        let mut finished: BTreeMap<AdmittedId, Result<MemberResult, Stop>> = BTreeMap::new();
        let mut batch_opened: Option<std::time::Instant> = None;
        let group = self.cx.backend().config().settings().group_commit;
        let clock = self.cx.backend().clock();
        loop {
            let folded = fold(&rows, &self.policies)?;
            let view = folded
                .round(self.run)
                .ok_or(RoundError::NotAdmitted(self.run))?
                .clone();
            // An outcome the rows already have came back on an
            // unacknowledged commit: it is not written again.
            finished.retain(|id, _| unsettled(&view, id));
            if view.settled() && in_flight.is_empty() && finished.is_empty() {
                self.cx.clear_due(DueSource::RetryDue);
                return Ok(RoundEnd {
                    fold: folded,
                    run: self.run,
                });
            }
            let cancelled = self.cancel.is_cancelled();
            let now = self.cx.now();

            // Run what this activation admitted; recover what it did not.
            let mut settlements = Vec::new();
            let mut due_starts = Vec::new();
            let mut next_due: Option<DurableInstant> = None;
            for member in view.members() {
                let id = view.id_of(member);
                if in_flight.contains(&id) || finished.contains_key(&id) {
                    continue;
                }
                let execution = view.execution(member);
                if self.fresh.remove(&id) && !cancelled {
                    in_flight.insert(id.clone());
                    running.push(self.spawn(execution));
                    continue;
                }
                match folded
                    .recovery(&id)
                    .cloned()
                    .unwrap_or(Recovery::NotStarted)
                {
                    Recovery::Settled(_) | Recovery::NotStarted => {}
                    Recovery::Interrupt => settlements.push((
                        execution,
                        if cancelled {
                            cancelled_outcome()
                        } else {
                            AttemptOutcome::Interrupted
                        },
                    )),
                    Recovery::RerunAtOrdinal(_) if cancelled => {
                        settlements.push((execution, cancelled_outcome()));
                    }
                    Recovery::RerunAtOrdinal(_) => {
                        in_flight.insert(id.clone());
                        running.push(self.spawn(execution));
                    }
                    Recovery::Vetoed(outcome) => settlements.push((execution, outcome)),
                    Recovery::RetryDue { .. } if cancelled => {
                        settlements.push((execution, cancelled_outcome()));
                    }
                    Recovery::RetryDue { at, .. } => {
                        if at <= now {
                            due_starts.push(execution);
                        } else {
                            next_due = Some(next_due.map_or(at, |due| due.min(at)));
                        }
                    }
                }
            }
            match next_due {
                Some(at) => self.cx.note_due(DueSource::RetryDue, at),
                None => self.cx.clear_due(DueSource::RetryDue),
            }

            // At most one commit per iteration, then fold again.
            let batch_due = !finished.is_empty()
                && (finished.len() >= group.max_members
                    || running.is_empty()
                    || batch_opened.is_some_and(|opened| clock.now() >= opened + group.window));
            if batch_due {
                let mut tx = self.cx.begin().await?;
                let mut retried = false;
                let mut outcomes = false;
                for (id, result) in &finished {
                    let Some(execution) = view.admitted(id) else {
                        continue;
                    };
                    match self.record_finished(&mut tx, &execution, result.clone(), now)? {
                        Recorded::Retry => retried = true,
                        Recorded::Outcome => outcomes = true,
                    }
                }
                let label = if retried && !outcomes {
                    CommitLabel::ROUND_RETRY
                } else {
                    CommitLabel::ROUND_OUTCOME
                };
                if self.commit(tx, label, &mut rows).await? {
                    finished.clear();
                    batch_opened = None;
                }
                continue;
            }
            if !settlements.is_empty() {
                let mut tx = self.cx.begin().await?;
                for (execution, outcome) in settlements {
                    settle(&mut tx, &execution, outcome, None)?;
                }
                self.commit(tx, CommitLabel::ROUND_OUTCOME, &mut rows)
                    .await?;
                continue;
            }
            if !due_starts.is_empty() {
                let mut tx = self.cx.begin().await?;
                let started: Vec<AdmittedExecution> = due_starts
                    .iter()
                    .map(|failed| start_retry(&mut tx, failed))
                    .collect();
                if self.commit(tx, CommitLabel::ROUND_START, &mut rows).await? {
                    self.fresh
                        .extend(started.iter().map(|execution| execution.id().clone()));
                }
                continue;
            }

            // Nothing to commit: wait for a body, the batch window, a retry's
            // due time, the turn's cancel or the activation's stop.
            let window = batch_opened
                .map(|opened| (opened + group.window).saturating_duration_since(clock.now()));
            let until_due = next_due.map(|at| {
                Duration::from_millis(u64::try_from(at.0.saturating_sub(now.0)).unwrap_or(0))
            });
            tokio::select! {
                Some(done) = running.next(), if !running.is_empty() => {
                    in_flight.remove(&done.id);
                    if matches!(done.result, Err(Stop::Activation)) {
                        return Err(RoundError::Stopped);
                    }
                    finished.insert(done.id, done.result);
                    batch_opened.get_or_insert_with(|| clock.now());
                }
                () = sleep_for(&clock, window), if window.is_some() => {}
                () = sleep_for(&clock, until_due), if until_due.is_some() => {}
                () = self.cancel.cancelled(), if !cancelled => {}
                () = self.cx.cancel().cancelled() => return Err(RoundError::Stopped),
            }
        }
    }

    /// The round's committed records, read once.
    async fn load(&self) -> Result<Vec<RunRecordRow>, RoundError> {
        let rows = self.cx.backend().durable().run_records(&self.owner).await?;
        Ok(rows.into_iter().filter(|row| row.run == self.run).collect())
    }

    /// Commit `tx`; on success, the rows it appended join `rows`. A refused
    /// or unacknowledged commit reads the rows again and answers `false`.
    /// Ownership lost is an error.
    async fn commit(
        &self,
        tx: lash_durable::ActorTx,
        label: CommitLabel,
        rows: &mut Vec<RunRecordRow>,
    ) -> Result<bool, RoundError> {
        let appended = appended_rows(tx.domain(), self.run, self.cx.epoch());
        match self.cx.commit(tx, label).await {
            Ok(_) => {
                rows.extend(appended);
                Ok(true)
            }
            Err(error @ DurableError::OwnershipLost(_)) => Err(error.into()),
            Err(error) => {
                if let DurableError::Domain(DomainRefusal::RunOrdinalTaken {
                    owner,
                    run,
                    ordinal,
                }) = &error
                {
                    self.cx
                        .probe()
                        .committed_ordinal_emitted(owner, *run, *ordinal);
                }
                *rows = self.load().await?;
                Ok(false)
            }
        }
    }

    fn spawn(&self, execution: AdmittedExecution) -> Running {
        let body = self.bodies.body(&execution);
        let cx = self.cx.clone();
        let cancel = self.cancel.clone();
        Box::pin(async move {
            let effect: Arc<Mutex<Option<StoreLocalEffect>>> = Arc::default();
            let slot = Arc::clone(&effect);
            let tool_body: ToolBody = Box::new(move |token| {
                Box::pin(async move {
                    let result = body(token).await;
                    *slot.lock().unwrap_or_else(PoisonError::into_inner) = result.store_local;
                    result.output
                })
            });
            let result = run_bounded(&cx, &execution, tool_body, &cancel)
                .await
                .map(|output| MemberResult {
                    output,
                    store_local: effect.lock().unwrap_or_else(PoisonError::into_inner).take(),
                });
            Finished {
                id: execution.id().clone(),
                result,
            }
        })
    }

    /// Record one finished attempt on `tx`: its final outcome with its
    /// store-local effect, or a retry when its pinned contract repeats it.
    fn record_finished(
        &self,
        tx: &mut lash_durable::ActorTx,
        execution: &AdmittedExecution,
        result: Result<MemberResult, Stop>,
        now: DurableInstant,
    ) -> Result<Recorded, RoundError> {
        let (output, store_local) = match result {
            Ok(result) => (result.output, result.store_local),
            Err(Stop::Limit(cause)) => (
                AttemptOutcome::TimedOut {
                    cause,
                    evidence: AvailableEvidence::default(),
                }
                .into(),
                None,
            ),
            Err(Stop::Cancelled | Stop::Activation) => (cancelled_outcome().into(), None),
        };
        if let Some(due) = self.retry_due(execution, &output.outcome, now) {
            settle_retry(tx, execution, output, due)?;
            return Ok(Recorded::Retry);
        }
        settle(tx, execution, output, store_local)?;
        Ok(Recorded::Outcome)
    }

    /// When `outcome` of `execution` is retried: a known failure or a slice
    /// expiry of a `Repeatable` call the current declaration still repeats,
    /// with attempts left, whose backoff ends before its limit does. The
    /// backoff is spent from the call's one limit, never added to it.
    fn retry_due(
        &self,
        execution: &AdmittedExecution,
        outcome: &AttemptOutcome,
        now: DurableInstant,
    ) -> Option<DurableInstant> {
        let pinned = execution.policy();
        if self.cancel.is_cancelled()
            || !outcome.may_repeat()
            || !self
                .policies
                .permits_repeat(execution.draft().tool(), pinned)
            || !pinned.permits_repeat(pinned, execution.attempt())
        {
            return None;
        }
        let suggested = match outcome {
            AttemptOutcome::Failed(KnownFailure {
                suggested_delay_ms, ..
            }) => *suggested_delay_ms,
            _ => None,
        };
        let delay = pinned.delay_ms_for_retry(execution.attempt() - 1, suggested);
        let due = now
            .0
            .saturating_add(i64::try_from(delay).unwrap_or(i64::MAX));
        let expires_at = i64::try_from(execution.limit().expires_at).unwrap_or(i64::MAX);
        (due < expires_at).then_some(DurableInstant(due))
    }
}

enum Recorded {
    Retry,
    Outcome,
}

fn cancelled_outcome() -> AttemptOutcome {
    AttemptOutcome::Cancelled {
        evidence: AvailableEvidence::default(),
    }
}

/// Whether `id` is still `view`'s open attempt of its member.
fn unsettled(view: &RoundView, id: &AdmittedId) -> bool {
    view.members().iter().any(|member| {
        member.outcome().is_none()
            && matches!(member.state(), super::MemberState::Started { start, .. } if *start == id.ordinal)
    })
}

/// The run records `writes` append to `run`, as the store will hold them.
fn appended_rows(
    writes: &[DomainWrite],
    run: RunSeq,
    epoch: lash_durable::Epoch,
) -> Vec<RunRecordRow> {
    writes
        .iter()
        .filter_map(|write| match write {
            DomainWrite::RunRecord(RunRecordWrite::Append {
                owner,
                run: written,
                ordinal,
                kind,
                call,
                record_json,
            }) if *written == run => Some(RunRecordRow {
                owner: owner.clone(),
                run: *written,
                ordinal: *ordinal,
                kind: *kind,
                call: call.clone(),
                record_json: record_json.clone(),
                written_epoch: epoch,
            }),
            _ => None,
        })
        .collect()
}

async fn sleep_for(clock: &Arc<dyn crate::Clock>, duration: Option<Duration>) {
    clock.sleep(duration.unwrap_or_default()).await;
}
