//! Admitted executions and tool rounds (ADR 0132 §5; S4 of I0, FIG-5194).
//!
//! The admitted-execution primitive is generic over its owner: a turn's tool
//! round, a process's steps (L6) and a VM's issued operations (L7) all run
//! through [`admit`], [`run_body`] and [`settle`], and recover through
//! [`fold`]. Nobody forks it.
//!
//! # Contracts
//!
//! - No body starts before [`admit`]'s transaction commits: its `x_start` row
//!   is the authorization.
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

use std::future::Future;
use std::pin::Pin;

use lash_core_store::tool_run::{AttemptOutcome, MaterialRef};
use lash_durable::ActorTx;
use lash_durable::domain::{AdmittedId, Ordinal, RunRecordRow};
use lash_sansio::{ExecutionLimit, ExecutionPolicy};
use tokio_util::sync::CancellationToken;

use super::ActorContext;
use super::waits::WaitDeadline;
use crate::{ToolCallId, ToolId};

pub use lash_durable::domain::{OwnerKey, ProcessStartRows, RunSeq};

/// One execution to admit: a call, its tool, its request material, the
/// policy and limit pinned now, and the wait deadline of a Pending call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionDraft {
    call: ToolCallId,
    tool: ToolId,
    request: MaterialRef,
    policy: ExecutionPolicy,
    limit: ExecutionLimit,
    wait: Option<WaitDeadline>,
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
        }
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
}

/// One execution whose admission and `x_start` are recorded on a
/// transaction. Its body may run once that transaction commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmittedExecution {
    id: AdmittedId,
    draft: ExecutionDraft,
}

impl AdmittedExecution {
    /// What [`admit`] records: `draft` admitted as `id`.
    #[expect(dead_code, reason = "V0 (FIG-5170): admit and its fold construct it")]
    pub(crate) fn admitted(id: AdmittedId, draft: ExecutionDraft) -> Self {
        Self { id, draft }
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
}

/// An admitted execution's body: the catalog tool's work, given the cancel
/// token it must observe. [`run_body`] bounds it by the execution's limit
/// and the stop grace.
pub type ToolBody = Box<
    dyn FnOnce(CancellationToken) -> Pin<Box<dyn Future<Output = AttemptOutcome> + Send>> + Send,
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
}

/// Why a settle was refused; nothing was recorded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SettleRefusal {
    /// The store-local effect does not belong to this execution's tool.
    #[error("call {0}'s store-local effect does not match its tool")]
    ForeignEffect(ToolCallId),
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
}

/// What an admitted execution's rows say to do on resume.
#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(
    clippy::large_enum_variant,
    reason = "the pinned recovery shape (S3): one per admitted execution, read once on resume"
)]
pub enum Recovery {
    /// It has its outcome.
    Settled(AttemptOutcome),
    /// A `Once` started without an outcome: record `Interrupted` and never
    /// enter its body again.
    Interrupt,
    /// A `Repeatable` started without an outcome: run again at this same
    /// ordinal, uncounted.
    RerunAtOrdinal(Ordinal),
    /// A retry is due at `at`; its next attempt takes ordinal `next`.
    RetryDue {
        /// When.
        at: lash_durable::DurableInstant,
        /// The next attempt's ordinal.
        next: Ordinal,
    },
    /// Admitted but never started.
    NotStarted,
}

/// An owner's run records folded: what each admitted execution recovers to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunFold {
    recoveries: Vec<(AdmittedId, Recovery)>,
}

impl RunFold {
    /// The fold of `recoveries`, in admission order.
    #[must_use]
    pub fn new(recoveries: Vec<(AdmittedId, Recovery)>) -> Self {
        Self { recoveries }
    }

    /// Each admitted execution and what it recovers to, in admission order.
    #[must_use]
    pub fn recoveries(&self) -> &[(AdmittedId, Recovery)] {
        &self.recoveries
    }
}

/// A store write whose effect is a lash store row, committed in the same
/// transaction as its tool's outcome: exactly once (ADR 0132 §5). L4
/// (FIG-5174) writes each; the process rows are L6's (FIG-5175).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreLocalEffect {
    /// Start a process: its registry row and its actor, ready.
    ProcessStart(ProcessStartRows),
    /// Create a trigger subscription.
    TriggerCreate(StoreLocalRows),
    /// Delete a trigger subscription.
    TriggerDelete(StoreLocalRows),
    /// Send a signal: a mailbox row on the target plus a wake.
    SignalSend(StoreLocalRows),
    /// Spawn a child session.
    ChildSessionSpawn(StoreLocalRows),
    /// The store half of an intent realization.
    RealizationStore(StoreLocalRows),
}

/// The rows of one store-local effect, encoded by L4 (FIG-5174).
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
    #[expect(dead_code, reason = "V0 (FIG-5170): admit and its fold construct it")]
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
}

/// What a round presents to the next model call, in declared order: a pure
/// function of its committed records.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Presentation {
    calls: Vec<ToolCallId>,
}

impl Presentation {
    /// The presentation of `calls`, in declared order.
    #[must_use]
    pub fn new(calls: Vec<ToolCallId>) -> Self {
        Self { calls }
    }

    /// The presented calls, in declared order.
    #[must_use]
    pub fn calls(&self) -> &[ToolCallId] {
        &self.calls
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
    _tx: &mut ActorTx,
    _owner: &OwnerKey,
    _run: RunSeq,
    _drafts: Vec<ExecutionDraft>,
) -> Result<Vec<AdmittedExecution>, AdmissionRefusal> {
    todo!("V0 (FIG-5170): record admit and x_start for every draft in one transaction")
}

/// Run `admitted`'s body under its limit, the context's cancel token and
/// the stop grace, and answer its outcome. Reports the body's entry to the
/// context's probe.
pub async fn run_body(
    _cx: &ActorContext,
    _admitted: &AdmittedExecution,
    _body: ToolBody,
) -> AttemptOutcome {
    todo!("V0 (FIG-5170): run a catalog body under its limit, cancel and stop grace")
}

/// Record `admitted`'s outcome on `tx`, with the store-local effect that
/// commits with it.
///
/// # Errors
///
/// [`SettleRefusal`]; nothing is recorded.
pub fn settle(
    _tx: &mut ActorTx,
    _admitted: &AdmittedExecution,
    _outcome: AttemptOutcome,
    _store_local: Option<StoreLocalEffect>,
) -> Result<(), SettleRefusal> {
    todo!(
        "V0 (FIG-5170): record the outcome and its store-local effect; L4 (FIG-5174) group-commits"
    )
}

/// Fold an owner's run records into what each admitted execution recovers
/// to, under the policies `current` declares. Calls no producer.
///
/// # Errors
///
/// [`FoldRefusal`] when the rows are inconsistent.
pub fn fold(_rows: &[RunRecordRow], _current: &PolicyView) -> Result<RunFold, FoldRefusal> {
    todo!("V0 (FIG-5170): fold run records to recoveries without calling a producer")
}

/// Admit a tool round inside the `model.done` transaction: its membership,
/// pinned policies, limits and wait deadlines, and an `x_start` for every
/// member.
///
/// # Errors
///
/// [`AdmissionRefusal`]; nothing is recorded.
pub fn admit_round(
    _tx: &mut ActorTx,
    _round: RoundDraft,
) -> Result<AdmittedRound, AdmissionRefusal> {
    todo!("L4 (FIG-5174): admit a tool round inside model.done")
}

/// Record a round's presentation inside the `round.present+model.start`
/// transaction, from its committed records, in declared order.
pub fn present(_tx: &mut ActorTx, _round: &AdmittedRound, _fold: &RunFold) -> Presentation {
    todo!("L4 (FIG-5174): present a round in declared order from its committed records")
}

/// The tool-round methods of the context: what the deleted controller trait's
/// Run-record family became. Their names keep the meaning that survives.
impl ActorContext {
    /// Record one record of a logical Run under `name`, and answer it.
    ///
    /// # Errors
    ///
    /// The record's refusal.
    pub async fn record_run_record(
        &self,
        _name: String,
        _step: crate::RunRecordStep<'_>,
    ) -> Result<lash_core_store::tool_run::RunJournalEntry, crate::RuntimeEffectControllerError>
    {
        todo!("V0 (FIG-5170): record a Run record as a run_records row")
    }

    /// Register a short record now, exposing its notification to the owner.
    pub fn start_run_record<'run>(
        &'run self,
        _name: String,
        _step: crate::RunRecordStep<'run>,
    ) -> crate::tool_dispatch::RunStepHandle<'run, crate::tool_run::RunJournalEntry> {
        todo!("V0 (FIG-5170): start a Run record as an admitted execution")
    }

    /// Register one independently completing attempt now.
    pub fn start_run_attempt<'run>(
        &'run self,
        _name: String,
        _step: crate::tool_dispatch::RunAttemptStep<'run>,
    ) -> crate::tool_dispatch::RunAttemptHandle<'run> {
        todo!("V0 (FIG-5170): start a Run attempt through admit, run_body and settle")
    }

    /// Register one declared-start launch and discharge.
    pub fn start_run_prepare<'run>(
        &'run self,
        _name: String,
        _step: crate::tool_dispatch::RunStartPrepareStep<'run>,
    ) -> crate::tool_dispatch::RunStepHandle<'run, crate::tool_dispatch::RunStartPrepared> {
        todo!("V0 (FIG-5170): start a declared-start preparation as an admitted execution")
    }

    /// Register a retry backoff now, its deadline recorded before it starts.
    pub fn start_run_retry(&self, _backoff_ms: u64) -> crate::tool_dispatch::RunRetryTimer<'_> {
        todo!("L4 (FIG-5174): record a retry with its due time and register the due source")
    }

    /// Arm a call's source before its attempt receives the completion key.
    ///
    /// # Errors
    ///
    /// The source's refusal.
    pub async fn arm_run_source(
        &self,
        _descriptor: crate::tool_run::SourceDescriptor,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        todo!("L4 (FIG-5174): arm a Run source as a pinned wait")
    }

    /// Attach the process terminal using the exact source admitted by the
    /// Run.
    ///
    /// # Errors
    ///
    /// The source's refusal.
    pub async fn attach_run_process_terminal(
        &self,
        _descriptor: crate::tool_run::SourceDescriptor,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        todo!("L4 (FIG-5174): attach a process-terminal source as a process_terminal wait")
    }

    /// Race the Run's sources, the turn's cancel and the selectable
    /// notifications through L5's `race`.
    ///
    /// # Errors
    ///
    /// The race's refusal.
    pub async fn await_run_sources(
        &self,
        _subscriptions: Vec<crate::tool_run::SourceSubscription>,
        _selectable: Vec<crate::tool_dispatch::SelectKey>,
        _cancel: crate::TurnCancelWait,
    ) -> Result<crate::tool_dispatch::RunSourceWake, crate::RuntimeEffectControllerError> {
        todo!("L4 (FIG-5174): race Run sources through waits::race")
    }

    /// The index of the first of `keys` to complete.
    ///
    /// # Errors
    ///
    /// The race's refusal.
    pub async fn select_run_sources(
        &self,
        _keys: Vec<crate::tool_dispatch::SelectKey>,
    ) -> Result<usize, crate::RuntimeEffectControllerError> {
        todo!("L4 (FIG-5174): select the first completed Run source")
    }

    /// Cancel at the source authority and return its winning seal.
    ///
    /// # Errors
    ///
    /// The source's refusal.
    pub async fn cancel_run_source(
        &self,
        _descriptor: crate::tool_run::SourceDescriptor,
    ) -> Result<crate::tool_run::SourceSeal, crate::RuntimeEffectControllerError> {
        todo!("L4 (FIG-5174): cancel a Run source, first resolution wins")
    }

    /// Issue the store half of an intent realization as a store-local effect.
    ///
    /// # Errors
    ///
    /// The realization's refusal.
    pub async fn issue_run_realization<'run>(
        &'run self,
        _request: crate::tool_dispatch::RealizationRequest,
    ) -> Result<crate::tool_dispatch::IssuedRealization<'run>, crate::RuntimeEffectControllerError>
    {
        todo!("L4 (FIG-5174): realize an intent as a store-local effect of its call's outcome")
    }

    /// Attach to previously issued realization work.
    ///
    /// # Errors
    ///
    /// The realization's refusal.
    pub async fn attach_run_realization<'run>(
        &'run self,
        _invocation_id: String,
    ) -> Result<
        crate::tool_dispatch::RunSelectable<'run, crate::tool_dispatch::RealizationReceipt>,
        crate::RuntimeEffectControllerError,
    > {
        todo!("L4 (FIG-5174): read a realization from its call's committed outcome")
    }

    /// The tool-round effects: `ToolAttempt` and `RestoreRunMaterial`
    /// (admitted executions), `PresentToolResult` (a write in
    /// `round.present+model.start`), `Trigger`, `IngestTriggerOccurrence`
    /// and `AdmitTriggerDelivery` (store-local effects of their call's
    /// outcome). Any other command is refused.
    ///
    /// # Errors
    ///
    /// The effect's refusal.
    pub async fn tool_effect(
        &self,
        _envelope: crate::RuntimeEffectEnvelope,
        _local: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        todo!("L4 (FIG-5174): run a tool-round effect as an admitted execution or a round write")
    }
}
