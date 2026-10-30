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
//! - a member whose body answered a capacity verdict
//!   ([`CapacityVerdict`]) from a `Repeatable` call the current declaration
//!   still repeats records nothing: the lifecycle holds it aside, never
//!   starts it again on this activation, and reports it in [`Idle::capacity`]
//!   so its owner backs off and ends the activation; the next claim reruns
//!   it at its ordinal, uncounted. A verdict from any other call records
//!   `Interrupted`;
//! - a member whose body parked records `Waiting` and races its waits: the
//!   tool completion wait its admission pinned, and the process terminal
//!   its resolver awaits; whichever ends first settles it, and its body is
//!   never entered again;
//! - once its run is cancelled, every unfinished member records
//!   `Cancelled`;
//! - an owner's fence ([`Lifecycle::fenced_by`]) is read in the
//!   transaction that commits finished answers: once it holds, every run is
//!   cancelled, and an answer that is not the member's own cancellation
//!   commits `Cancelled`, with nothing it staged, so a member that answers
//!   after its owner's stop was accepted publishes nothing (FIG-5381);
//! - an outcome a member's body did not answer (a settlement without a
//!   body, a body dropped at its limit or its stop) is handed to its
//!   owner's bodies once it commits ([`MemberBodies::unanswered`]);
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
//! `StepSettled`. [`Lifecycle::request_cancels_before`] cancels what it admitted
//! so far.
//!
//! [`GroupCommit`]: lash_durable::GroupCommit
//! [`RoundRunner`]: super::RoundRunner

mod attempt;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lash_core_store::tool_run::AvailableEvidence;
use lash_core_store::tool_run::CompletionSource;
use lash_durable::domain::{AdmittedId, DomainRefusal, RunFacts, RunSeq};
use lash_durable::{CommitLabel, DueSource, DurableError, DurableInstant};
use lash_vocabulary::tool_contract::LimitCause;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::super::ActorContext;
use super::super::waits;
use super::super::waits::Resolution;
use super::super::waits::WaitId;
use super::{
    AdmittedExecution, PolicyView, Recovery, RoundError, RoundView, RunFold, StagedRecord, Stop,
    StoreLocalEffect, settle_retry, settle_staged, start_retry,
};
use lash_core_store::tool_run::Material;
use lash_core_store::tool_run::SettledOutput;
use lash_durable::domain::WaitKind;
use lash_execution_contracts::process::CapacityVerdict;
use lash_execution_contracts::process::EngineStepAnswer;
use lash_execution_contracts::process::StepCandidate;
use lash_execution_contracts::waits::ParkDeadline;
use lash_execution_contracts::waits::RaceWinner;
use lash_execution_contracts::waits::WaitRef;

/// What a member's body answers: its output, and the store-local effects
/// its completion or its park commits with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberOutcome {
    /// The output.
    pub output: SettledOutput,
    /// The store writes that commit with the output.
    pub store_local: Vec<StoreLocalEffect>,
    /// For a park, the process whose terminal the call also awaits: its
    /// `process_terminal` wait is pinned with the park.
    pub terminal: Option<crate::ProcessId>,
    /// Absolute deadline supplied by an ordinary operation; pinned with its park.
    pub timer: Option<DurableInstant>,
    /// The host lacked capacity for the attempt
    /// ([`CapacityVerdict`]): honoured, nothing is recorded and the
    /// member runs again uncounted; refused, `output` is recorded.
    pub capacity: Option<CapacityVerdict>,
    /// The heap pages a completion publishes with its outcome
    /// ([`EngineStepAnswer::Paged`]): the outcome's run fact owns them.
    pub candidate: Option<StepCandidate>,
}

impl From<SettledOutput> for MemberOutcome {
    fn from(output: SettledOutput) -> Self {
        Self {
            output,
            store_local: Vec::new(),
            terminal: None,
            timer: None,
            capacity: None,
            candidate: None,
        }
    }
}

impl From<EngineStepAnswer> for MemberOutcome {
    /// An engine step's answer: its output, or its capacity verdict, whose
    /// output when the lifecycle refuses it is `Interrupted`, an attempt
    /// that started and never settled.
    fn from(answer: EngineStepAnswer) -> Self {
        match answer {
            EngineStepAnswer::Settled(output) => (*output).into(),
            EngineStepAnswer::Paged { output, candidate } => Self {
                candidate: Some(candidate),
                ..(*output).into()
            },
            EngineStepAnswer::Capacity(verdict) => Self {
                capacity: Some(verdict),
                ..SettledOutput::Interrupted.into()
            },
        }
    }
}

/// One attempt's body, given the cancel token it must observe.
pub type MemberBody = Box<
    dyn FnOnce(CancellationToken) -> Pin<Box<dyn Future<Output = MemberOutcome> + Send>> + Send,
>;

/// The member body of `body`, whose answer is its output alone: no
/// store-local effect, and no process whose terminal it awaits.
pub fn member_body(body: super::ToolBody) -> MemberBody {
    Box::new(move |token| {
        let running = body(token);
        Box::pin(async move { MemberOutcome::from(running.await) })
    })
}

/// Where an owner's member bodies come from: a round's catalog tools, or a
/// process's steps.
pub trait MemberBodies: Send + Sync {
    /// Observe the committed state of a round before any member runs.
    /// Observers keep their own live deduplication; this runs after every fold.
    /// `admitted_here` holds the members this activation's own commit
    /// admitted and has not started yet; `settled_here` the members whose
    /// final outcomes this activation's acknowledged commit wrote, which
    /// only this owner closes (P1). A successor that folds an outcome
    /// another owner committed closes nothing.
    fn observe(
        &self,
        _round: &RoundView,
        _admitted_here: &BTreeSet<AdmittedId>,
        _settled_here: &BTreeSet<AdmittedId>,
    ) {
    }

    /// The body of `execution`'s attempt, for its call and request.
    fn body(&self, execution: &AdmittedExecution) -> MemberBody;

    /// How long a body whose token was cancelled may still answer before it
    /// is dropped: the runtime's
    /// [`ExecutionBudgets::stop_grace`](lash_protocol_types::ExecutionBudgets::stop_grace).
    fn stop_grace(&self) -> std::time::Duration;

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

    /// Project an owner's outstanding claim obligations onto a lifecycle
    /// commit, atomically with the outcomes it records. Session contexts
    /// already reduce theirs; process bodies supply their driver facts.
    fn stage_commit(
        &self,
        _tx: &mut lash_durable::ActorTx,
        _epoch: lash_durable::Epoch,
    ) -> Result<(), DurableError> {
        Ok(())
    }

    /// Restore a protected final's reservations before any body runs.
    fn reserve_state(
        &self,
        _state: &[crate::plugin::StateResolution],
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        Ok(())
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

    /// The owner's run is closing: every open member is cancelled for it
    /// next, and a stop its bodies meet is the closing's own cause.
    fn closing(&self) {}

    /// `execution` committed `output`, which no body of it answered: a
    /// settlement without a body, or a body dropped at its limit or its
    /// stop. Called once its commit is acknowledged; a body that answered
    /// observed its own outcome.
    fn unanswered(&self, _execution: &AdmittedExecution, _output: &SettledOutput) {}

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
        Box::pin(async { Ok(()) })
    }
}

/// A park's presented answer: see [`MemberBodies::present`].
pub type Presented<'a> = Pin<Box<dyn Future<Output = SettledOutput> + Send + 'a>>;

/// A park's discharge: see [`MemberBodies::discharge`].
pub type Discharge<'a> =
    Pin<Box<dyn Future<Output = Result<(), lash_execution_contracts::PluginError>> + Send + 'a>>;

/// What one [`Lifecycle::act`] did.
#[derive(Debug)]
pub enum Act {
    /// It committed one transaction: fold again.
    Committed(Committed),
    /// Its commit was refused, or its acknowledgement lost: read the rows
    /// again and fold them.
    Refused,
    /// It has nothing to commit: what it waits on.
    Idle(Idle),
}

/// One acknowledged lifecycle commit: what its owner's fold gains, and the
/// actor revision it left.
#[derive(Debug)]
pub struct Committed {
    /// The facts it appended, as the store holds them.
    pub appended: RunFacts,
    /// The actor's state revision after it.
    pub revision: lash_durable::StateRevision,
    /// The records it staged, as their owner's fold takes them
    /// ([`RunRecords::extend`](super::RunRecords::extend)).
    pub staged: Vec<StagedRecord>,
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
    capacity: Option<CapacityVerdict>,
}

impl Idle {
    /// Whether it runs no body, holds no outcome, races no wait and has no
    /// retry due: nothing of the owner's members is open.
    #[must_use]
    pub fn quiet(&self) -> bool {
        !self.busy && self.parked.is_empty() && self.retry_due.is_none() && self.capacity.is_none()
    }

    /// A capacity verdict this activation holds a member on, with the
    /// longest `retry_after` of those it holds: the member has no outcome,
    /// and runs again on a later claim ([`CapacityVerdict`]).
    #[must_use]
    pub fn capacity(&self) -> Option<&CapacityVerdict> {
        self.capacity.as_ref()
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
    held: HeldOutcome,
}

struct HeldOutcome {
    result: Result<MemberOutcome, Stop>,
    final_at_outcome: bool,
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
    final_sender: tokio::sync::mpsc::UnboundedSender<super::decision::FinalRequest>,
    final_receiver: tokio::sync::mpsc::UnboundedReceiver<super::decision::FinalRequest>,
    final_requests: BTreeMap<AdmittedId, super::decision::FinalRequest>,
    cancel_requests: BTreeMap<crate::ToolCallId, crate::ToolCancellation>,
    refresh_controls: bool,
    /// The owner's stop, read in the transaction that commits finished
    /// answers.
    fence: Option<Fence>,
    /// Only a committed call decision may signal this call's body.
    tokens: BTreeMap<crate::ToolCallId, CancellationToken>,
    fresh: BTreeSet<AdmittedId>,
    /// The attempts running, each a task; dropped with the lifecycle, it
    /// aborts every one still running.
    running: JoinSet<Finished>,
    in_flight: BTreeSet<AdmittedId>,
    finished: BTreeMap<AdmittedId, HeldOutcome>,
    /// Members whose attempt met a capacity verdict this lifecycle
    /// honoured: no outcome, and never started again on this activation.
    capacity: BTreeMap<AdmittedId, CapacityVerdict>,
    batch_opened: Option<Instant>,
    /// The process-terminal waits this lifecycle checked against their
    /// process's recorded end.
    checked: HashSet<WaitRef>,
    /// The members whose committed plugin state this lifecycle published.
    published: BTreeSet<AdmittedId>,
    /// The members whose outcomes this activation's acknowledged commits
    /// wrote, until the next fold observes them.
    settled: BTreeSet<AdmittedId>,
    prepare_final: bool,
    /// A final batch held for the owner's next output, with the members
    /// whose outcomes it writes and the records it staged.
    prepared: Option<Prepared>,
}

/// A final batch held for the owner's next output: its transaction, the
/// members whose outcomes it writes, and the records it staged.
pub(super) type Prepared = (
    lash_durable::ActorTx,
    BTreeSet<AdmittedId>,
    Vec<StagedRecord>,
);

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
        let (final_sender, final_receiver) = tokio::sync::mpsc::unbounded_channel();
        Self {
            cx: cx.clone(),
            policies,
            bodies,
            outcome_label,
            settlement_label: outcome_label,
            final_sender,
            final_receiver,
            final_requests: BTreeMap::new(),
            cancel_requests: BTreeMap::new(),
            refresh_controls: false,
            fence: None,
            tokens: BTreeMap::new(),
            fresh: BTreeSet::new(),
            running: JoinSet::new(),
            in_flight: BTreeSet::new(),
            finished: BTreeMap::new(),
            capacity: BTreeMap::new(),
            batch_opened: None,
            checked: HashSet::new(),
            published: BTreeSet::new(),
            settled: BTreeSet::new(),
            prepare_final: false,
            prepared: None,
        }
    }

    /// This lifecycle committing the outcomes it settles without a body
    /// under `label`: a cell's are injected into it (`cell.inject`).
    #[must_use]
    pub fn with_settlement_label(mut self, label: CommitLabel) -> Self {
        self.settlement_label = label;
        self
    }

    pub(super) fn prepare_final(&mut self, enabled: bool) {
        self.prepare_final = enabled;
    }

    pub(super) fn take_prepared(&mut self) -> Option<Prepared> {
        self.prepared.take()
    }

    /// This lifecycle reading `fence` in every transaction that commits
    /// finished answers: once it holds on that transaction's open read,
    /// every run is cancelled, and an answer that is not the member's own
    /// cancellation commits `Cancelled`, with nothing it staged. The owner's
    /// accepted stop, not its live watch, decides what a late answer may
    /// publish.
    #[must_use]
    pub fn fenced_by(mut self, fence: Fence) -> Self {
        self.fence = Some(fence);
        self
    }

    /// `members` were admitted by this activation's own commit, which
    /// landed: each one's body may run once.
    pub fn admitted<'a>(&mut self, members: impl IntoIterator<Item = &'a AdmittedExecution>) {
        self.fresh
            .extend(members.into_iter().map(|member| member.id().clone()));
    }

    /// `members`' outcomes were written by this activation's own commit,
    /// which landed: the next fold observes them as settled here.
    pub fn settled_here<'a>(&mut self, members: impl IntoIterator<Item = &'a AdmittedExecution>) {
        self.settled
            .extend(members.into_iter().map(|member| member.id().clone()));
    }

    /// The current declarations a stored repeat is vetoed against, from now
    /// on.
    pub fn declare(&mut self, policies: PolicyView) {
        self.policies = policies;
    }

    /// Submit an explicit request. The owner's next act resolves the call
    /// through its durable membership and commits the winning decision before
    /// signalling its token. An unknown call has no effect.
    pub fn request_cancel(
        &mut self,
        call: crate::ToolCallId,
        cancellation: crate::ToolCancellation,
    ) {
        self.cancel_requests.entry(call).or_insert(cancellation);
    }

    /// Submit the same per-call request for every outstanding member selected
    /// from the committed fold. A run stop and cell closing use this path too.
    pub fn request_cancels_before(
        &mut self,
        folded: &RunFold,
        bound: RunSeq,
        cancellation: crate::ToolCancellation,
    ) {
        for view in folded.rounds().filter(|view| view.run() < bound) {
            for member in view
                .members()
                .iter()
                .filter(|member| member.outcome().is_none())
            {
                self.request_cancel(member.call().clone(), cancellation.clone());
            }
        }
    }

    fn read_host_cancels(&mut self, tx: &lash_durable::ActorTx, folded: &RunFold) {
        for request in tx.call_cancels() {
            let owns = match folded.rounds().next().map(|view| view.owner()) {
                Some(
                    lash_durable::domain::OwnerKey::Turn(session, run)
                    | lash_durable::domain::OwnerKey::Cell(session, run, _),
                ) => session == request.session && run == request.run,
                _ => false,
            };
            if owns {
                let mut cancellation = crate::ToolCancellation::runtime(
                    request
                        .reason
                        .clone()
                        .unwrap_or_else(|| "the host cancelled this call".to_owned()),
                )
                .with_origin(crate::CancelOrigin::OperatorRequested);
                cancellation.raw = Some(crate::ToolValue::untrusted_json(serde_json::json!({
                    "request_id": request.request_id, "origin": request.origin,
                })));
                self.request_cancel(request.call.clone(), cancellation);
            }
        }
    }

    async fn commit_cancels(
        &mut self,
        folded: &RunFold,
        attempt: Option<&lash_durable::ActorTx>,
    ) -> Result<Option<Act>, RoundError> {
        if self.cancel_requests.is_empty() && attempt.is_none() && !self.refresh_controls {
            return Ok(None);
        }
        let mut tx = match attempt {
            Some(tx) => std::borrow::Cow::Borrowed(tx),
            None => std::borrow::Cow::Owned(self.cx.begin().await?),
        };
        self.refresh_controls = false;
        self.read_host_cancels(&tx, folded);
        if self.fence.as_ref().is_some_and(|fence| fence(&tx)) {
            self.request_cancels_before(
                folded,
                RunSeq(u64::MAX),
                crate::ToolCancellation::runtime("the owning turn stopped")
                    .with_origin(crate::CancelOrigin::TurnStopped),
            );
        }
        let mut staged = Vec::new();
        for view in folded.rounds() {
            for member in view.members() {
                let Some(cancellation) = self.cancel_requests.get(member.call()) else {
                    continue;
                };
                if member.outcome().is_none()
                    && member.cancellation().is_none()
                    && member.final_payload().is_none()
                {
                    let execution = view.execution(member);
                    staged.extend(super::rounds::cancel_call(
                        tx.to_mut(),
                        &execution,
                        cancellation.clone(),
                    ));
                }
            }
        }
        if staged.is_empty() {
            self.cancel_requests.clear();
            return Ok(None);
        }
        self.commit(tx.into_owned(), CommitLabel::CALL_CANCEL, staged)
            .await
            .map(Some)
    }

    async fn commit_finals(
        &mut self,
        folded: &RunFold,
        attempt: Option<&lash_durable::ActorTx>,
    ) -> Result<Option<Act>, RoundError> {
        while let Ok(request) = self.final_receiver.try_recv() {
            self.final_requests
                .insert(request.execution.id().clone(), request);
        }
        let decided: Vec<_> = self
            .final_requests
            .keys()
            .filter_map(|id| {
                let member = folded.round(id.run)?.member_started_at(id.ordinal)?;
                (member.final_payload().is_some()
                    || member.cancellation().is_some()
                    || member.outcome().is_some())
                .then(|| (id.clone(), member.final_payload().is_some()))
            })
            .collect();
        for (id, won) in decided {
            if let Some(request) = self.final_requests.remove(&id) {
                if won {
                    request.protected.cancel();
                }
                let _ = request.reply.send(won);
            }
        }
        self.final_requests
            .retain(|_, request| !request.reply.is_closed());
        if self.final_requests.is_empty() {
            return Ok(None);
        }
        let mut tx = match attempt {
            Some(tx) => tx.clone(),
            None => self.cx.begin().await?,
        };
        // The stop may have arrived since commit_cancels read its transaction.
        self.read_host_cancels(&tx, folded);
        if self.fence.as_ref().is_some_and(|fence| fence(&tx)) {
            self.request_cancels_before(
                folded,
                RunSeq(u64::MAX),
                crate::ToolCancellation::runtime("the owning turn stopped")
                    .with_origin(crate::CancelOrigin::TurnStopped),
            );
            return Ok(Some(Act::Refused));
        }
        if self.final_requests.keys().any(|id| {
            folded
                .admitted(id)
                .is_some_and(|execution| self.cancel_requests.contains_key(execution.call()))
        }) {
            return Ok(Some(Act::Refused));
        }
        let mut staged = Vec::new();
        for (id, request) in &self.final_requests {
            if let Some(execution) = folded.admitted(id) {
                staged.push(super::rounds::final_call(
                    &mut tx,
                    &execution,
                    request.payload.clone(),
                ));
            }
        }
        if staged.is_empty() {
            return Ok(None);
        }
        self.commit(tx, CommitLabel::CALL_FINAL, staged)
            .await
            .map(Some)
    }

    /// The attempt is running here, or its outcome awaits this owner's commit.
    pub(crate) fn holds_execution(&self, id: &AdmittedId) -> bool {
        self.in_flight.contains(id)
            || self.finished.contains_key(id)
            || self.capacity.contains_key(id)
    }

    /// Whether it runs a body or holds an outcome not yet committed.
    #[must_use]
    pub fn busy(&self) -> bool {
        !self.running.is_empty() || !self.finished.is_empty()
    }

    /// Drop every running body and every outcome held: the owner ended
    /// without them.
    pub fn abandon(&mut self) {
        self.running.abort_all();
        self.in_flight.clear();
        self.finished.clear();
        self.capacity.clear();
        self.batch_opened = None;
    }

    fn token(&mut self, call: &crate::ToolCallId) -> CancellationToken {
        self.tokens.entry(call.clone()).or_default().clone()
    }

    /// Decide from `folded`, the owner's committed records, as of `now`,
    /// the store's clock as the owner last read it, and commit at most one
    /// transaction: a batch of finished outcomes, the settlement of members
    /// that need no body, or the starts of retries now due. Starts the
    /// bodies the fold says to run.
    ///
    /// # Errors
    ///
    /// [`RoundError`]: ownership lost, another store failure on a read, or a
    /// record that cannot be written as it is.
    pub async fn act(&mut self, folded: &RunFold, now: DurableInstant) -> Result<Act, RoundError> {
        self.act_with(folded, true, Some(now), None).await
    }

    /// Read the store clock only when a retry needs a due comparison.
    /// Outcome backoffs use their own transaction's validated clock.
    pub(crate) async fn act_current(&mut self, folded: &RunFold) -> Result<Act, RoundError> {
        self.act_with(folded, true, None, None).await
    }

    /// [`act`](Self::act) for an owner that is draining: start nothing,
    /// recover nothing and race no wait; only commit the outcomes of bodies
    /// already running. Idle and not [`suspendable`](Idle::suspendable)
    /// while a body runs.
    ///
    /// # Errors
    ///
    /// As [`act`](Self::act).
    pub async fn drain(
        &mut self,
        folded: &RunFold,
        now: DurableInstant,
    ) -> Result<Act, RoundError> {
        self.act_with(folded, false, Some(now), None).await
    }

    /// Use the process pass's coherent open; never grant a newer basis to
    /// outcomes classified from that pass's rows.
    pub(crate) async fn act_in(
        &mut self,
        folded: &RunFold,
        now: DurableInstant,
        tx: &lash_durable::ActorTx,
        starting: bool,
    ) -> Result<Act, RoundError> {
        self.act_with(folded, starting, Some(now), Some(tx)).await
    }

    /// Restore committed namespaces before the process drops consumed runs
    /// from its live fold. This runs no body and writes nothing.
    pub(crate) fn publish_committed(&mut self, folded: &RunFold) -> Result<(), RoundError> {
        for view in folded.rounds() {
            for member in view.members() {
                if member.outcome().is_some() {
                    self.publish(view.id_of(member), member.committed_state())?;
                }
            }
        }
        // Committed outcomes publish before pending finals reserve their namespaces.
        for view in folded.rounds() {
            for member in view
                .members()
                .iter()
                .filter(|member| member.outcome().is_none())
            {
                if let Some(payload) = member.final_payload() {
                    let state = crate::tool_dispatch::protected_state(payload)
                        .map_err(RoundError::StatePublication)?;
                    self.bodies
                        .reserve_state(&state)
                        .map_err(RoundError::StatePublication)?;
                }
            }
        }
        Ok(())
    }

    /// Hand the bodies `folded`'s committed state to observe
    /// ([`MemberBodies::observe`]), with the members this activation
    /// admitted and has not started, and those whose outcomes its own
    /// acknowledged commits wrote since the last observation. A process
    /// observes each fold before it hands a settled step to its engine,
    /// which drops the step's run from the next fold.
    pub(crate) fn observe(&mut self, folded: &RunFold) {
        for view in folded.rounds() {
            self.bodies.observe(view, &self.fresh, &self.settled);
        }
        self.settled.clear();
    }

    async fn act_with(
        &mut self,
        folded: &RunFold,
        starting: bool,
        mut now: Option<DurableInstant>,
        attempt: Option<&lash_durable::ActorTx>,
    ) -> Result<Act, RoundError> {
        // What the rows committed publishes before anything reads it: an
        // earlier activation's outcomes on a resume, and outcomes whose
        // acknowledgement was lost.
        self.publish_committed(folded)?;
        self.observe(folded);
        // An outcome the rows already have came back on an unacknowledged
        // commit: it is not written again.
        self.finished.retain(|id, held| {
            if held.result.is_err()
                && folded
                    .round(id.run)
                    .and_then(|view| view.member_started_at(id.ordinal))
                    .is_some_and(|member| member.final_payload().is_some())
            {
                return false;
            }
            folded
                .round(id.run)
                .is_some_and(|view| unsettled(view, id, parks(&held.result)))
        });
        // With no outcome held, no batch is open: a window left from a
        // batch whose acknowledgement was lost would wake the owner at
        // once, again and again.
        if self.finished.is_empty() {
            self.batch_opened = None;
        }
        if let Some(act) = self.commit_cancels(folded, attempt).await? {
            return Ok(act);
        }
        if let Some(act) = self.commit_finals(folded, attempt).await? {
            return Ok(act);
        }
        // A lost acknowledgement is resolved by this same fold on recovery.
        // Never signal from an unacknowledged or merely requested decision.
        let mut abandoned = Vec::new();
        for view in folded.rounds() {
            for member in view.members() {
                if let Some(cancellation) = member.cancellation() {
                    if let Some(token) = self.tokens.get(member.call()) {
                        token.cancel();
                    }
                    let id = view.id_of(member);
                    if let Some(held) = self.finished.get_mut(&id) {
                        held.final_at_outcome = false;
                        let result = &mut held.result;
                        if let Ok(outcome) = result {
                            abandoned.extend(outcome.candidate.take());
                        }
                        discard_answer(result);
                        if let Ok(outcome) = result {
                            outcome.output = cancelled_with(cancellation.clone());
                        }
                    }
                }
            }
        }
        for candidate in abandoned {
            candidate.0.abandoned().await;
        }
        // Protected work drains even when the owner stopped starting new bodies.
        for view in folded.rounds() {
            for member in view.members() {
                let id = view.id_of(member);
                if member.final_payload().is_some()
                    && member.outcome().is_none()
                    && !self.in_flight.contains(&id)
                    && !self.finished.contains_key(&id)
                {
                    self.fresh.remove(&id);
                    self.spawn(view.execution(member), member.final_payload().cloned());
                }
            }
        }
        let mut settlements = Vec::new();
        let mut due_starts = Vec::new();
        let mut parked: Vec<Parked> = Vec::new();
        let mut next_due: Option<DurableInstant> = None;
        if starting {
            for view in folded.rounds() {
                for member in view.members() {
                    let id = view.id_of(member);
                    let cancelled = member.cancellation().is_some();
                    let cancel_output = || {
                        member
                            .cancellation()
                            .cloned()
                            .map_or_else(cancelled_outcome, cancelled_with)
                    };
                    if self.in_flight.contains(&id) || self.finished.contains_key(&id) {
                        continue;
                    }
                    // A member held on a capacity verdict runs again on a
                    // later claim; a cancel settles it now.
                    if self.capacity.contains_key(&id) {
                        if !cancelled {
                            continue;
                        }
                        self.capacity.remove(&id);
                    }
                    let execution = view.execution(member);
                    if self.fresh.remove(&id) && !cancelled {
                        self.spawn(execution, None);
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
                                cancel_output()
                            } else {
                                SettledOutput::Interrupted
                            },
                        )),
                        Recovery::RerunAtOrdinal(_) if cancelled => {
                            settlements.push((execution, cancel_output()));
                        }
                        Recovery::RerunAtOrdinal(_) => self.spawn(execution, None),
                        Recovery::Vetoed(_) if cancelled => {
                            settlements.push((execution, cancel_output()))
                        }
                        Recovery::Vetoed(outcome) => settlements.push((execution, outcome)),
                        Recovery::RetryDue { .. } if cancelled => {
                            settlements.push((execution, cancel_output()));
                        }
                        Recovery::RetryDue { at, .. } => {
                            let observed = match now {
                                Some(now) => now,
                                None => {
                                    let observed = self.cx.durable_now().await?;
                                    now = Some(observed);
                                    observed
                                }
                            };
                            if at <= observed {
                                due_starts.push(execution);
                            } else {
                                next_due = Some(next_due.map_or(at, |due| due.min(at)));
                            }
                        }
                        Recovery::Waiting(source) if cancelled => {
                            self.bodies
                                .discharge(&execution, &source, true)
                                .await
                                .map_err(|error| RoundError::Discharge(Box::new(error)))?;
                            settlements.push((execution, cancel_output()));
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
                let acknowledged = self.cx.acknowledged_commits();
                let racing = self.check_terminals(&parked).await?;
                let winner = waits::poll(&self.cx, &racing).await?;
                if attempt.is_some() && self.cx.acknowledged_commits() != acknowledged {
                    // Polling a due wait committed its winner. The process
                    // pass's transaction and fold precede that write: reload
                    // them before classifying or settling the member.
                    return Ok(Act::Refused);
                }
                if let Some(won) = winner {
                    self.parked_ended(won, &parked).await?;
                    parked.clear();
                }
            }
        }

        if let Some(act) = self
            .commit_one(folded, settlements, due_starts, attempt)
            .await?
        {
            return Ok(act);
        }
        Ok(Act::Idle(Idle {
            parked,
            retry_due: next_due,
            busy: !self.running.is_empty() || !self.finished.is_empty(),
            capacity: self
                .capacity
                .values()
                .max_by_key(|verdict| verdict.retry_after)
                .cloned(),
        }))
    }

    /// Commit at most one transaction: a due batch of finished outcomes,
    /// else the settlements, else the starts of due retries.
    async fn commit_one(
        &mut self,
        folded: &RunFold,
        settlements: Vec<(AdmittedExecution, SettledOutput)>,
        due_starts: Vec<AdmittedExecution>,
        attempt: Option<&lash_durable::ActorTx>,
    ) -> Result<Option<Act>, RoundError> {
        let group = self.cx.backend().config().settings().group_commit;
        let batch_due = !self.finished.is_empty()
            && (self.finished.len() >= group.max_members
                || self.running.is_empty()
                || self
                    .batch_opened
                    .is_some_and(|opened| self.cx.clock().now() >= opened + group.window));
        if batch_due {
            let mut tx = match attempt {
                Some(tx) => tx.clone(),
                None => self.cx.begin().await?,
            };
            let now = tx.opened_at();
            self.read_host_cancels(&tx, folded);
            let fenced = self.fence.as_ref().is_some_and(|fence| fence(&tx));
            if fenced
                && folded.rounds().any(|view| {
                    view.members().iter().any(|member| {
                        member.outcome().is_none()
                            && member.cancellation().is_none()
                            && member.final_payload().is_none()
                    })
                })
            {
                self.request_cancels_before(
                    folded,
                    RunSeq(u64::MAX),
                    crate::ToolCancellation::runtime("the owning turn stopped")
                        .with_origin(crate::CancelOrigin::TurnStopped),
                );
                return Ok(Some(Act::Refused));
            }
            // One transaction publishes at most one candidate: a second
            // paged completion waits for the next batch.
            let deferred: BTreeSet<AdmittedId> = self
                .finished
                .iter()
                .filter(
                    |(_, held)| matches!(&held.result, Ok(outcome) if outcome.candidate.is_some()),
                )
                .skip(1)
                .map(|(id, _)| id.clone())
                .collect();
            if self
                .finished
                .keys()
                .filter(|id| !deferred.contains(*id))
                .any(|id| self.cancel_precedes(folded, id))
            {
                return Ok(Some(Act::Refused));
            }
            let mut retried = false;
            let mut outcomes = BTreeSet::new();
            let mut unanswered = Vec::new();
            let mut publication = None;
            let mut unused = Vec::new();
            let mut staged = Vec::new();
            for (id, held) in self
                .finished
                .iter()
                .filter(|(id, _)| !deferred.contains(*id))
            {
                let Some(execution) = folded.admitted(id) else {
                    continue;
                };
                let dropped = held.result.is_err();
                // The ordinal the outcome's record takes.
                let place = (
                    execution.id().owner.clone(),
                    execution.id().run,
                    execution.cursor().peek(),
                );
                let member = folded
                    .round(id.run)
                    .and_then(|view| view.member_started_at(id.ordinal));
                let result = member
                    .and_then(super::RoundMember::cancellation)
                    .map_or_else(
                        || held.result.clone(),
                        |cancellation| Ok(cancelled_with(cancellation.clone()).into()),
                    );
                let final_at_outcome = held.final_at_outcome
                    && member.is_some_and(|member| {
                        member.cancellation().is_none() && member.final_payload().is_none()
                    });
                let (recorded, candidate, record) =
                    self.record_finished(&mut tx, &execution, result, now, final_at_outcome)?;
                staged.push(record);
                match recorded {
                    Recorded::Retry => retried = true,
                    Recorded::Outcome { output, paged } => {
                        outcomes.insert(id.clone());
                        if dropped {
                            unanswered.push((execution, *output));
                        }
                        if paged {
                            publication = candidate.map(|candidate| (candidate, place));
                            continue;
                        }
                    }
                }
                unused.extend(candidate);
            }
            for candidate in unused {
                candidate.0.abandoned().await;
            }
            let label = if retried && outcomes.is_empty() {
                CommitLabel::ROUND_RETRY
            } else {
                self.outcome_label
            };
            // Only a final batch of plain answers can ride the next output.
            // Parks, retries, state publication and callbacks on unanswered
            // members still consume a recorded boundary here.
            let final_plain = self.prepare_final
                && !fenced
                && !retried
                && unanswered.is_empty()
                && self.running.is_empty()
                && self.finished.values().all(|held| {
                    matches!(&held.result,
                    Ok(MemberOutcome { store_local, terminal: None, timer: None, output, capacity: None, candidate: None })
                        if store_local.is_empty() && !matches!(output, SettledOutput::Waiting(_)))
                })
                && folded.rounds().all(|view| {
                    view.members().iter().all(|member| {
                        member.outcome().is_some() || outcomes.contains(&view.id_of(member))
                    })
                });
            if final_plain {
                self.prepared = Some((tx, outcomes, staged));
                self.finished.clear();
                self.batch_opened = None;
                return Ok(Some(Act::Idle(Idle {
                    parked: Vec::new(),
                    retry_due: None,
                    busy: false,
                    capacity: None,
                })));
            }
            let act = match publication {
                Some((candidate, place)) => {
                    self.commit_paged(tx, label, candidate, place, staged)
                        .await?
                }
                None => self.commit(tx, label, staged).await?,
            };
            if matches!(act, Act::Committed(_)) {
                self.settled.extend(outcomes.iter().cloned());
                // The outcomes are durable: their plugin state publishes now,
                // before the reservations they hold are released.
                for (id, held) in std::mem::take(&mut self.finished) {
                    if deferred.contains(&id) {
                        self.finished.insert(id, held);
                        continue;
                    }
                    if let (true, Ok(result)) = (outcomes.contains(&id), &held.result) {
                        self.publish(id, &staged_state(&result.store_local))?;
                    }
                }
                self.batch_opened = None;
                for (execution, output) in &unanswered {
                    self.bodies.unanswered(execution, output);
                }
            }
            if !self.finished.is_empty() {
                self.batch_opened
                    .get_or_insert_with(|| self.cx.clock().now());
            }
            return Ok(Some(act));
        }
        if !settlements.is_empty() {
            let mut tx = match attempt {
                Some(tx) => tx.clone(),
                None => self.cx.begin().await?,
            };
            self.read_host_cancels(&tx, folded);
            if settlements
                .iter()
                .any(|(execution, _)| self.cancel_precedes(folded, execution.id()))
            {
                return Ok(Some(Act::Refused));
            }
            let mut staged = Vec::with_capacity(settlements.len());
            for (execution, outcome) in &settlements {
                staged.push(settle_staged(
                    &mut tx,
                    execution,
                    outcome.clone(),
                    Vec::new(),
                    None,
                    false,
                )?);
            }
            let act = self.commit(tx, self.settlement_label, staged).await?;
            if matches!(act, Act::Committed(_)) {
                self.settled.extend(
                    settlements
                        .iter()
                        .map(|(execution, _)| execution.id().clone()),
                );
                for (execution, outcome) in &settlements {
                    self.bodies.unanswered(execution, outcome);
                }
            }
            return Ok(Some(act));
        }
        if !due_starts.is_empty() {
            let mut tx = match attempt {
                Some(tx) => tx.clone(),
                None => self.cx.begin().await?,
            };
            self.read_host_cancels(&tx, folded);
            if due_starts
                .iter()
                .any(|execution| self.cancel_precedes(folded, execution.id()))
            {
                return Ok(Some(Act::Refused));
            }
            let started: Vec<AdmittedExecution> = due_starts
                .iter()
                .map(|failed| start_retry(&mut tx, failed))
                .collect();
            let act = self
                .commit(tx, CommitLabel::ROUND_START, Vec::new())
                .await?;
            if matches!(act, Act::Committed(_)) {
                self.admitted(&started);
            }
            return Ok(Some(act));
        }
        Ok(None)
    }

    fn cancel_precedes(&self, folded: &RunFold, id: &AdmittedId) -> bool {
        folded
            .round(id.run)
            .and_then(|view| view.member_started_at(id.ordinal))
            .is_some_and(|member| {
                self.cancel_requests.contains_key(member.call())
                    && member.outcome().is_none()
                    && member.cancellation().is_none()
                    && member.final_payload().is_none()
            })
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
    /// one did; the owner acts again. A finished body brings every other one
    /// already finished with it, up to the batch's member bound, without
    /// waiting for any still running. Races none of the owner's own events.
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
        // The store's clock times a retry's due, and nothing else here.
        let until_due = match idle.retry_due {
            Some(at) => {
                let now = self.cx.durable_now().await?;
                Some(Duration::from_millis(
                    u64::try_from(at.0.saturating_sub(now.0)).unwrap_or(0),
                ))
            }
            None => None,
        };
        let racing: Vec<WaitRef> = idle.parked.iter().map(|(wait, ..)| *wait).collect();
        tokio::select! {
            won = waits::race(&self.cx, &racing), if !racing.is_empty() => {
                self.parked_ended(won?, &idle.parked).await?;
            }
            Some(request) = self.final_receiver.recv() => {
                self.final_requests.insert(request.execution.id().clone(), request);
            }
            Some(joined) = self.running.join_next(), if !self.running.is_empty() => {
                self.collect(joined, group.max_members)?;
            }
            () = self.cx.wait_for_mail(), if self.fence.is_some() => {
                self.refresh_controls = true;
            }
            () = sleep_for(&clock, window), if window.is_some() => {}
            () = sleep_for(&clock, until_due), if until_due.is_some() => {}
            () = self.cx.cancel().cancelled() => return Err(RoundError::Stopped),
        }
        Ok(())
    }

    /// Hold `joined`, the attempt that finished first, and every attempt
    /// already finished beside it, without waiting for one still running:
    /// at most `bound` attempts, and no more outcomes than one batch holds.
    /// The first that stops the owner ends the collection with its error;
    /// those held before it stay held.
    fn collect(
        &mut self,
        joined: Result<Finished, tokio::task::JoinError>,
        bound: usize,
    ) -> Result<(), RoundError> {
        self.ingest(joined)?;
        let mut collected = 1;
        while collected < bound && self.finished.len() < bound {
            let Some(joined) = self.running.try_join_next() else {
                break;
            };
            self.ingest(joined)?;
            collected += 1;
        }
        Ok(())
    }

    /// Hold one finished attempt: its outcome until its batch commits, or
    /// an honoured capacity verdict, which records nothing.
    fn ingest(
        &mut self,
        joined: Result<Finished, tokio::task::JoinError>,
    ) -> Result<(), RoundError> {
        let done = match joined {
            Ok(done) => done,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            // Only the runtime shutting down cancels an attempt the
            // lifecycle still holds.
            Err(_) => return Err(RoundError::Stopped),
        };
        self.in_flight.remove(&done.id);
        match done.held.result {
            Err(Stop::Activation | Stop::Lapsed) => Err(RoundError::Stopped),
            // The durable clock could not be read: no outcome, and the
            // owner retries from its rows.
            Err(Stop::Durable(error)) => Err(error.into()),
            // An honoured capacity verdict: no outcome is held or
            // recorded, and the member waits for a later claim.
            Err(Stop::Capacity(verdict)) => {
                self.capacity.insert(done.id, verdict);
                Ok(())
            }
            result => {
                self.finished.insert(
                    done.id,
                    HeldOutcome {
                        result,
                        final_at_outcome: done.held.final_at_outcome,
                    },
                );
                let clock = self.cx.clock();
                self.batch_opened.get_or_insert_with(|| clock.now());
                Ok(())
            }
        }
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
    async fn parked_ended(&mut self, won: RaceWinner, parked: &[Parked]) -> Result<(), RoundError> {
        let member_of = |wait: &WaitRef| parked.iter().find(|(parked, ..)| parked == wait);
        let mut ends: Vec<(&Parked, SettledOutput)> = Vec::new();
        match won {
            RaceWinner::Resolved { wait, resolution } => {
                if let Some(entry @ (_, execution, source)) = member_of(&wait) {
                    let output = self.bodies.resolved(execution, source, resolution);
                    ends.push((entry, output));
                }
            }
            // A park that outlives its own deadline times out on that
            // deadline, never on its body's limit.
            RaceWinner::TimedOut(wait) => {
                if let Some(entry) = member_of(&wait) {
                    ends.push((
                        entry,
                        SettledOutput::TimedOut {
                            cause: LimitCause::WaitDeadline,
                            evidence: AvailableEvidence::default(),
                        },
                    ));
                }
            }
            RaceWinner::Cancelled => {
                for (_, execution, _) in parked {
                    self.request_cancel(
                        execution.call().clone(),
                        crate::ToolCancellation::runtime("the call's wait scope closed")
                            .with_origin(crate::CancelOrigin::ParentEnded),
                    );
                }
                return Ok(());
            }
        }
        let opened = self.cx.clock().now();
        for ((_, execution, source), output) in ends {
            let output = self.bodies.present(execution, output).await;
            let cancelled = matches!(output, SettledOutput::Cancelled { .. });
            if cancelled {
                self.request_cancel(
                    execution.call().clone(),
                    crate::ToolCancellation::runtime("the call was cancelled"),
                );
                continue;
            }
            self.bodies
                .discharge(execution, source, false)
                .await
                .map_err(|error| RoundError::Discharge(Box::new(error)))?;
            self.finished.insert(
                execution.id().clone(),
                HeldOutcome {
                    result: Ok(output.into()),
                    final_at_outcome: false,
                },
            );
            self.batch_opened.get_or_insert(opened);
        }
        Ok(())
    }

    /// Commit `tx`, which staged `staged`, under `label`: the rows it
    /// appended with those records, or [`Act::Refused`] when the store
    /// refused it or its acknowledgement was lost. Ownership lost is an
    /// error.
    async fn commit(
        &mut self,
        mut tx: lash_durable::ActorTx,
        label: CommitLabel,
        staged: Vec<StagedRecord>,
    ) -> Result<Act, RoundError> {
        self.bodies.stage_commit(&mut tx, self.cx.epoch())?;
        let appended = RunFacts::appended(tx.domain(), tx.graph(), self.cx.epoch());
        match self.cx.commit(tx, label).await {
            Ok(commit) => Ok(Act::Committed(Committed {
                appended,
                revision: commit.revision,
                staged,
            })),
            Err(
                error @ (DurableError::OwnershipLost(_) | DurableError::RevisionConflict { .. }),
            ) => Err(error.into()),
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

    /// Commit `tx`, which records `candidate`'s outcome at `place`, with
    /// the candidate's pages in its one fenced transaction, owned by that
    /// run fact (FIG-6193, P1). The staging settles once the commit is
    /// authoritative. A commit refused before it entered the store is
    /// [`Act::Refused`], as [`commit`](Self::commit)'s: the staging keeps
    /// its pages for the outcome's next commit. A lost fence, a lost
    /// acknowledgement or a refused candidate abandons it and ends the
    /// activation: the pages were never published, or a lost
    /// acknowledgement left them the store's under the fact, and the step's
    /// next owner settles the call as its records say.
    async fn commit_paged(
        &mut self,
        mut tx: lash_durable::ActorTx,
        label: CommitLabel,
        candidate: StepCandidate,
        (owner, run, ordinal): (
            lash_durable::domain::OwnerKey,
            RunSeq,
            lash_durable::domain::Ordinal,
        ),
        staged: Vec<StagedRecord>,
    ) -> Result<Act, RoundError> {
        self.bodies.stage_commit(&mut tx, self.cx.epoch())?;
        let commit = lash_core_store::checkpoint_graph::CommitId::mint();
        let published = match candidate.0.publication(&mut tx, commit).await {
            Ok(published) => published,
            Err(error) => {
                candidate.0.abandoned().await;
                return Err(error.into());
            }
        };
        // The facts as the store holds them once the commit lands: the
        // nodes the transaction stages, with what the publication staged in
        // it, and the root page the outcome names, which only the
        // candidate's manifest supplies.
        let appended = RunFacts::appended(tx.domain(), tx.graph(), self.cx.epoch())
            .with_published(&published.root);
        let publication = lash_durable::domain::pages::CandidatePublication {
            owner: lash_durable::domain::pages::CandidateOwner::RunFactAppend {
                owner,
                run,
                ordinal,
            },
            manifest: published.manifest,
        };
        match self
            .cx
            .commit_candidate_owing(tx, publication, label, None)
            .await
        {
            Ok(receipt) => {
                candidate.0.settled(commit).await;
                Ok(Act::Committed(Committed {
                    appended,
                    revision: receipt.revision,
                    staged,
                }))
            }
            Err(error @ DurableError::RevisionConflict { .. }) => {
                candidate.0.withdrawn(commit).await;
                Err(error.into())
            }
            Err(
                error @ (DurableError::OwnershipLost(_)
                | DurableError::AckLost { .. }
                | DurableError::Graph(_)
                | DurableError::Domain(DomainRefusal::ProcessStartRefused { .. })),
            ) => {
                candidate.0.abandoned().await;
                Err(error.into())
            }
            Err(error) => {
                candidate.0.withdrawn(commit).await;
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

    /// Record one finished attempt on `tx`: its final outcome with its
    /// store-local effect, or a retry when its pinned contract repeats it.
    /// A completion's heap pages are recorded with its outcome, which owns
    /// them once `tx` commits with their publication; the candidate is
    /// handed back with whether its root was recorded, and the record staged.
    fn record_finished(
        &self,
        tx: &mut lash_durable::ActorTx,
        execution: &AdmittedExecution,
        result: Result<MemberOutcome, Stop>,
        now: DurableInstant,
        final_at_outcome: bool,
    ) -> Result<(Recorded, Option<StepCandidate>, StagedRecord), RoundError> {
        let (mut output, store_local, terminal, timer, candidate) = match result {
            Ok(result) => (
                result.output,
                result.store_local,
                result.terminal,
                result.timer,
                result.candidate,
            ),
            Err(Stop::Limit(cause)) => (
                SettledOutput::TimedOut {
                    cause,
                    evidence: AvailableEvidence::default(),
                },
                Vec::new(),
                None,
                None,
                None,
            ),
            Err(Stop::Cancelled | Stop::Activation | Stop::Lapsed) => {
                (cancelled_outcome(), Vec::new(), None, None, None)
            }
            Err(Stop::Durable(error)) => return Err(error.into()),
            // Never held as finished: the lifecycle keeps an honoured
            // verdict's member aside, without an outcome.
            Err(Stop::Capacity(_)) => (SettledOutput::Interrupted, Vec::new(), None, None, None),
        };
        // A park whose resolver awaits a process waits on its terminal too:
        // the wait is pinned with the park, under the park's own deadline.
        if let (SettledOutput::Waiting(source), Some(process)) = (&mut output, terminal) {
            let (wait, _) = waits::pin(
                tx,
                lash_execution_contracts::waits::WaitSpec {
                    scope: waits::execution_wait_scope(&self.cx, &execution.id().owner)?,
                    purpose: waits::WaitPurpose::ProcessTerminal {
                        process,
                        deadline: execution
                            .draft()
                            .park()
                            .and_then(ParkDeadline::deadline)
                            .map(lash_execution_contracts::waits::WaitDeadline::at),
                    },
                },
            );
            source.await_terminal(wait.id().to_hex());
        }
        if let (SettledOutput::Waiting(source), Some(deadline)) = (&mut output, timer) {
            let (wait, _) = waits::pin(
                tx,
                lash_execution_contracts::waits::WaitSpec {
                    scope: waits::execution_wait_scope(&self.cx, &execution.id().owner)?,
                    purpose: waits::WaitPurpose::Timer { deadline },
                },
            );
            source.await_timer(lash_core_store::tool_run::CompletionTimer {
                wait: wait.id().to_hex(),
                deadline_ms: deadline.0,
            });
        }
        if let Some(due) = self.retry_due(execution, &output, now) {
            let record = settle_retry(tx, execution, output, due)?;
            return Ok((Recorded::Retry, candidate, record));
        }
        let pages = candidate
            .as_ref()
            .filter(|_| matches!(output, SettledOutput::Completed(_)))
            .map(StepCandidate::root);
        let final_at_outcome = final_at_outcome
            && matches!(
                output,
                SettledOutput::Completed(_) | SettledOutput::Failed(_)
            );
        let record = settle_staged(
            tx,
            execution,
            output.clone(),
            store_local,
            pages,
            final_at_outcome,
        )?;
        Ok((
            Recorded::Outcome {
                output: Box::new(output),
                paged: pages.is_some(),
            },
            candidate,
            record,
        ))
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
        if self
            .tokens
            .get(execution.call())
            .is_some_and(CancellationToken::is_cancelled)
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
        let retry = pinned.retry()?;
        let delay = retry.delay_ms(execution.attempt() - 1, suggested);
        let due = now
            .0
            .saturating_add(i64::try_from(delay).unwrap_or(i64::MAX));
        let expires_at = i64::try_from(execution.limit().expires_at).unwrap_or(i64::MAX);
        (due < expires_at).then_some(DurableInstant(due))
    }
}

enum Recorded {
    Retry,
    /// The call's final outcome, as it was recorded, and whether it names
    /// its candidate's pages.
    Outcome {
        output: Box<SettledOutput>,
        paged: bool,
    },
}

/// An owner's stop, as the open read of one of its transactions says: see
/// [`Lifecycle::fenced_by`].
pub type Fence = Arc<dyn Fn(&lash_durable::ActorTx) -> bool + Send + Sync>;

fn cancelled_outcome() -> SettledOutput {
    cancelled_with(crate::ToolCancellation::runtime("the call was cancelled"))
}

fn cancelled_with(cancellation: crate::ToolCancellation) -> SettledOutput {
    SettledOutput::Cancelled {
        evidence: AvailableEvidence::default(),
        cancellation,
    }
}

/// `result`, a finished attempt's, as its owner's accepted stop leaves it:
/// the member's own cancellation stands, and any other answer is
/// `Cancelled`, with the plugin state it staged discarded and none of its
/// store-local effects written.
fn discard_answer(result: &mut Result<MemberOutcome, Stop>) {
    let Ok(outcome) = result else {
        return;
    };
    if matches!(outcome.output, SettledOutput::Cancelled { .. }) {
        return;
    }
    for effect in std::mem::take(&mut outcome.store_local) {
        if let StoreLocalEffect::PluginState(staged) = effect {
            staged.discard();
        }
    }
    *outcome = cancelled_outcome().into();
}

/// Whether `id` is still `view`'s open attempt of its member, for a
/// finished result that does (`park`) or does not park it: a park records
/// only on a started attempt, an end on a started or a parked one.
fn unsettled(view: &RoundView, id: &AdmittedId, park: bool) -> bool {
    view.members().iter().any(|member| {
        view.id_of(member) == *id
            && match member.state() {
                super::MemberState::Started { start, .. } => *start == id.ordinal,
                super::MemberState::Waiting { start, .. } => !park && *start == id.ordinal,
                super::MemberState::RetryDue { .. } | super::MemberState::Final { .. } => false,
            }
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
fn parks(result: &Result<MemberOutcome, Stop>) -> bool {
    matches!(
        result,
        Ok(MemberOutcome {
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
    if let Some(timer) = &source.timer {
        refs.push(WaitRef::new(
            WaitId::parse_hex(&timer.wait).ok_or_else(undecodable)?,
            WaitKind::Timer,
        ));
    }
    Ok(refs)
}

async fn sleep_for(clock: &Arc<dyn crate::Clock>, duration: Option<Duration>) {
    clock.sleep(duration.unwrap_or_default()).await;
}
