//! Admitted executions and tool rounds (ADR 0132 §5; S4 of I0, FIG-5194).
//!
//! The admitted-execution primitive is generic over its owner: a turn's tool
//! round, a process's steps (L6) and a VM's issued operations (L7) all run
//! through [`admit`], a bounded body run and [`settle`], and recover through
//! [`fold`]. Nobody forks it. A round's members and a process's steps share
//! one lifecycle on top of it ([`lifecycle`]): admission, retry records,
//! the parked-wait race and settlement.
//!
//! # Contracts
//!
//! - No body starts before [`admit`]'s transaction commits: its `x_start` row
//!   is the authorization.
//! - No body starts on a node past its lease: an admission acknowledged after
//!   the node's self-stop deadline may already be another owner's to
//!   recover, so its body is never entered there (ADR 0132 §3).
//! - The fold calls no producer: resume folds rows into state.
//! - A `Once` execution started without an outcome folds to `Interrupted`
//!   and its body is never entered again; a `Repeatable` one reruns at its
//!   same ordinal, uncounted.
//! - A current `Once` declaration vetoes a stored `Repeatable` repeat, and a
//!   stored `Once` is never upgraded.
//! - `(owner, run, ordinal)` is the second fence: one record per ordinal.
//!
//! V0 (FIG-5170) lands the primitive and its laws; L4 (FIG-5174) builds
//! rounds, retries, group commit, deadlines and store-local effects on it.
//!
//! # Record layout
//!
//! One admission is one run of its owner. Its records take the run's
//! ordinals in sequence, from 0, without a gap: the owner is the only writer
//! under its epoch, and the store refuses an append that does not follow a
//! record ([`DomainRefusal::RunOrdinalGap`]), so a gap is never committed and
//! the fold refuses one it finds.
//!
//! | Ordinal | Kind | Body |
//! |---|---|---|
//! | 0 | `admit` | every member's call, tool, request, policy, limit and wait |
//! | 1 + i | `x_start` | member i's first attempt, committed with the admission |
//! | next | `x_outcome` | an attempt's final outcome and the material it names |
//! | next | `x_wait` | an attempt that parked: the waits it races and its pending completion |
//! | next | `retry` | a `Repeatable` attempt's failure and the due time of the next |
//! | next | `x_start` | the next attempt of a retried member, once its retry is due |
//! | next | `present` | the round's presentation, in declared order |
//!
//! "next" is the run's next ordinal when its transaction is built. An
//! [`AdmittedExecution`] carries the cursor its run's appends take ordinals
//! from; [`admit`] and [`fold`] build it. A transaction that is refused, or
//! whose acknowledgement is lost, leaves the cursor ahead of the rows: the
//! owner folds again before it writes that run again.
//!
//! A call has at most one `x_outcome`: a `retry` settles a failed attempt
//! without ending the call, and an `x_wait` parks it on the completion wait
//! its admission pinned until its `x_outcome`, at the same start, settles
//! it. An outcome's material payload rides in its row as one
//! [`SettledOutput`]: the record is the payload's journal-local home, and a
//! record whose payload is missing or does not hash to its outcome's digest
//! does not decode, so the fold refuses it. A vetoed retry's final record
//! carries the retry's payload again: no record leans on another's bytes.
//!
//! [`DomainRefusal::RunOrdinalGap`]: lash_durable::domain::DomainRefusal::RunOrdinalGap

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lash_core_store::tool_run::{CompletionSource, MaterialRef};
use lash_durable::domain::{AdmittedId, Ordinal, RunRecordKind};
use lash_durable::{ActorTx, DurableError};
use lash_sansio::{ExecutionBudgets, ExecutionLimit, ExecutionPolicy, LimitCause};
use tokio_util::sync::CancellationToken;

use super::ActorContext;
use super::waits::{WaitDeadline, WaitId, WaitKind, WaitRef};
use crate::{ToolCallId, ToolId};

pub use lash_durable::domain::{OwnerKey, ProcessStartRows, RunSeq};

mod context;
mod fold;
#[cfg(test)]
mod fold_tests;
pub mod lifecycle;
mod output;
mod records;
mod rounds;
mod runner;
mod store_local;
mod tools;

pub use fold::{MemberState, RoundMember, RoundView, fold};
pub use lifecycle::{Discharge, MemberBodies, MemberBody, MemberResult, Presented, member_body};
pub use output::{Material, NamesMaterial, SettledOutput, SettledOutputRefusal};
pub use records::RUN_RECORD_FORMAT_VERSION;
pub use rounds::{admit_round, present, presentation, settle_retry, start_retry};
pub use runner::{RoundEnd, RoundError, RoundRunner, SettledRound};
pub use tools::{
    CompletedCall, MemberPin, RoundCalls, RoundCallsRefusal, RoundTools, call_draft,
    completed_material, decode_completed, request_material, require_admitted, settle_cancelled,
};

use records::{OutcomeBody, append, encode, first_start};

/// The tool completion wait a round pinned for a member that may defer,
/// with its admission: its id is also its completion key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PinnedWait {
    /// The wait.
    pub id: WaitId,
}

impl PinnedWait {
    /// The wait, as its owner races it.
    #[must_use]
    pub fn wait(&self) -> WaitRef {
        WaitRef::new(self.id, WaitKind::ToolCompletion)
    }
}

/// One execution to admit: a call, its tool, its request material, the
/// policy and limit pinned now, and the wait deadline of a call that may
/// park (Pending), with the wait its round pins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionDraft {
    call: ToolCallId,
    tool: ToolId,
    request: MaterialRef,
    policy: ExecutionPolicy,
    limit: ExecutionLimit,
    wait: Option<WaitDeadline>,
    pinned: Option<PinnedWait>,
}

impl ExecutionDraft {
    /// A draft of `call` of `tool` over `request`, its policy and limit
    /// pinned now.
    #[must_use]
    pub fn new(
        call: ToolCallId,
        tool: ToolId,
        request: MaterialRef,
        policy: ExecutionPolicy,
        limit: ExecutionLimit,
        wait: Option<WaitDeadline>,
    ) -> Self {
        Self {
            call,
            tool,
            request,
            policy,
            limit,
            wait,
            pinned: None,
        }
    }

    /// This draft with `pinned`, the completion wait its round pinned.
    #[must_use]
    pub(crate) fn with_pinned_wait(mut self, pinned: Option<PinnedWait>) -> Self {
        self.pinned = pinned;
        self
    }

    /// The call.
    #[must_use]
    pub fn call(&self) -> &ToolCallId {
        &self.call
    }

    /// The tool.
    #[must_use]
    pub fn tool(&self) -> &ToolId {
        &self.tool
    }

    /// The request material.
    #[must_use]
    pub fn request(&self) -> &MaterialRef {
        &self.request
    }

    /// The pinned policy.
    #[must_use]
    pub fn policy(&self) -> ExecutionPolicy {
        self.policy
    }

    /// The pinned limit.
    #[must_use]
    pub fn limit(&self) -> ExecutionLimit {
        self.limit
    }

    /// A Pending call's wait deadline.
    #[must_use]
    pub fn wait(&self) -> Option<WaitDeadline> {
        self.wait
    }

    /// The completion wait its round pinned, for a call that may park.
    #[must_use]
    pub fn pinned_wait(&self) -> Option<PinnedWait> {
        self.pinned
    }
}

/// Where a run's next record goes: the run's next ordinal, shared by every
/// execution of the run an [`admit`] or a [`fold`] handed out.
#[derive(Debug)]
pub(crate) struct RunCursor {
    next: AtomicU64,
}

impl PartialEq for RunCursor {
    fn eq(&self, other: &Self) -> bool {
        self.peek() == other.peek()
    }
}

impl Eq for RunCursor {}

impl RunCursor {
    pub(crate) fn at(next: Ordinal) -> Arc<Self> {
        Arc::new(Self {
            next: AtomicU64::new(next.0),
        })
    }

    /// Take the next ordinal for a record about to be written.
    pub(crate) fn take(&self) -> Ordinal {
        Ordinal(self.next.fetch_add(1, Ordering::SeqCst))
    }

    /// The next ordinal, untaken.
    pub(crate) fn peek(&self) -> Ordinal {
        Ordinal(self.next.load(Ordering::SeqCst))
    }
}

/// One execution whose admission and `x_start` are recorded on a
/// transaction. Its body may run once that transaction commits.
#[derive(Clone, Debug)]
pub struct AdmittedExecution {
    id: AdmittedId,
    draft: ExecutionDraft,
    member: u64,
    attempt: u32,
    cursor: Arc<RunCursor>,
}

impl PartialEq for AdmittedExecution {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.draft == other.draft
            && self.member == other.member
            && self.attempt == other.attempt
    }
}

impl Eq for AdmittedExecution {}

impl AdmittedExecution {
    /// What [`admit`], a retry's start or a [`fold`] records: `draft`,
    /// member `member` of its admission, admitted as `id` for its
    /// `attempt`, its run's records taking ordinals from `cursor`.
    pub(crate) fn admitted(
        id: AdmittedId,
        draft: ExecutionDraft,
        member: u64,
        attempt: u32,
        cursor: Arc<RunCursor>,
    ) -> Self {
        Self {
            id,
            draft,
            member,
            attempt,
            cursor,
        }
    }

    /// Its identity: owner, run and ordinal.
    #[must_use]
    pub fn id(&self) -> &AdmittedId {
        &self.id
    }

    /// Its ordinal.
    #[must_use]
    pub fn ordinal(&self) -> Ordinal {
        self.id.ordinal
    }

    /// The call.
    #[must_use]
    pub fn call(&self) -> &ToolCallId {
        self.draft.call()
    }

    /// The draft its admission pinned.
    #[must_use]
    pub fn draft(&self) -> &ExecutionDraft {
        &self.draft
    }

    /// The policy pinned at admission.
    #[must_use]
    pub fn policy(&self) -> ExecutionPolicy {
        self.draft.policy()
    }

    /// The limit pinned at admission.
    #[must_use]
    pub fn limit(&self) -> ExecutionLimit {
        self.draft.limit()
    }

    /// Its member index in its admission.
    #[must_use]
    pub fn member(&self) -> u64 {
        self.member
    }

    /// Which attempt of its call it is, from 1. A crash rerun keeps it; only
    /// a retry advances it.
    #[must_use]
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    pub(crate) fn cursor(&self) -> &Arc<RunCursor> {
        &self.cursor
    }
}

/// An admitted execution's body: the catalog tool's work, given the cancel
/// token it must observe. Its lifecycle bounds it by the execution's limit
/// and the stop grace.
pub type ToolBody = Box<
    dyn FnOnce(CancellationToken) -> Pin<Box<dyn Future<Output = SettledOutput> + Send>> + Send,
>;

/// Why an admission was refused; nothing was recorded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionRefusal {
    /// A round admits at least one execution.
    #[error("an admission names no execution")]
    Empty,
    /// One call appears twice.
    #[error("call {0} is admitted twice")]
    DuplicateCall(ToolCallId),
    /// A member's completion wait could not be pinned.
    #[error(transparent)]
    Wait(#[from] super::waits::PinRefusal),
}

/// Why a settle was refused; nothing was recorded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SettleRefusal {
    /// The store-local effect does not belong to this execution's outcome:
    /// an effect commits only with the completion or the park that staged
    /// it.
    #[error("call {0}'s store-local effect does not match its outcome")]
    ForeignEffect(ToolCallId),
    /// The fold names no started execution under this identity.
    #[error("no started execution is admitted as run {run:?} ordinal {ordinal:?}")]
    NotStarted {
        /// The run.
        run: RunSeq,
        /// The ordinal.
        ordinal: Ordinal,
    },
    /// A retry was asked of an attempt its pinned policy does not repeat:
    /// a `Once` call, an outcome that may not repeat, or the last attempt.
    #[error("call {0}'s attempt may not be retried")]
    NotRetryable(ToolCallId),
}

/// Why rows did not fold; the owner's state is unreadable.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FoldRefusal {
    /// The ordinals of a run have a gap.
    #[error("run {run:?} has no record at ordinal {missing:?}")]
    OrdinalGap {
        /// The run.
        run: RunSeq,
        /// The first missing ordinal.
        missing: Ordinal,
    },
    /// A call has a second final or cancel record.
    #[error("call {0} has a second final record")]
    SecondFinal(ToolCallId),
    /// A record is out of its call's order: an outcome or retry of an
    /// attempt that is not the call's open one, or an attempt no retry
    /// issued.
    #[error("record {ordinal:?} of run {run:?} is out of its call's order")]
    OutOfOrder {
        /// The run.
        run: RunSeq,
        /// The ordinal.
        ordinal: Ordinal,
    },
    /// A record's body does not decode.
    #[error("record {ordinal:?} of run {run:?} does not decode: {reason}")]
    Undecodable {
        /// The run.
        run: RunSeq,
        /// The ordinal.
        ordinal: Ordinal,
        /// Why.
        reason: String,
    },
}

/// The policies currently declared, by tool: what a fold vetoes a stored
/// repeat against.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyView {
    current: std::collections::BTreeMap<ToolId, ExecutionPolicy>,
}

impl PolicyView {
    /// The view over `current` declarations.
    #[must_use]
    pub fn new(current: impl IntoIterator<Item = (ToolId, ExecutionPolicy)>) -> Self {
        Self {
            current: current.into_iter().collect(),
        }
    }

    /// The policy `tool` currently declares.
    #[must_use]
    pub fn policy(&self, tool: &ToolId) -> Option<ExecutionPolicy> {
        self.current.get(tool).copied()
    }

    /// Whether a stored `pinned` policy of `tool` may run again: it is
    /// `Repeatable`, and the current declaration does not veto it with
    /// `Once`. A stored `Once` is never upgraded.
    #[must_use]
    pub fn permits_repeat(&self, tool: &ToolId, pinned: ExecutionPolicy) -> bool {
        matches!(pinned, ExecutionPolicy::Repeatable { .. })
            && self.policy(tool) != Some(ExecutionPolicy::Once)
    }
}

/// What an admitted execution's rows say to do on resume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// It has its outcome.
    Settled(SettledOutput),
    /// A `Once` started without an outcome: record `Interrupted` and never
    /// enter its body again.
    Interrupt,
    /// A `Repeatable` started without an outcome: run again at this same
    /// ordinal, uncounted.
    RerunAtOrdinal(Ordinal),
    /// A retry is due at `at`; its next attempt is `attempt`, which takes
    /// the run's next ordinal when it starts.
    RetryDue {
        /// When.
        at: lash_durable::DurableInstant,
        /// The attempt it starts, from 1.
        attempt: u32,
    },
    /// A retry was recorded, but the current declaration vetoes the repeat:
    /// the failed attempt's output, payload and all, becomes the call's
    /// final one.
    Vetoed(SettledOutput),
    /// The attempt parked: race its waits, never enter its body again.
    Waiting(Material<CompletionSource>),
    /// Admitted but never started.
    NotStarted,
}

/// An owner's run records folded: what each admitted execution recovers to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunFold {
    recoveries: Vec<(AdmittedId, Recovery)>,
    rounds: std::collections::BTreeMap<RunSeq, RoundView>,
}

impl RunFold {
    /// The fold of `recoveries`, in admission order.
    #[must_use]
    pub fn new(recoveries: Vec<(AdmittedId, Recovery)>) -> Self {
        Self {
            recoveries,
            rounds: std::collections::BTreeMap::new(),
        }
    }

    /// Each admitted execution and what it recovers to, in admission order.
    #[must_use]
    pub fn recoveries(&self) -> &[(AdmittedId, Recovery)] {
        &self.recoveries
    }

    /// What `id` recovers to, if it was admitted.
    #[must_use]
    pub fn recovery(&self, id: &AdmittedId) -> Option<&Recovery> {
        self.recoveries
            .iter()
            .find(|(admitted, _)| admitted == id)
            .map(|(_, recovery)| recovery)
    }

    /// The call `id` executes, if it was started.
    #[must_use]
    pub fn call(&self, id: &AdmittedId) -> Option<&ToolCallId> {
        self.admitted_member(id).map(|member| member.draft.call())
    }

    /// The execution `id` names, rebuilt with its pinned draft and its run's
    /// cursor, if it was started: what a rerun or a recovery settles
    /// through.
    #[must_use]
    pub fn admitted(&self, id: &AdmittedId) -> Option<AdmittedExecution> {
        let round = self.rounds.get(&id.run)?;
        round.admitted(id)
    }

    fn admitted_member(&self, id: &AdmittedId) -> Option<&RoundMember> {
        self.rounds.get(&id.run)?.member_started_at(id.ordinal)
    }

    /// One admission's state: its members in declared order.
    #[must_use]
    pub fn round(&self, run: RunSeq) -> Option<&RoundView> {
        self.rounds.get(&run)
    }

    /// The round admitted as `run`, rebuilt from its records: what an owner
    /// that did not admit it presents through.
    #[must_use]
    pub fn admitted_round(&self, run: RunSeq) -> Option<AdmittedRound> {
        let view = self.rounds.get(&run)?;
        Some(AdmittedRound::admitted(
            run,
            view.members()
                .iter()
                .map(|member| view.execution(member))
                .collect(),
        ))
    }

    /// Every admission's state, by run.
    pub fn rounds(&self) -> impl Iterator<Item = &RoundView> {
        self.rounds.values()
    }
}

/// A store write whose effect is a lash store row, committed in the same
/// transaction as its tool's outcome, under its owner's epoch fence: exactly
/// once (ADR 0132 §5). A call's realization stages each; nothing commits one
/// before its outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreLocalEffect {
    /// Start a process: its registry row, its observers and its actor,
    /// ready.
    ProcessStart(ProcessStartRows),
    /// Send a signal: its event on the target, its mail and the target's
    /// wake.
    SignalSend(SignalSendRows),
    /// Create a trigger subscription.
    TriggerCreate(StoreLocalRows),
    /// Delete a trigger subscription.
    TriggerDelete(StoreLocalRows),
    /// Spawn a child session.
    ChildSessionSpawn(StoreLocalRows),
}

impl StoreLocalEffect {
    /// The effect that sends `signal`.
    ///
    /// # Errors
    ///
    /// A signal that does not encode.
    pub fn signal(signal: &crate::ProcessSignal) -> Result<Self, crate::PluginError> {
        Ok(Self::SignalSend(SignalSendRows {
            process: signal.identity.process_id().clone(),
            signal_json: serde_json::to_string(signal).map_err(|error| {
                crate::PluginError::Session(format!("failed to encode a signal: {error}"))
            })?,
        }))
    }
}

/// The rows of one signal sent as a store-local effect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignalSendRows {
    /// The target.
    pub process: crate::ProcessId,
    /// The signal, encoded.
    pub signal_json: String,
}

/// The rows of one store-local effect no producer stages yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreLocalRows {
    /// The rows.
    pub rows_json: String,
}

/// A tool round to admit inside the `model.done` transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundDraft {
    /// The owner.
    pub owner: OwnerKey,
    /// The round's run.
    pub run: RunSeq,
    /// Its members, in declared order.
    pub members: Vec<ExecutionDraft>,
}

/// A round whose admission and `x_start` rows are recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmittedRound {
    run: RunSeq,
    members: Vec<AdmittedExecution>,
}

impl AdmittedRound {
    /// The round of `members` in `run`.
    pub(crate) fn admitted(run: RunSeq, members: Vec<AdmittedExecution>) -> Self {
        Self { run, members }
    }

    /// Its run.
    #[must_use]
    pub fn run(&self) -> RunSeq {
        self.run
    }

    /// Its members, in declared order.
    #[must_use]
    pub fn members(&self) -> &[AdmittedExecution] {
        &self.members
    }

    /// Its owner.
    #[must_use]
    pub fn owner(&self) -> Option<&OwnerKey> {
        self.members.first().map(|member| &member.id.owner)
    }
}

/// What a round presents to the next model call, in declared order: a pure
/// function of its committed records.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Presentation {
    calls: Vec<ToolCallId>,
    outcomes: Vec<Option<SettledOutput>>,
}

impl Presentation {
    /// The presentation of `calls`, in declared order.
    #[must_use]
    pub fn new(calls: Vec<ToolCallId>) -> Self {
        let outcomes = vec![None; calls.len()];
        Self { calls, outcomes }
    }

    pub(crate) fn of(entries: Vec<(ToolCallId, Option<SettledOutput>)>) -> Self {
        let (calls, outcomes) = entries.into_iter().unzip();
        Self { calls, outcomes }
    }

    /// The presented calls, in declared order.
    #[must_use]
    pub fn calls(&self) -> &[ToolCallId] {
        &self.calls
    }

    /// Each presented call with its committed outcome, in declared order;
    /// `None` for a call with none yet.
    pub fn entries(&self) -> impl Iterator<Item = (&ToolCallId, Option<&SettledOutput>)> {
        self.calls
            .iter()
            .zip(self.outcomes.iter().map(Option::as_ref))
    }
}

/// Record the admission and an `x_start` of every draft on `tx`, as
/// `owner`'s run `run`, in one transaction. No body starts before `tx`
/// commits.
///
/// # Errors
///
/// [`AdmissionRefusal`]; nothing is recorded.
pub fn admit(
    tx: &mut ActorTx,
    owner: &OwnerKey,
    run: RunSeq,
    drafts: Vec<ExecutionDraft>,
) -> Result<Vec<AdmittedExecution>, AdmissionRefusal> {
    check_drafts(&drafts)?;
    tx.write(records::admit_record(owner, run, &drafts));
    let cursor = RunCursor::at(first_start(drafts.len() as u64));
    let mut admitted = Vec::with_capacity(drafts.len());
    for (member, draft) in (0_u64..).zip(drafts) {
        let ordinal = first_start(member);
        tx.write(records::start_record(
            owner, run, ordinal, &draft, member, 1,
        ));
        admitted.push(AdmittedExecution::admitted(
            AdmittedId {
                owner: owner.clone(),
                run,
                ordinal,
            },
            draft,
            member,
            1,
            Arc::clone(&cursor),
        ));
    }
    Ok(admitted)
}

/// Refuse an admission of no draft, or of one call twice.
fn check_drafts(drafts: &[ExecutionDraft]) -> Result<(), AdmissionRefusal> {
    if drafts.is_empty() {
        return Err(AdmissionRefusal::Empty);
    }
    let mut seen = std::collections::BTreeSet::new();
    for draft in drafts {
        if !seen.insert(draft.call().clone()) {
            return Err(AdmissionRefusal::DuplicateCall(draft.call().clone()));
        }
    }
    Ok(())
}

/// The ordinal of member `member`'s first `x_start` in its admission's run:
/// the ordinal its [`AdmittedId`] takes.
#[must_use]
pub fn member_ordinal(member: u64) -> Ordinal {
    first_start(member)
}

/// How an admitted body's run stopped short of its own answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// The execution's slice or limit ran out.
    Limit(LimitCause),
    /// Its member was cancelled.
    Cancelled,
    /// The activation stopped: the node is going away, and nothing may be
    /// recorded for it.
    Activation,
    /// The node's lease lapsed before the body started: it never ran, and
    /// nothing may be recorded for it on this owner.
    Lapsed,
    /// The durable clock could not be read; the activation retries without an outcome.
    Durable(DurableError),
}

/// Run `admitted`'s body under its limit, the context's cancel token,
/// `member_cancel` and the stop grace: the body's own answer, or why it
/// stopped without one. Reports the body's entry to the context's probe.
///
/// The body runs for at most one slice of what remains of its limit on the
/// store's clock. When the slice or the limit ends, its member is cancelled
/// or the activation is, the body's token is cancelled and it has the stop
/// grace to answer (none on the activation's stop); after that it is
/// dropped and the stop is its outcome. A limit already expired settles at
/// once, without entering the body, and a node whose lease lapsed never
/// enters it ([`Stop::Lapsed`]).
pub(crate) async fn run_bounded(
    cx: &ActorContext,
    admitted: &AdmittedExecution,
    body: ToolBody,
    member_cancel: &CancellationToken,
) -> Result<SettledOutput, Stop> {
    // The admission may have been acknowledged after the node paused past
    // its self-stop deadline: by then the actor's new owner may have
    // settled it, so its body never starts here.
    if !cx.lease_held() {
        return Err(Stop::Lapsed);
    }
    let clock = Arc::clone(cx.clock());
    let now = u64::try_from(cx.durable_now().await.map_err(Stop::Durable)?.0).unwrap_or(0);
    // Reading the durable clock can yield past the node's self-stop deadline.
    if !cx.lease_held() {
        return Err(Stop::Lapsed);
    }
    let limit = admitted.limit();
    let remaining = limit.remaining(now);
    if remaining.is_zero() {
        return Err(Stop::Limit(LimitCause::ExecutionTotal));
    }
    let (slice, cause) = if limit.max_slice < remaining {
        (limit.max_slice, LimitCause::ExecutionSlice)
    } else {
        (remaining, LimitCause::ExecutionTotal)
    };
    cx.probe().body_entered(admitted.id());
    let token = cx.cancel().child_token();
    let mut running = body(token.clone());
    let stopped = tokio::select! {
        output = &mut running => return Ok(output),
        () = clock.sleep(slice) => Stop::Limit(cause),
        () = member_cancel.cancelled() => Stop::Cancelled,
        () = cx.cancel().cancelled() => Stop::Activation,
    };
    token.cancel();
    if stopped == Stop::Activation {
        return Err(stopped);
    }
    let grace = ExecutionBudgets::default().stop_grace();
    tokio::select! {
        output = running => Ok(output),
        () = clock.sleep(grace) => Err(stopped),
    }
}

/// Record `admitted`'s outcome on `tx`, with the store-local effects that
/// commit with it, at its run's next ordinal. Only a completion or a park
/// carries effects: a park's declared start is what it waits on.
///
/// # Errors
///
/// [`SettleRefusal`]; nothing is recorded.
pub fn settle(
    tx: &mut ActorTx,
    admitted: &AdmittedExecution,
    output: SettledOutput,
    store_local: Vec<StoreLocalEffect>,
) -> Result<(), SettleRefusal> {
    if !store_local.is_empty() {
        if !matches!(
            output,
            SettledOutput::Completed(_) | SettledOutput::Waiting(_)
        ) {
            return Err(SettleRefusal::ForeignEffect(admitted.call().clone()));
        }
        for effect in store_local {
            store_local::write(tx, admitted, effect)?;
        }
    }
    record_outcome(tx, admitted, output);
    Ok(())
}

/// Record `Interrupted` for `id`, a started `Once` the fold found without
/// an outcome (its [`Recovery::Interrupt`]): its body is never entered
/// again, and the outcome commits before anything acts on it.
///
/// # Errors
///
/// [`SettleRefusal::NotStarted`] when `fold` names no started execution as
/// `id`.
pub fn settle_interrupted(
    tx: &mut ActorTx,
    fold: &RunFold,
    id: &AdmittedId,
) -> Result<(), SettleRefusal> {
    let admitted = fold.admitted(id).ok_or(SettleRefusal::NotStarted {
        run: id.run,
        ordinal: id.ordinal,
    })?;
    record_outcome(tx, &admitted, SettledOutput::Interrupted);
    Ok(())
}

fn record_outcome(tx: &mut ActorTx, admitted: &AdmittedExecution, output: SettledOutput) {
    let id = admitted.id();
    // A park is no outcome: the call's one `x_outcome` follows it.
    let kind = if matches!(output, SettledOutput::Waiting(_)) {
        RunRecordKind::XWait
    } else {
        RunRecordKind::XOutcome
    };
    tx.write(append(
        &id.owner,
        id.run,
        admitted.cursor().take(),
        kind,
        Some(admitted.call()),
        encode(&OutcomeBody {
            start: id.ordinal.0,
            output,
        }),
    ));
}
