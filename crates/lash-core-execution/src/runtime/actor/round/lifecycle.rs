//! The admitted-execution lifecycle (ADR 0132 §5): an owner's admitted
//! members run to their final outcomes on the owner that holds them,
//! whoever the owner is. A turn's tool round ([`RoundRunner`]) and a
//! process's steps (the process activation) run through it; an owner adds
//! only how it learns of its members and what it does once one settles.
//!
//! It works from the owner's committed records and nothing else. The owner
//! folds the rows it knows are committed and hands that fold to
//! [`Lifecycle::act`], which decides from it and commits at most one
//! transaction before the owner folds again:
//!
//! - a member admitted or retried by this activation runs its body; a member
//!   started by an earlier one recovers as the fold says (a `Once` records
//!   `Interrupted`, a `Repeatable` reruns at its ordinal);
//! - each running attempt is a task of its own: what the owner awaits (a
//!   commit, a read) never stops a member mid-transaction, so members never
//!   hold the store's connections while the owner waits on one;
//! - finished members commit in batches under the owner's outcome label,
//!   one transaction per batch, bounded by [`GroupCommit`]'s size and
//!   window;
//! - a `Repeatable` failure the pinned contract repeats records a retry with
//!   its due time (`round.retry`); once due, the next attempt's `x_start`
//!   commits (`round.start`) before its body runs;
//! - a member whose body parked records `Waiting` and races its waits: the
//!   tool completion wait its admission pinned, and the process terminal
//!   its resolver awaits; whichever ends first settles it, and its body is
//!   never entered again;
//! - once its run is cancelled, every unfinished member records
//!   `Cancelled`;
//! - a member's plugin-state resolutions commit in its `x_outcome` and
//!   publish into the resident namespaces from that committed record only:
//!   once its commit is acknowledged, or, for a record an earlier activation
//!   or a lost acknowledgement committed, once the fold holds it, before
//!   anything runs or the owner reads the fold's outcomes. No member sees
//!   another's state before that state is durable.
//!
//! With nothing to commit, `act` answers [`Idle`], what the lifecycle waits
//! on, and the owner races [`Lifecycle::wake`] against its own events. An
//! idle lifecycle that runs no body and holds no outcome
//! ([`Idle::suspendable`]) waits only on rows: parked waits and retry dues.
//! Its owner may release the actor as `waiting` until [`Lifecycle::due`];
//! the wake re-claims it, and a lifecycle built from the fold carries on.
//!
//! A refused or unacknowledged commit drops nothing the lifecycle holds: the
//! owner reads the rows again, and an outcome the store already has is not
//! written twice.
//!
//! # Plugging in an owner
//!
//! An owner admits its members with [`admit`](super::admit) or
//! [`admit_round`](super::admit_round) inside its own transaction and, once
//! that commits, hands the executions to [`Lifecycle::admitted`]. It gives
//! the lifecycle the [`ActorContext`] commits and bodies run under (whose
//! cancel token is the activation's stop), the [`PolicyView`] its current
//! declarations veto stored repeats with, its [`MemberBodies`], and the
//! label its outcomes commit under. Each pass it folds its rows, calls
//! [`Lifecycle::act`], and reads each member's final outcome from the fold:
//! a round presents them, a process hands each to its engine as
//! `StepSettled`. [`Lifecycle::cancel_runs_before`] cancels what it admitted
//! so far.
//!
//! [`GroupCommit`]: lash_durable::GroupCommit
//! [`RoundRunner`]: super::RoundRunner

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use lash_core_store::tool_run::{AvailableEvidence, CompletionSource, LimitCause};
use lash_durable::domain::{AdmittedId, DomainRefusal, RunRecordRow, RunRecordWrite, RunSeq};
use lash_durable::{CommitLabel, DomainWrite, DueSource, DurableError, DurableInstant};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use super::super::ActorContext;
use super::super::waits::{self, RaceWinner, Resolution, WaitId, WaitKind, WaitRef};
use super::{
    AdmittedExecution, Material, PolicyView, Recovery, RoundError, RoundView, RunFold,
    SettledOutput, Stop, StoreLocalEffect, ToolBody, run_bounded, settle, settle_retry,
    start_retry,
};

/// What a member's body answers: its output, and the store-local effects
/// its completion or its park commits with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberResult {
    /// The output.
    pub output: SettledOutput,
    /// The store writes that commit with the output.
    pub store_local: Vec<StoreLocalEffect>,
    /// For a park, the process whose terminal the call also awaits: its
    /// `process_terminal` wait is pinned with the park.
    pub terminal: Option<crate::ProcessId>,
}

impl From<SettledOutput> for MemberResult {
    fn from(output: SettledOutput) -> Self {
        Self {
            output,
            store_local: Vec::new(),
            terminal: None,
        }
    }
}

/// One attempt's body, given the cancel token it must observe.
pub type MemberBody =
    Box<dyn FnOnce(CancellationToken) -> Pin<Box<dyn Future<Output = MemberResult> + Send>> + Send>;

/// The member body of `body`, whose answer is its output alone: no
/// store-local effect, and no process whose terminal it awaits.
pub fn member_body(body: super::ToolBody) -> MemberBody {
    Box::new(move |token| {
        let running = body(token);
        Box::pin(async move { MemberResult::from(running.await) })
    })
}

/// Where an owner's member bodies come from: a round's catalog tools, or a
/// process's steps.
pub trait MemberBodies: Send + Sync {
    /// Observe the committed state of a round before any member runs.
    /// Observers keep their own live deduplication; this runs after every fold.
    fn observe(&self, _round: &RoundView) {}

    /// The body of `execution`'s attempt, for its call and request.
    fn body(&self, execution: &AdmittedExecution) -> MemberBody;

    /// The final answer of `execution`, which parked as `parked`, once one
    /// of its waits ended with `resolution`. Runs no body.
    fn resolved(
        &self,
        execution: &AdmittedExecution,
        parked: &Material<CompletionSource>,
        resolution: Resolution,
    ) -> SettledOutput;

    /// Present `output`, the final answer [`resolved`](Self::resolved) gave
    /// `execution`'s park: what the call's presentation makes of it, which
    /// is recorded as its outcome. A body presents its own answer; a park's
    /// answer is presented here, before its outcome commits, so a crash in
    /// between presents it again. Runs no body.
    fn present<'a>(
        &'a self,
        _execution: &'a AdmittedExecution,
        output: SettledOutput,
    ) -> Presented<'a> {
        Box::pin(async move { output })
    }

    /// Publish `state`, the plugin-state resolutions a member's committed
    /// outcome carries, into the resident namespaces the members' bodies
    /// reduce against. Called once per committed outcome, never before its
    /// commit; a resolution a namespace already holds applies once. An owner
    /// whose bodies reduce no plugin state publishes nothing.
    ///
    /// # Errors
    ///
    /// A resolution a namespace's frontier refuses.
    fn publish_state(
        &self,
        _state: &[crate::plugin::StateResolution],
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        Ok(())
    }

    /// The plugin namespaces the members' run changed that its rows do not
    /// record yet (FIG-5301): a commit that prunes members' records writes
    /// them, so a pruned outcome is never the only copy of a value or of its
    /// dedup receipt. Empty for bodies that reduce no session plugin state.
    fn run_changes(&self) -> Vec<lash_durable::domain::TurnNamespaceWrite> {
        Vec::new()
    }

    /// Record that a commit wrote `written`, what
    /// [`run_changes`](Self::run_changes) named.
    fn run_changes_committed(&self, _written: &[lash_durable::domain::TurnNamespaceWrite]) {}

    /// Release what `execution`'s park launched, once its park ended:
    /// `cancelled` when the call ends cancelled. Runs before the call's
    /// final outcome is recorded, so a crash in between repeats it; it is
    /// idempotent. Runs no body.
    fn discharge<'a>(
        &'a self,
        _execution: &'a AdmittedExecution,
        _parked: &'a Material<CompletionSource>,
        _cancelled: bool,
    ) -> Discharge<'a> {
        Box::pin(async {})
    }
}

/// A park's presented answer: see [`MemberBodies::present`].
pub type Presented<'a> = Pin<Box<dyn Future<Output = SettledOutput> + Send + 'a>>;

/// A park's discharge: see [`MemberBodies::discharge`].
pub type Discharge<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// What one [`Lifecycle::act`] did.
#[derive(Debug)]
pub enum Act {
    /// It committed one transaction, which appended these rows as the
    /// store holds them: fold again.
    Committed(Vec<RunRecordRow>),
    /// Its commit was refused, or its acknowledgement lost: read the rows
    /// again and fold them.
    Refused,
    /// It has nothing to commit: what it waits on.
    Idle(Idle),
}

/// A parked member: the wait it races, its execution, and what it parked on
/// with its pending completion's payload.
type Parked = (WaitRef, AdmittedExecution, Material<CompletionSource>);

/// What an idle lifecycle waits on.
#[derive(Debug)]
pub struct Idle {
    parked: Vec<Parked>,
    retry_due: Option<DurableInstant>,
    busy: bool,
}

impl Idle {
    /// Whether it runs no body, holds no outcome, races no wait and has no
    /// retry due: nothing of the owner's members is open.
    #[must_use]
    pub fn quiet(&self) -> bool {
        !self.busy && self.parked.is_empty() && self.retry_due.is_none()
    }

    /// Whether it runs no body and holds no outcome, so all it waits on is
    /// rows: parked waits and retry dues. Its owner may release the actor
    /// as `waiting` until [`Lifecycle::due`]; nothing is lost.
    #[must_use]
    pub fn suspendable(&self) -> bool {
        !self.busy
    }
}

/// A finished attempt the lifecycle holds until its outcome commits.
struct Finished {
    id: AdmittedId,
    result: Result<MemberResult, Stop>,
}

/// One owner's admitted members on one activation: the bodies it runs and
/// the outcomes it holds until they commit. Never a grant: a new owner
/// builds it again and carries on from the fold.
pub struct Lifecycle {
    cx: ActorContext,
    policies: PolicyView,
    bodies: Arc<dyn MemberBodies>,
    outcome_label: CommitLabel,
    /// The label the outcomes it settles without a body (an interrupted
    /// start, a cancel, a timeout) commit under.
    settlement_label: CommitLabel,
    /// Members of runs before this one are cancelled.
    cancelled_before: RunSeq,
    /// Each run's member cancel, handed to its running bodies.
    tokens: BTreeMap<RunSeq, CancellationToken>,
    fresh: BTreeSet<AdmittedId>,
    /// The attempts running, each a task; dropped with the lifecycle, it
    /// aborts every one still running.
    running: JoinSet<Finished>,
    in_flight: BTreeSet<AdmittedId>,
    finished: BTreeMap<AdmittedId, Result<MemberResult, Stop>>,
    batch_opened: Option<Instant>,
    /// The process-terminal waits this lifecycle checked against their
    /// process's recorded end.
    checked: HashSet<WaitRef>,
    /// The members whose committed plugin state this lifecycle published.
    published: BTreeSet<AdmittedId>,
}

impl Lifecycle {
    /// The lifecycle of the members `cx`'s actor owns, whose bodies come
    /// from `bodies` and whose outcomes commit under `outcome_label`;
    /// `policies` are the current declarations a stored repeat is vetoed
    /// against.
    #[must_use]
    pub fn new(
        cx: &ActorContext,
        policies: PolicyView,
        bodies: Arc<dyn MemberBodies>,
        outcome_label: CommitLabel,
    ) -> Self {
        Self {
            cx: cx.clone(),
            policies,
            bodies,
            outcome_label,
            settlement_label: outcome_label,
            cancelled_before: RunSeq(0),
            tokens: BTreeMap::new(),
            fresh: BTreeSet::new(),
            running: JoinSet::new(),
            in_flight: BTreeSet::new(),
            finished: BTreeMap::new(),
            batch_opened: None,
            checked: HashSet::new(),
            published: BTreeSet::new(),
        }
    }

    /// This lifecycle committing the outcomes it settles without a body
    /// under `label`: a cell's are injected into it (`cell.inject`).
    #[must_use]
    pub fn with_settlement_label(mut self, label: CommitLabel) -> Self {
        self.settlement_label = label;
        self
    }

    /// `members` were admitted by this activation's own commit, which
    /// landed: each one's body may run once.
    pub fn admitted<'a>(&mut self, members: impl IntoIterator<Item = &'a AdmittedExecution>) {
        self.fresh
            .extend(members.into_iter().map(|member| member.id().clone()));
    }

    /// The current declarations a stored repeat is vetoed against, from now
    /// on.
    pub fn declare(&mut self, policies: PolicyView) {
        self.policies = policies;
    }

    /// Cancel every member of the runs before `bound`: a running body gets
    /// its token cancelled and the stop grace; a member not running records
    /// `Cancelled` at once, and none is retried. Members of later runs are
    /// untouched.
    pub fn cancel_runs_before(&mut self, bound: RunSeq) {
        if bound <= self.cancelled_before {
            return;
        }
        self.cancelled_before = bound;
        for (_, token) in self.tokens.range(..bound) {
            token.cancel();
        }
    }

    /// Drop every running body and every outcome held: the owner ended
    /// without them.
    pub fn abandon(&mut self) {
        self.running.abort_all();
        self.in_flight.clear();
        self.finished.clear();
        self.batch_opened = None;
    }

    fn cancelled(&self, run: RunSeq) -> bool {
        run < self.cancelled_before
    }

    fn token(&mut self, run: RunSeq) -> CancellationToken {
        let cancelled = self.cancelled(run);
        self.tokens
            .entry(run)
            .or_insert_with(|| {
                let token = CancellationToken::new();
                if cancelled {
                    token.cancel();
                }
                token
            })
            .clone()
    }

    /// Decide from `folded`, the owner's committed records, and commit at
    /// most one transaction: a batch of finished outcomes, the settlement of
    /// members that need no body, or the starts of retries now due. Starts
    /// the bodies the fold says to run.
    ///
    /// # Errors
    ///
    /// [`RoundError`]: ownership lost, another store failure on a read, or a
    /// record that cannot be written as it is.
    pub async fn act(&mut self, folded: &RunFold) -> Result<Act, RoundError> {
        self.act_with(folded, true).await
    }

    /// [`act`](Self::act) for an owner that is draining: start nothing,
    /// recover nothing and race no wait; only commit the outcomes of bodies
    /// already running. Idle and not [`suspendable`](Idle::suspendable)
    /// while a body runs.
    ///
    /// # Errors
    ///
    /// As [`act`](Self::act).
    pub async fn drain(&mut self, folded: &RunFold) -> Result<Act, RoundError> {
        self.act_with(folded, false).await
    }

    async fn act_with(&mut self, folded: &RunFold, starting: bool) -> Result<Act, RoundError> {
        // What the rows committed publishes before anything reads it: an
        // earlier activation's outcomes on a resume, and outcomes whose
        // acknowledgement was lost.
        for view in folded.rounds() {
            for member in view.members() {
                if member.outcome().is_some() {
                    self.publish(view.id_of(member), member.committed_state())?;
                }
            }
            self.bodies.observe(view);
        }
        // An outcome the rows already have came back on an unacknowledged
        // commit: it is not written again.
        self.finished.retain(|id, result| {
            folded
                .round(id.run)
                .is_some_and(|view| unsettled(view, id, parks(result)))
        });
        // With no outcome held, no batch is open: a window left from a
        // batch whose acknowledgement was lost would wake the owner at
        // once, again and again.
        if self.finished.is_empty() {
            self.batch_opened = None;
        }
        let now = self.cx.durable_now().await?;
        let mut settlements = Vec::new();
        let mut due_starts = Vec::new();
        let mut parked: Vec<Parked> = Vec::new();
        let mut next_due: Option<DurableInstant> = None;
        if starting {
            for view in folded.rounds() {
                let cancelled = self.cancelled(view.run());
                for member in view.members() {
                    let id = view.id_of(member);
                    if self.in_flight.contains(&id) || self.finished.contains_key(&id) {
                        continue;
                    }
                    let execution = view.execution(member);
                    if self.fresh.remove(&id) && !cancelled {
                        self.spawn(execution);
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
                                SettledOutput::Interrupted
                            },
                        )),
                        Recovery::RerunAtOrdinal(_) if cancelled => {
                            settlements.push((execution, cancelled_outcome()));
                        }
                        Recovery::RerunAtOrdinal(_) => self.spawn(execution),
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
                        Recovery::Waiting(source) if cancelled => {
                            self.bodies.discharge(&execution, &source, true).await;
                            settlements.push((execution, cancelled_outcome()));
                        }
                        Recovery::Waiting(source) => {
                            for wait in source_waits(&execution, source.named())? {
                                parked.push((wait, execution.clone(), source.clone()));
                            }
                        }
                    }
                }
            }
            match next_due {
                Some(at) => self.cx.note_due(DueSource::RetryDue, at),
                None => self.cx.clear_due(DueSource::RetryDue),
            }
            // A wait that ended while nobody raced it, or whose deadline
            // passed, settles its member now: a resumed owner never leaves
            // a resolution unread.
            if !parked.is_empty() {
                let racing = self.check_terminals(&parked).await?;
                if let Some(won) = waits::poll(&self.cx, &racing).await? {
                    self.parked_ended(won, &parked).await;
                    parked.clear();
                }
            }
        }

        if let Some(act) = self
            .commit_one(folded, settlements, due_starts, now)
            .await?
        {
            return Ok(act);
        }
        Ok(Act::Idle(Idle {
            parked,
            retry_due: next_due,
            busy: !self.running.is_empty() || !self.finished.is_empty(),
        }))
    }

    /// Commit at most one transaction: a due batch of finished outcomes,
    /// else the settlements, else the starts of due retries.
    async fn commit_one(
        &mut self,
        folded: &RunFold,
        settlements: Vec<(AdmittedExecution, SettledOutput)>,
        due_starts: Vec<AdmittedExecution>,
        now: DurableInstant,
    ) -> Result<Option<Act>, RoundError> {
        let group = self.cx.backend().config().settings().group_commit;
        let batch_due = !self.finished.is_empty()
            && (self.finished.len() >= group.max_members
                || self.running.is_empty()
                || self
                    .batch_opened
                    .is_some_and(|opened| self.cx.clock().now() >= opened + group.window));
        if batch_due {
            let mut tx = self.cx.begin().await?;
            let mut retried = false;
            let mut outcomes = BTreeSet::new();
            for (id, result) in &self.finished {
                let Some(execution) = folded.admitted(id) else {
                    continue;
                };
                match self.record_finished(&mut tx, &execution, result.clone(), now)? {
                    Recorded::Retry => retried = true,
                    Recorded::Outcome => {
                        outcomes.insert(id.clone());
                    }
                }
            }
            let label = if retried && outcomes.is_empty() {
                CommitLabel::ROUND_RETRY
            } else {
                self.outcome_label
            };
            let act = self.commit(tx, label).await?;
            if matches!(act, Act::Committed(_)) {
                // The outcomes are durable: their plugin state publishes now,
                // before the reservations they hold are released.
                for (id, result) in std::mem::take(&mut self.finished) {
                    if let (true, Ok(result)) = (outcomes.contains(&id), &result) {
                        self.publish(id, &staged_state(&result.store_local))?;
                    }
                }
                self.batch_opened = None;
            }
            return Ok(Some(act));
        }
        if !settlements.is_empty() {
            let mut tx = self.cx.begin().await?;
            for (execution, outcome) in settlements {
                settle(&mut tx, &execution, outcome, Vec::new())?;
            }
            return Ok(Some(self.commit(tx, self.settlement_label).await?));
        }
        if !due_starts.is_empty() {
            let mut tx = self.cx.begin().await?;
            let started: Vec<AdmittedExecution> = due_starts
                .iter()
                .map(|failed| start_retry(&mut tx, failed))
                .collect();
            let act = self.commit(tx, CommitLabel::ROUND_START).await?;
            if matches!(act, Act::Committed(_)) {
                self.admitted(&started);
            }
            return Ok(Some(act));
        }
        Ok(None)
    }

    /// Publish `state`, what `id`'s committed outcome carries, unless this
    /// lifecycle published it already.
    fn publish(
        &mut self,
        id: AdmittedId,
        state: &[crate::plugin::StateResolution],
    ) -> Result<(), RoundError> {
        if state.is_empty() || self.published.contains(&id) {
            return Ok(());
        }
        self.bodies
            .publish_state(state)
            .map_err(RoundError::StatePublication)?;
        self.published.insert(id);
        Ok(())
    }

    /// Wait for what `idle` waits on: a body to finish, a parked wait to
    /// end, the batch window to close or a retry to come due. Answers once
    /// one did; the owner acts again. Races none of the owner's own events.
    ///
    /// # Errors
    ///
    /// [`RoundError::Stopped`] when the activation stops; a store failure
    /// of the race, ownership lost among them.
    pub async fn wake(&mut self, idle: &Idle) -> Result<(), RoundError> {
        let clock = Arc::clone(self.cx.clock());
        let group = self.cx.backend().config().settings().group_commit;
        let window = self
            .batch_opened
            .map(|opened| (opened + group.window).saturating_duration_since(clock.now()));
        let now = self.cx.durable_now().await?;
        let until_due = idle.retry_due.map(|at| {
            Duration::from_millis(u64::try_from(at.0.saturating_sub(now.0)).unwrap_or(0))
        });
        let racing: Vec<WaitRef> = idle.parked.iter().map(|(wait, ..)| *wait).collect();
        tokio::select! {
            won = waits::race(&self.cx, &racing), if !racing.is_empty() => {
                self.parked_ended(won?, &idle.parked).await;
            }
            Some(joined) = self.running.join_next(), if !self.running.is_empty() => {
                let done = match joined {
                    Ok(done) => done,
                    Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
                    // Only the runtime shutting down cancels an attempt the
                    // lifecycle still holds.
                    Err(_) => return Err(RoundError::Stopped),
                };
                self.in_flight.remove(&done.id);
                if matches!(done.result, Err(Stop::Activation | Stop::Lapsed)) {
                    return Err(RoundError::Stopped);
                }
                // The durable clock could not be read: no outcome, and the
                // owner retries from its rows.
                if let Err(Stop::Durable(error)) = &done.result {
                    return Err(error.clone().into());
                }
                self.finished.insert(done.id, done.result);
                self.batch_opened.get_or_insert_with(|| clock.now());
            }
            () = sleep_for(&clock, window), if window.is_some() => {}
            () = sleep_for(&clock, until_due), if until_due.is_some() => {}
            () = self.cx.cancel().cancelled() => return Err(RoundError::Stopped),
        }
        Ok(())
    }

    /// The earliest durable instant `idle` waits for: a retry's due time
    /// or a parked wait's deadline. What a release as `waiting` records.
    ///
    /// # Errors
    ///
    /// A store failure reading a parked wait.
    pub async fn due(&mut self, idle: &Idle) -> Result<Option<DurableInstant>, RoundError> {
        let mut due = idle.retry_due;
        let store = self.cx.backend().durable();
        for (wait, ..) in &idle.parked {
            if let Some(deadline) = store
                .wait(&wait.id())
                .await?
                .and_then(|row| row.purpose.deadline())
            {
                due = Some(due.map_or(deadline, |due| due.min(deadline)));
            }
        }
        Ok(due)
    }

    /// The waits `parked` race, each process terminal among them checked
    /// once against its process's recorded end: a process that ended before
    /// its terminal wait was pinned resolved no wait, so it is resolved
    /// from that end, also on a resume after a crash.
    async fn check_terminals(&mut self, parked: &[Parked]) -> Result<Vec<WaitRef>, RoundError> {
        let racing: Vec<WaitRef> = parked.iter().map(|(wait, ..)| *wait).collect();
        for wait in &racing {
            if wait.kind() == WaitKind::ProcessTerminal && self.checked.insert(*wait) {
                waits::resolve_ended_terminal(self.cx.backend(), wait).await?;
            }
        }
        Ok(racing)
    }

    /// Hold the final answers a race's winner gives the parked members,
    /// after discharging what each park launched: the member whose wait
    /// resolved or timed out, or every parked member when the awaiter was
    /// cancelled or the scope revoked its waits.
    async fn parked_ended(&mut self, won: RaceWinner, parked: &[Parked]) {
        let member_of = |wait: &WaitRef| parked.iter().find(|(parked, ..)| parked == wait);
        let mut ends: Vec<(&Parked, SettledOutput)> = Vec::new();
        match won {
            RaceWinner::Resolved { wait, resolution } => {
                if let Some(entry @ (_, execution, source)) = member_of(&wait) {
                    let output = self.bodies.resolved(execution, source, resolution);
                    ends.push((entry, output));
                }
            }
            RaceWinner::TimedOut(wait) => {
                if let Some(entry) = member_of(&wait) {
                    ends.push((
                        entry,
                        SettledOutput::TimedOut {
                            cause: LimitCause::ExecutionTotal,
                            evidence: AvailableEvidence::default(),
                        },
                    ));
                }
            }
            RaceWinner::Cancelled => {
                for entry @ (_, execution, source) in parked {
                    if ends
                        .iter()
                        .all(|((_, end, ..), _)| end.id() != execution.id())
                    {
                        let output = self
                            .bodies
                            .resolved(execution, source, Resolution::Cancelled);
                        ends.push((entry, output));
                    }
                }
            }
        }
        let opened = self.cx.clock().now();
        for ((_, execution, source), output) in ends {
            let output = self.bodies.present(execution, output).await;
            let cancelled = matches!(output, SettledOutput::Cancelled { .. });
            self.bodies.discharge(execution, source, cancelled).await;
            self.finished
                .insert(execution.id().clone(), Ok(output.into()));
            self.batch_opened.get_or_insert(opened);
        }
    }

    /// Commit `tx` under `label`: the rows it appended, or [`Act::Refused`]
    /// when the store refused it or its acknowledgement was lost. Ownership
    /// lost is an error.
    async fn commit(
        &mut self,
        tx: lash_durable::ActorTx,
        label: CommitLabel,
    ) -> Result<Act, RoundError> {
        let appended = appended_rows(tx.domain(), self.cx.epoch());
        match self.cx.commit(tx, label).await {
            Ok(_) => Ok(Act::Committed(appended)),
            Err(error @ DurableError::OwnershipLost(_)) => Err(error.into()),
            // A start its registrar refused after it was staged refuses
            // every repeat of the commit: the activation ends, and the next
            // owner settles the call as its records say, without the start.
            // A `Once` is interrupted; a `Repeatable` runs again, and its
            // start is refused as it stages, so the call settles with the
            // registrar's typed refusal.
            Err(error @ DurableError::Domain(DomainRefusal::ProcessStartRefused { .. })) => {
                Err(error.into())
            }
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
                Ok(Act::Refused)
            }
        }
    }

    fn spawn(&mut self, execution: AdmittedExecution) {
        let body = self.bodies.body(&execution);
        let cx = self.cx.clone();
        let cancel = self.token(execution.id().run);
        self.in_flight.insert(execution.id().clone());
        let attempt = async move {
            type Carried = (Vec<StoreLocalEffect>, Option<crate::ProcessId>);
            let carried: Arc<Mutex<Carried>> = Arc::default();
            let slot = Arc::clone(&carried);
            let tool_body: ToolBody = Box::new(move |token| {
                Box::pin(async move {
                    let result = body(token).await;
                    *slot.lock().unwrap_or_else(PoisonError::into_inner) =
                        (result.store_local, result.terminal);
                    result.output
                })
            });
            let result = run_bounded(&cx, &execution, tool_body, &cancel)
                .await
                .map(|output| {
                    let (store_local, terminal) = std::mem::take(
                        &mut *carried.lock().unwrap_or_else(PoisonError::into_inner),
                    );
                    MemberResult {
                        output,
                        store_local,
                        terminal,
                    }
                });
            Finished {
                id: execution.id().clone(),
                result,
            }
        };
        self.running
            .spawn(attempt.instrument(tracing::Span::current()));
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
        let (mut output, store_local, terminal) = match result {
            Ok(result) => (result.output, result.store_local, result.terminal),
            Err(Stop::Limit(cause)) => (
                SettledOutput::TimedOut {
                    cause,
                    evidence: AvailableEvidence::default(),
                },
                Vec::new(),
                None,
            ),
            Err(Stop::Cancelled | Stop::Activation | Stop::Lapsed) => {
                (cancelled_outcome(), Vec::new(), None)
            }
            Err(Stop::Durable(error)) => return Err(error.into()),
        };
        // A park whose resolver awaits a process waits on its terminal too:
        // the wait is pinned with the park, under the call's own deadline.
        if let (SettledOutput::Waiting(source), Some(process)) = (&mut output, terminal) {
            let (wait, _) = waits::pin(
                tx,
                waits::WaitSpec {
                    kind: WaitKind::ProcessTerminal,
                    scope: waits::wait_scope(&self.cx)?,
                    target_process: Some(process),
                    deadline: execution.draft().wait(),
                },
            )
            .map_err(|refusal| {
                RoundError::Durable(DurableError::Store(lash_durable::StoreFailure {
                    kind: lash_durable::StoreFailureKind::Corrupt,
                    message: refusal.to_string(),
                }))
            })?;
            source.await_terminal(wait.id().to_hex());
        }
        if let Some(due) = self.retry_due(execution, &output, now) {
            settle_retry(tx, execution, output, due)?;
            return Ok(Recorded::Retry);
        }
        settle(tx, execution, output, store_local)?;
        Ok(Recorded::Outcome)
    }

    /// When `outcome` of `execution` is retried: a known failure or a slice
    /// expiry of a `Repeatable` call the current declaration still repeats,
    /// with attempts left and its run not cancelled, whose backoff ends
    /// before its limit does. The backoff is spent from the call's one
    /// limit, never added to it.
    fn retry_due(
        &self,
        execution: &AdmittedExecution,
        outcome: &SettledOutput,
        now: DurableInstant,
    ) -> Option<DurableInstant> {
        let pinned = execution.policy();
        if self.cancelled(execution.id().run)
            || !outcome.may_repeat()
            || !self
                .policies
                .permits_repeat(execution.draft().tool(), pinned)
            || !pinned.permits_repeat(pinned, execution.attempt())
        {
            return None;
        }
        let suggested = match outcome {
            SettledOutput::Failed(failure) => failure.named().suggested_delay_ms,
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

fn cancelled_outcome() -> SettledOutput {
    SettledOutput::Cancelled {
        evidence: AvailableEvidence::default(),
    }
}

/// Whether `id` is still `view`'s open attempt of its member, for a
/// finished result that does (`park`) or does not park it: a park records
/// only on a started attempt, an end on a started or a parked one.
fn unsettled(view: &RoundView, id: &AdmittedId, park: bool) -> bool {
    view.members().iter().any(|member| match member.state() {
        super::MemberState::Started { start, .. } => *start == id.ordinal,
        super::MemberState::Waiting { start, .. } => !park && *start == id.ordinal,
        super::MemberState::RetryDue { .. } | super::MemberState::Final { .. } => false,
    })
}

/// The plugin-state resolutions `effects` stage.
fn staged_state(effects: &[StoreLocalEffect]) -> Vec<crate::plugin::StateResolution> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            StoreLocalEffect::PluginState(staged) => Some(staged.resolutions()),
            _ => None,
        })
        .flatten()
        .cloned()
        .collect()
}

/// Whether a finished result parks its call.
fn parks(result: &Result<MemberResult, Stop>) -> bool {
    matches!(
        result,
        Ok(MemberResult {
            output: SettledOutput::Waiting(_),
            ..
        })
    )
}

/// The waits a parked `execution` races: its tool completion wait, and the
/// process terminal its resolver awaits.
fn source_waits(
    execution: &AdmittedExecution,
    source: &CompletionSource,
) -> Result<Vec<WaitRef>, RoundError> {
    let undecodable = || {
        RoundError::Fold(super::FoldRefusal::Undecodable {
            run: execution.id().run,
            ordinal: execution.ordinal(),
            reason: "a parked call names a wait that is not a wait id".to_owned(),
        })
    };
    let mut refs = vec![WaitRef::new(
        WaitId::parse_hex(&source.wait).ok_or_else(undecodable)?,
        WaitKind::ToolCompletion,
    )];
    if let Some(terminal) = &source.terminal {
        refs.push(WaitRef::new(
            WaitId::parse_hex(terminal).ok_or_else(undecodable)?,
            WaitKind::ProcessTerminal,
        ));
    }
    Ok(refs)
}

/// The run records `writes` append, as the store will hold them.
fn appended_rows(writes: &[DomainWrite], epoch: lash_durable::Epoch) -> Vec<RunRecordRow> {
    writes
        .iter()
        .filter_map(|write| match write {
            DomainWrite::RunRecord(RunRecordWrite::Append {
                owner,
                run,
                ordinal,
                kind,
                call,
                record_json,
            }) => Some(RunRecordRow {
                owner: owner.clone(),
                run: *run,
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
