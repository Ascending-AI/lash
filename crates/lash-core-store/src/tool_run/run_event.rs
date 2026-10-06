//! K3 and K9: the Run's event log and its reported-retry schedule
//! (FIG-4877, FIG-4879 and FIG-4880 implement them).
//!
//! The sole active admitted segment of a logical Run appends ordered events
//! in records. Each event has a stable ordinal; a record is one journal
//! entry holding a nonempty batch, so a fused record still names every
//! event it carries. The small singleton is four records: admission (A),
//! attempt (X), decision (D) and presentation with its incorporation (V).
//! Replay reads the chosen events in ordinal order; it never races ready
//! futures to choose an old event again.
//!
//! [`RunLedger`] is the pure fold every producer and every replay applies.
//! It refuses an event that breaks the contract: a gap in ordinals, an
//! append from a segment that is not the active one, a second
//! final-or-cancel, a body or retry the admission did not issue, a
//! declaration issued before every lower final rank is seated, presentation
//! before its declarations settle, admission after an AbortRun or Closing,
//! and settlement while protected work remains.
//!
//! A final's declared start (K5, FIG-4884) drains inside its declarations:
//! it is admitted with them, launched under its key, and discharged — its
//! recorded cancel policy followed and its consumer hold released — before
//! they settle. A start key names one start of the Run.
//!
//! A final's intent realization (ADR 0130) is likewise admitted with its
//! declarations and must record its receipt — `Realized`, carrying a
//! [`MaterialRole::RealizationReceipt`](super::material::MaterialRole)
//! material the Run's schedule selected — before they settle. A
//! realization key names one realization of the Run; the work itself runs
//! in its own invocation, outside this journal.

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU32;

use lash_sansio::ToolCallId;
use serde::{Deserialize, Serialize};

use super::admission::{ExecutionPolicy, RoundAdmission};
pub use super::aggregate::{AggregateConsumer, AggregateLeaf, AggregatePlan};
use super::material::{MaterialEntry, MaterialRef};
use super::tool_hooks::{AfterCheckVerdict, BeforeSelection, CheckRecord, HookCause};
use crate::ProcessId;
use crate::await_event_identity::AwaitEventKey;
use crate::effect_opener::EffectOpener;
use crate::process_identity::StartKey;

/// The idempotency key of one final's intent realization, in the invocation
/// it runs under (ADR 0130): `run:{opener}:{call_id}:realize`.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct RealizationKey(String);

impl RealizationKey {
    /// The realization key of `call_id` in the Run `opener` opens.
    #[must_use]
    pub fn for_call(opener: &EffectOpener, call_id: &ToolCallId) -> Self {
        Self(format!(
            "run:{}:{call_id}:realize",
            opener.identity_encoding()
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RealizationKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The ordinal of an attempt of one logical call, from 1. A crash
/// redelivery keeps it; only a reported retry advances it.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct AttemptOrdinal(NonZeroU32);

impl AttemptOrdinal {
    pub const FIRST: Self = Self(NonZeroU32::MIN);

    #[must_use]
    pub const fn new(value: u32) -> Option<Self> {
        match NonZeroU32::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }

    /// The ordinal a reported retry issues next.
    #[must_use]
    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

impl fmt::Display for AttemptOrdinal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// The stable ordinal of one event in its logical Run, from 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunEventOrdinal(pub u64);

/// The physical segment of a logical Run that appended a record, from 0.
/// A successor segment takes the next ordinal when ownership transfers.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct SegmentOrdinal(pub u32);

/// What one admitted application attempt settled as (X).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "result",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AttemptOutcome {
    Completed(MaterialRef),
    Waiting(CompletionSource),
    Failed(KnownFailure),
    Interrupted,
    TimedOut {
        cause: LimitCause,
        evidence: AvailableEvidence,
    },
    Cancelled {
        evidence: AvailableEvidence,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompletionSource {
    Pending {
        source: AwaitEventKey,
        metadata: MaterialRef,
        start: Option<Box<PendingStart>>,
    },
    Deferred {
        source: AwaitEventKey,
    },
    DeferredStart {
        source: AwaitEventKey,
        start_key: StartKey,
        obligation: MaterialRef,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownFailure {
    pub output: MaterialRef,
    pub reason: KnownFailureReason,
    pub suggested_delay_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnownFailureReason {
    Reported,
    DeclarationRefused,
    StartRefused,
}

pub use lash_sansio::LimitCause;

/// Only material actually retained before an attempt stopped.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AvailableEvidence {
    pub retained: Option<MaterialRef>,
}

impl AttemptOutcome {
    /// A known failure or slice expiry may use the pinned Repeatable contract.
    /// Failure reasons describe facts; they never carry retry permission.
    pub fn may_repeat(&self) -> bool {
        matches!(
            self,
            Self::Failed(_)
                | Self::TimedOut {
                    cause: LimitCause::ExecutionSlice,
                    ..
                }
        )
    }

    pub fn output(&self) -> Option<&MaterialRef> {
        match self {
            Self::Completed(output) => Some(output),
            Self::Failed(failure) => Some(&failure.output),
            Self::TimedOut { evidence, .. } | Self::Cancelled { evidence } => {
                evidence.retained.as_ref()
            }
            Self::Waiting(_) | Self::Interrupted => None,
        }
    }
}

/// The obligation of a pending call that declared a process start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingStart {
    pub start_key: StartKey,
    pub obligation: MaterialRef,
}

/// Where a final result came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResultSource {
    /// A recorded attempt's Done or Failed output.
    Attempt { attempt: AttemptOrdinal },
    /// The resolved seal of the source the attempt parked on.
    DeferredCompletion {
        attempt: AttemptOrdinal,
        resolved: Box<MaterialRef>,
    },
    /// The cached success the winning before-check supplied.
    Cached,
}

/// The one final-or-cancel decision of a call (D).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum CallDecision {
    /// The call's result is final. Its declarations, if it `declares`,
    /// drain before its presentation.
    Final {
        source: ResultSource,
        declares: bool,
    },
    /// A check denied the call.
    Denied,
    /// A check cancelled only this call. Its attributed cause stays in the
    /// admission or after-check record; aggregate consumers see a rejection.
    CheckCancelled,
    /// The Run's control cancelled the call, independently of check replies.
    Cancelled,
    /// A check returned AbortRun: the call fails and the Run stops.
    Aborted,
}

/// The lifecycle of a logical Run, at its owner.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RunLifecycle {
    Live,
    /// Admits nothing new; issued and protected work still finishes.
    Closing,
    Settled,
}

/// One Run event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunEvent {
    /// A whole round refused before it admitted any member or ran a body.
    AdmissionRefused {
        cause: super::AdmissionRefusal,
    },
    /// A declared isolation route cannot honor admission. No start was issued.
    IsolationRefused {
        cause: super::IsolatedStartRefusal,
    },
    /// Source order, aliases and the timers' recorded admission instant.
    AggregateAdmitted {
        plan: AggregatePlan,
        admitted_at_ms: u64,
    },
    /// A generic aggregate timer elapsed, in the Run's recorded schedule.
    TimerElapsed {
        aggregate: String,
        leaf: u32,
    },
    /// Eligible external cancellation discharged by logical Closing only.
    CancelDischarged {
        call_id: ToolCallId,
    },
    /// A: a whole round, which issues attempt 1 of every member it selected
    /// to execute.
    Admitted {
        round: RoundAdmission,
    },
    /// X: one issued attempt's durable result.
    AttemptRecorded {
        call_id: ToolCallId,
        attempt: AttemptOrdinal,
        result: AttemptOutcome,
    },
    /// The Run's schedule selected an open source's seal; the call's
    /// decision follows from it.
    SourceSealed {
        call_id: ToolCallId,
        seal: super::SourceSeal,
    },
    /// X: a retained source's result passed through the admitted result transforms.
    SourceCaptured {
        call_id: ToolCallId,
        output: MaterialRef,
    },
    /// Eligibility and backoff are fixed before registering a durable timer.
    RetryTimerRegistered {
        call_id: ToolCallId,
        failed: AttemptOrdinal,
        next: AttemptOrdinal,
        backoff_ms: u64,
    },
    /// K9: a repeatable attempt failure, its backoff and the registration
    /// of the next attempt, as one schedule entry.
    RetryScheduled {
        call_id: ToolCallId,
        failed: AttemptOrdinal,
        next: AttemptOrdinal,
        backoff_ms: u64,
    },
    /// The after-check's messages and observations, owned by its D record.
    CheckContributions {
        call_id: ToolCallId,
        material: MaterialRef,
    },
    /// D: the call's one decision, with the after-check
    /// record when a result candidate existed.
    Decided {
        call_id: ToolCallId,
        decision: CallDecision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<CheckRecord<AfterCheckVerdict>>,
    },
    /// A final's declarations begin; every lower final rank is seated.
    DeclarationsIssued {
        call_id: ToolCallId,
    },
    /// A final's declarations finished; the call is seated.
    DeclarationsSettled {
        call_id: ToolCallId,
    },
    /// K5: a final's declared start is admitted with its declarations. From
    /// here it starts under `start_key` whatever becomes of the Run.
    StartAdmitted {
        call_id: ToolCallId,
        start_key: StartKey,
    },
    /// The start's process is registered under its key. A start its call
    /// declared as an intent records its launch receipt, the call's
    /// `StartProcess` intent outcome, as a realization receipt this record
    /// owns; the call's presentation reports it and the session possesses
    /// the process from it.
    StartLaunched {
        call_id: ToolCallId,
        start_key: StartKey,
        process_id: ProcessId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        receipt: Option<MaterialRef>,
    },
    /// The registrar refused a deferred call's admitted start for good, so
    /// no process exists and nothing is owed: a retry would only meet the
    /// refusal again, as a closed starter scope does. `output` is the call's
    /// failure capture, which reports the refusal as the call's
    /// `StartProcess` intent outcome; the call's deferred completion is it.
    StartRefused {
        call_id: ToolCallId,
        start_key: StartKey,
        output: MaterialRef,
    },
    /// The start's recorded cancel policy is followed and its consumer hold
    /// released. `cancelled` when a cancellation of the Run made that policy
    /// cancel the process.
    StartDischarged {
        call_id: ToolCallId,
        start_key: StartKey,
        cancelled: bool,
    },
    /// A final's intent realization is admitted with its declarations; from
    /// here it runs in its own invocation under `key`.
    RealizationAdmitted {
        call_id: ToolCallId,
        key: RealizationKey,
    },
    /// The durable invocation admitted by the realization send.
    RealizationIssued {
        call_id: ToolCallId,
        invocation_id: String,
    },
    /// The realization's receipt, selected by the Run's schedule.
    Realized {
        call_id: ToolCallId,
        receipt: MaterialRef,
    },
    /// V: the call's model-facing presentation.
    Presented {
        call_id: ToolCallId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        presentation: Option<MaterialRef>,
        /// The original declared cause, retained when presentation used fallback.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<HookCause>,
    },
    /// The consumer took the call's value.
    Consumed {
        call_id: ToolCallId,
    },
    /// The call's result joined the owner's history.
    Incorporated {
        call_id: ToolCallId,
    },
    Lifecycle {
        state: RunLifecycle,
    },
}

/// One journal record of a Run: a nonempty batch of events whose ordinals
/// start at `first`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    pub segment: SegmentOrdinal,
    pub first: RunEventOrdinal,
    pub events: Vec<RunEvent>,
    /// Original observation data. Retained reads never restore a permit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<RunTraceFacts>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunTraceFacts {
    pub at_ms: u64,
    pub owner: lash_trace::TraceToolOwner,
    pub admissions: BTreeMap<ToolCallId, lash_trace::DurableTraceScope>,
    /// Detailed producer observations, replayed as data and emitted only with
    /// the owning call's first-writer receipt permit.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub projections: BTreeMap<ToolCallId, serde_json::Value>,
}

/// One Run record as its journal entry holds it (FIG-4877): the record and
/// the canonical material it owns. A owns the prepared request and a cached
/// result, X the attempt output, V only presentation bytes distinct from the
/// output; D and the declaration boundaries own none. Every reference a
/// record names resolves to material of this entry or of an entry served
/// before it in the same journal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunJournalEntry {
    pub record: RunRecord,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub materials: Vec<MaterialEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub state: Vec<super::StateResolution>,
}

/// An independent X receipt. Its place in the Run is chosen by the recorded
/// selection schedule, rather than by the order its body finishes on replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunAttemptEntry {
    pub call_id: ToolCallId,
    pub attempt: AttemptOrdinal,
    pub result: AttemptOutcome,
    pub materials: Vec<MaterialEntry>,
}

/// Why the fold refused a record.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error, schemars::JsonSchema,
)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunEventRefusal {
    #[error("aggregate {key} has an invalid or changed operand mapping")]
    AggregateShape { key: String },
    #[error("aggregate {key} was never admitted")]
    UnknownAggregate { key: String },
    #[error("aggregate {key} timer {leaf} is out of order")]
    TimerOrder { key: String, leaf: u32 },
    #[error("call {call_id} discharged cancellation outside logical Closing")]
    CancelOrder { call_id: ToolCallId },
    #[error("a record holds no event")]
    EmptyRecord,
    #[error("expected event ordinal {expected}, record starts at {found}")]
    OrdinalGap { expected: u64, found: u64 },
    #[error("segment {found} appended while segment {active} is active")]
    NotActiveSegment { active: u32, found: u32 },
    #[error("segment {found} appended after segment {latest} took ownership")]
    StaleSegment { latest: u32, found: u32 },
    #[error("a round of another owner was admitted")]
    ForeignOwner,
    #[error("the Run admits no new round in its current state")]
    AdmissionClosed,
    #[error("call {call_id} was already admitted")]
    DuplicateCall { call_id: ToolCallId },
    #[error("call {call_id} was never admitted")]
    UnknownCall { call_id: ToolCallId },
    #[error("call {call_id} has no issued, unrecorded attempt {attempt}")]
    AttemptNotIssued {
        call_id: ToolCallId,
        attempt: AttemptOrdinal,
    },
    #[error("call {call_id} cannot retry attempt {failed} as {next}")]
    RetryNotEligible {
        call_id: ToolCallId,
        failed: AttemptOrdinal,
        next: AttemptOrdinal,
    },
    #[error("call {call_id} already has its one decision")]
    DecidedTwice { call_id: ToolCallId },
    #[error("call {call_id}'s decision does not follow from its record")]
    DecisionUnsupported { call_id: ToolCallId },
    #[error("call {call_id} issued declarations before every lower final rank was seated")]
    DrainFrontier { call_id: ToolCallId },
    #[error("call {call_id} is out of order at its boundary")]
    BoundaryOrder { call_id: ToolCallId },
    #[error("start key {start_key} already names a start of this Run")]
    StartReused { start_key: StartKey },
    #[error("call {call_id}'s start {start_key} is out of order")]
    StartOrder {
        call_id: ToolCallId,
        start_key: StartKey,
    },
    #[error("call {call_id}'s declarations cannot settle while start {start_key} is owed")]
    StartOwed {
        call_id: ToolCallId,
        start_key: StartKey,
    },
    #[error("call {call_id}'s realization is out of order")]
    RealizationOrder { call_id: ToolCallId },
    #[error("call {call_id}'s declarations cannot settle while its realization is owed")]
    RealizationOwed { call_id: ToolCallId },
    #[error("the lifecycle cannot move from {from:?} to {to:?}")]
    Lifecycle {
        from: RunLifecycle,
        to: RunLifecycle,
    },
    #[error("the Run cannot settle while call {call_id} owes protected work")]
    UnsettledWork { call_id: ToolCallId },
}

/// How far a call's declared start has drained.
#[derive(Clone, Debug, PartialEq, Eq)]
enum StartProgress {
    Admitted,
    Launched,
    Discharged,
}

/// How far a call's admitted realization has run.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RealizationProgress {
    Admitted,
    Issued(String),
    Realized,
}

#[derive(Clone, Debug)]
struct CallState {
    cancel: super::ExternalCancelPolicy,
    cancel_discharged: bool,
    selection: BeforeSelection,
    execution: ExecutionPolicy,
    /// The attempt issued and not yet recorded.
    outstanding: Option<AttemptOrdinal>,
    attempts: BTreeMap<AttemptOrdinal, AttemptOutcome>,
    retry_timer: Option<(AttemptOrdinal, AttemptOrdinal, u64)>,
    /// The schedule selected the open source's seal.
    source_sealed: bool,
    decision: Option<(u64, CallDecision)>,
    declarations_issued: bool,
    /// The declared start, admitted with the declarations.
    start: Option<(StartKey, StartProgress)>,
    /// The intent realization, admitted with the declarations.
    realization: Option<(RealizationKey, RealizationProgress)>,
    seated: bool,
    presented: bool,
    consumed: bool,
    incorporated: bool,
}

/// The fold over a logical Run's records.
#[derive(Clone, Debug)]
pub struct RunLedger {
    owner: EffectOpener,
    next: u64,
    latest_segment: Option<SegmentOrdinal>,
    lifecycle: RunLifecycle,
    aborted: bool,
    decisions: u64,
    calls: BTreeMap<ToolCallId, CallState>,
    aggregates: BTreeMap<String, AggregatePlan>,
    elapsed: std::collections::BTreeSet<(String, u32)>,
    /// Every admitted round's capacity scope and unique members (K1).
    rounds: Vec<(super::CapacityScope, Vec<ToolCallId>)>,
}

impl RunLedger {
    /// An empty, live ledger of the Run `owner` opens.
    #[must_use]
    pub fn new(owner: EffectOpener) -> Self {
        Self {
            owner,
            next: 0,
            latest_segment: None,
            lifecycle: RunLifecycle::Live,
            aborted: false,
            decisions: 0,
            calls: BTreeMap::new(),
            aggregates: BTreeMap::new(),
            elapsed: std::collections::BTreeSet::new(),
            rounds: Vec::new(),
        }
    }

    #[must_use]
    pub fn lifecycle(&self) -> RunLifecycle {
        self.lifecycle
    }

    #[must_use]
    pub fn has_call(&self, call_id: &ToolCallId) -> bool {
        self.calls.contains_key(call_id)
    }

    #[must_use]
    pub fn consumed(&self, call_id: &ToolCallId) -> bool {
        self.calls.get(call_id).is_some_and(|call| call.consumed)
    }

    /// Calls whose admitted policy still owes external cancellation at Closing.
    #[must_use]
    pub fn eligible_cancellations(&self) -> Vec<ToolCallId> {
        self.calls
            .iter()
            .filter(|(_, call)| {
                call.selection == BeforeSelection::Execute
                    && call.cancel == super::ExternalCancelPolicy::CancelExternalWork
                    && !call.cancel_discharged
                    && !matches!(
                        call.decision,
                        Some((
                            _,
                            CallDecision::Final { .. }
                                | CallDecision::Denied
                                | CallDecision::CheckCancelled
                                | CallDecision::Aborted
                        ))
                    )
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    #[must_use]
    pub fn cancel_discharged(&self, call_id: &ToolCallId) -> bool {
        self.calls
            .get(call_id)
            .is_some_and(|call| call.cancel_discharged)
    }

    /// Whether an AbortRun decision stopped the Run.
    #[must_use]
    pub fn aborted(&self) -> bool {
        self.aborted
    }

    /// The ordinal the next event takes.
    #[must_use]
    pub fn next_ordinal(&self) -> RunEventOrdinal {
        RunEventOrdinal(self.next)
    }

    /// The call's rank, derived from accepted decision order, starting at 1.
    /// Attempts and refused records take no rank.
    #[must_use]
    pub fn decision_rank(&self, call_id: &ToolCallId) -> Option<u64> {
        self.calls
            .get(call_id)?
            .decision
            .as_ref()
            .map(|(rank, _)| *rank)
    }

    /// Whether a final ranked `rank` may issue its declarations (L18): every
    /// committed final ranked below it is seated, whether or not it declared.
    /// An intent-free final seats at its decision without waiting, so its
    /// seat certifies nothing about the ranks below it; the frontier is
    /// therefore every lower rank, never only the one just below.
    #[must_use]
    pub fn drain_frontier_open(&self, rank: u64) -> bool {
        self.calls.values().all(|other| match &other.decision {
            Some((lower, CallDecision::Final { .. })) if *lower < rank => other.seated,
            _ => true,
        })
    }

    /// The calls whose admitted realization has not recorded its receipt,
    /// in call order: each is owed its `Realized` by whichever segment owns
    /// the Run next.
    #[must_use]
    pub fn owed_realizations(&self) -> Vec<ToolCallId> {
        self.calls
            .iter()
            .filter_map(|(call_id, call)| match &call.realization {
                Some((_, RealizationProgress::Admitted | RealizationProgress::Issued(_))) => {
                    Some(call_id.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// The admitted starts not yet discharged, in call order: each is owed
    /// its launch under its key, or its discharge, by whichever segment owns
    /// the Run next.
    #[must_use]
    pub fn owed_starts(&self) -> Vec<StartKey> {
        self.calls
            .values()
            .filter_map(|call| match &call.start {
                Some((key, progress)) if *progress != StartProgress::Discharged => {
                    Some(key.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// The calls counted against `scope`'s `max_tool_calls` (K1): a held
    /// round counts all its members until every one is presented; a cell's
    /// rounds count for the Run's whole life.
    #[must_use]
    pub fn counted(&self, scope: &super::CapacityScope) -> u32 {
        self.rounds
            .iter()
            .filter(|(admitted, _)| admitted == scope)
            .map(|(_, members)| self.reserved(scope, members))
            .sum()
    }

    /// The tool-call capacity the Run holds: every unretired held round and
    /// every cell round. A continuation carries exactly this value (K6).
    #[must_use]
    pub fn held_calls(&self) -> u32 {
        self.rounds
            .iter()
            .map(|(scope, members)| self.reserved(scope, members))
            .sum()
    }

    fn reserved(&self, scope: &super::CapacityScope, members: &[ToolCallId]) -> u32 {
        let retired = *scope == super::CapacityScope::Held
            && members
                .iter()
                .all(|id| self.calls.get(id).is_some_and(|call| call.presented));
        if retired { 0 } else { members.len() as u32 }
    }

    /// Issued local attempts or retry timers not yet accepted by the Run.
    #[must_use]
    pub fn unacknowledged_local(&self) -> usize {
        self.calls
            .values()
            .filter(|call| call.outstanding.is_some() || call.retry_timer.is_some())
            .count()
    }

    /// Launched starts whose policy and consumer hold have not discharged.
    #[must_use]
    pub fn owed_cancels(&self) -> Vec<StartKey> {
        self.calls
            .values()
            .filter_map(|call| match &call.start {
                Some((key, StartProgress::Launched)) => Some(key.clone()),
                _ => None,
            })
            .collect()
    }

    /// Fence predecessor publication as soon as the successor is admitted.
    pub fn admit_successor(&mut self, successor: SegmentOrdinal) {
        self.latest_segment = Some(
            self.latest_segment
                .map_or(successor, |latest| latest.max(successor)),
        );
    }

    /// Apply `record`, appended by `active`, all of it or none of it.
    ///
    /// # Errors
    ///
    /// The first [`RunEventRefusal`]; the ledger is then unchanged.
    pub fn append(
        &mut self,
        active: SegmentOrdinal,
        record: &RunRecord,
    ) -> Result<(), RunEventRefusal> {
        if record.events.is_empty() {
            return Err(RunEventRefusal::EmptyRecord);
        }
        if let Some(latest) = self.latest_segment
            && record.segment < latest
        {
            return Err(RunEventRefusal::StaleSegment {
                latest: latest.0,
                found: record.segment.0,
            });
        }
        if record.segment != active {
            return Err(RunEventRefusal::NotActiveSegment {
                active: active.0,
                found: record.segment.0,
            });
        }
        if record.first.0 != self.next {
            return Err(RunEventRefusal::OrdinalGap {
                expected: self.next,
                found: record.first.0,
            });
        }
        let mut next = self.clone();
        for event in &record.events {
            next.apply(event)?;
            next.next += 1;
        }
        next.latest_segment = Some(record.segment);
        *self = next;
        Ok(())
    }

    fn call(&mut self, call_id: &ToolCallId) -> Result<&mut CallState, RunEventRefusal> {
        self.calls
            .get_mut(call_id)
            .ok_or_else(|| RunEventRefusal::UnknownCall {
                call_id: call_id.clone(),
            })
    }

    fn apply(&mut self, event: &RunEvent) -> Result<(), RunEventRefusal> {
        match event {
            RunEvent::AdmissionRefused { .. } | RunEvent::IsolationRefused { .. } => {
                if self.lifecycle != RunLifecycle::Live || self.aborted {
                    return Err(RunEventRefusal::AdmissionClosed);
                }
                self.aborted = true;
                Ok(())
            }
            RunEvent::AggregateAdmitted { plan, .. } => {
                if self.lifecycle != RunLifecycle::Live || self.aborted {
                    return Err(RunEventRefusal::AdmissionClosed);
                }
                plan.validate()?;
                if self.aggregates.contains_key(&plan.key) {
                    return Err(RunEventRefusal::AggregateShape {
                        key: plan.key.clone(),
                    });
                }
                for leaf in &plan.leaves {
                    if let AggregateLeaf::Call { call_id } = leaf {
                        self.call(call_id)?;
                    }
                }
                self.aggregates.insert(plan.key.clone(), plan.clone());
                Ok(())
            }
            RunEvent::TimerElapsed { aggregate, leaf } => {
                if self.lifecycle != RunLifecycle::Live
                    || !self.aggregates.get(aggregate).is_some_and(|plan| {
                        matches!(
                            plan.leaves.get(*leaf as usize),
                            Some(AggregateLeaf::Timer { .. })
                        )
                    })
                    || !self.elapsed.insert((aggregate.clone(), *leaf))
                {
                    return Err(RunEventRefusal::TimerOrder {
                        key: aggregate.clone(),
                        leaf: *leaf,
                    });
                }
                Ok(())
            }
            RunEvent::CancelDischarged { call_id } => {
                let closing = self.lifecycle == RunLifecycle::Closing;
                let call = self.call(call_id)?;
                if !closing
                    || call.cancel != super::ExternalCancelPolicy::CancelExternalWork
                    || matches!(call.decision, Some((_, CallDecision::Final { .. })))
                    || call.cancel_discharged
                {
                    return Err(RunEventRefusal::CancelOrder {
                        call_id: call_id.clone(),
                    });
                }
                call.cancel_discharged = true;
                Ok(())
            }
            RunEvent::Admitted { round } => self.admit(round),
            RunEvent::AttemptRecorded {
                call_id,
                attempt,
                result,
            } => {
                let call = self.call(call_id)?;
                if call.outstanding != Some(*attempt) {
                    return Err(RunEventRefusal::AttemptNotIssued {
                        call_id: call_id.clone(),
                        attempt: *attempt,
                    });
                }
                call.outstanding = None;
                call.attempts.insert(*attempt, result.clone());
                Ok(())
            }
            RunEvent::SourceSealed { call_id, .. } => {
                let call = self.call(call_id)?;
                if call.decision.is_some()
                    || call.source_sealed
                    || !call.attempts.values().any(|result| {
                        matches!(
                            result,
                            AttemptOutcome::Waiting(CompletionSource::Deferred { .. })
                                | AttemptOutcome::Waiting(CompletionSource::DeferredStart { .. })
                                | AttemptOutcome::Waiting(CompletionSource::Pending { .. })
                        )
                    })
                {
                    return Err(boundary(call_id));
                }
                call.source_sealed = true;
                Ok(())
            }
            RunEvent::SourceCaptured { call_id, output } => {
                let call = self.calls.get(call_id).ok_or_else(|| boundary(call_id))?;
                if output.role != super::MaterialRole::AttemptOutput
                    || output.owner
                        != (super::MaterialOwner::Run {
                            opener: self.owner.clone(),
                        })
                    || call.decision.is_some()
                    || !call.attempts.values().any(|result| {
                        matches!(
                            result,
                            AttemptOutcome::Waiting(CompletionSource::Deferred { .. })
                                | AttemptOutcome::Waiting(CompletionSource::DeferredStart { .. })
                                | AttemptOutcome::Waiting(CompletionSource::Pending { .. })
                        )
                    })
                {
                    return Err(boundary(call_id));
                }
                Ok(())
            }
            RunEvent::RetryTimerRegistered {
                call_id,
                failed,
                next,
                backoff_ms,
            } => {
                let mut candidate = self.clone();
                candidate.schedule_retry(call_id, *failed, *next)?;
                let call = self.call(call_id)?;
                if call.retry_timer.is_some() {
                    return Err(RunEventRefusal::RetryNotEligible {
                        call_id: call_id.clone(),
                        failed: *failed,
                        next: *next,
                    });
                }
                call.retry_timer = Some((*failed, *next, *backoff_ms));
                Ok(())
            }
            RunEvent::RetryScheduled {
                call_id,
                failed,
                next,
                backoff_ms,
            } => {
                if self.call(call_id)?.retry_timer != Some((*failed, *next, *backoff_ms)) {
                    return Err(RunEventRefusal::RetryNotEligible {
                        call_id: call_id.clone(),
                        failed: *failed,
                        next: *next,
                    });
                }
                self.schedule_retry(call_id, *failed, *next)?;
                self.call(call_id)?.retry_timer = None;
                Ok(())
            }
            RunEvent::CheckContributions { call_id, material } => {
                if self.call(call_id)?.decision.is_some()
                    || material.role != super::MaterialRole::CheckContributions
                {
                    return Err(boundary(call_id));
                }
                Ok(())
            }
            RunEvent::Decided {
                call_id,
                decision,
                after,
            } => self.decide(call_id, decision, after.as_ref()),
            RunEvent::DeclarationsIssued { call_id } => self.issue_declarations(call_id),
            RunEvent::DeclarationsSettled { call_id } => {
                let call = self.call(call_id)?;
                if !call.declarations_issued || call.seated {
                    return Err(boundary(call_id));
                }
                if let Some((start_key, progress)) = &call.start
                    && *progress != StartProgress::Discharged
                {
                    return Err(RunEventRefusal::StartOwed {
                        call_id: call_id.clone(),
                        start_key: start_key.clone(),
                    });
                }
                if let Some((_, RealizationProgress::Admitted | RealizationProgress::Issued(_))) =
                    &call.realization
                {
                    return Err(RunEventRefusal::RealizationOwed {
                        call_id: call_id.clone(),
                    });
                }
                call.seated = true;
                Ok(())
            }
            RunEvent::StartAdmitted { call_id, start_key } => self.admit_start(call_id, start_key),
            RunEvent::StartLaunched {
                call_id,
                start_key,
                receipt,
                ..
            } => {
                if receipt
                    .as_ref()
                    .is_some_and(|receipt| receipt.role != super::MaterialRole::RealizationReceipt)
                {
                    return Err(RunEventRefusal::StartOrder {
                        call_id: call_id.clone(),
                        start_key: start_key.clone(),
                    });
                }
                self.advance_start(
                    call_id,
                    start_key,
                    &StartProgress::Admitted,
                    StartProgress::Launched,
                )
            }
            RunEvent::StartRefused {
                call_id,
                start_key,
                output,
            } => {
                if output.role != super::MaterialRole::AttemptOutput {
                    return Err(RunEventRefusal::StartOrder {
                        call_id: call_id.clone(),
                        start_key: start_key.clone(),
                    });
                }
                self.advance_start(
                    call_id,
                    start_key,
                    &StartProgress::Admitted,
                    StartProgress::Discharged,
                )
            }
            RunEvent::StartDischarged {
                call_id, start_key, ..
            } => self.advance_start(
                call_id,
                start_key,
                &StartProgress::Launched,
                StartProgress::Discharged,
            ),
            RunEvent::RealizationAdmitted { call_id, key } => self.admit_realization(call_id, key),
            RunEvent::RealizationIssued {
                call_id,
                invocation_id,
            } => match &mut self.call(call_id)?.realization {
                Some((_, progress @ RealizationProgress::Admitted))
                    if !invocation_id.is_empty() =>
                {
                    *progress = RealizationProgress::Issued(invocation_id.clone());
                    Ok(())
                }
                _ => Err(RunEventRefusal::RealizationOrder {
                    call_id: call_id.clone(),
                }),
            },
            RunEvent::Realized { call_id, receipt } => match &mut self.call(call_id)?.realization {
                Some((_, progress @ RealizationProgress::Issued(_)))
                    if receipt.role == super::MaterialRole::RealizationReceipt =>
                {
                    *progress = RealizationProgress::Realized;
                    Ok(())
                }
                _ => Err(RunEventRefusal::RealizationOrder {
                    call_id: call_id.clone(),
                }),
            },
            RunEvent::Presented { call_id, .. } => {
                let call = self.call(call_id)?;
                let final_unseated =
                    matches!(call.decision, Some((_, CallDecision::Final { .. }))) && !call.seated;
                if call.decision.is_none() || final_unseated || call.presented {
                    return Err(boundary(call_id));
                }
                call.presented = true;
                Ok(())
            }
            RunEvent::Consumed { call_id } => {
                let call = self.call(call_id)?;
                if !call.presented || call.consumed {
                    return Err(boundary(call_id));
                }
                call.consumed = true;
                Ok(())
            }
            RunEvent::Incorporated { call_id } => {
                let call = self.call(call_id)?;
                if !call.presented || call.incorporated {
                    return Err(boundary(call_id));
                }
                call.incorporated = true;
                Ok(())
            }
            RunEvent::Lifecycle { state } => self.move_lifecycle(*state),
        }
    }

    fn admit(&mut self, round: &RoundAdmission) -> Result<(), RunEventRefusal> {
        if round.owner != self.owner {
            return Err(RunEventRefusal::ForeignOwner);
        }
        if self.aborted || self.lifecycle != RunLifecycle::Live {
            return Err(RunEventRefusal::AdmissionClosed);
        }
        for member in &round.members {
            if self.calls.contains_key(&member.call_id) {
                return Err(RunEventRefusal::DuplicateCall {
                    call_id: member.call_id.clone(),
                });
            }
            let selection = member.selection();
            self.calls.insert(
                member.call_id.clone(),
                CallState {
                    cancel: member.policy.cancel,
                    cancel_discharged: false,
                    selection,
                    execution: member.policy.execution,
                    outstanding: (selection == BeforeSelection::Execute)
                        .then_some(AttemptOrdinal::FIRST),
                    attempts: BTreeMap::new(),
                    retry_timer: None,
                    source_sealed: false,
                    decision: None,
                    declarations_issued: false,
                    start: None,
                    realization: None,
                    seated: false,
                    presented: false,
                    consumed: false,
                    incorporated: false,
                },
            );
        }
        self.rounds.push((
            round.capacity.clone(),
            round
                .members
                .iter()
                .map(|member| member.call_id.clone())
                .collect(),
        ));
        Ok(())
    }

    fn schedule_retry(
        &mut self,
        call_id: &ToolCallId,
        failed: AttemptOrdinal,
        next: AttemptOrdinal,
    ) -> Result<(), RunEventRefusal> {
        let stopped = self.aborted || self.lifecycle != RunLifecycle::Live;
        let call = self.call(call_id)?;
        let within_policy = call.execution.permits_repeat(call.execution, failed.get());
        let eligible = !stopped
            && within_policy
            && call.decision.is_none()
            && call.outstanding.is_none()
            && failed.next() == Some(next)
            && call.attempts.keys().next_back() == Some(&failed)
            && call
                .attempts
                .get(&failed)
                .is_some_and(AttemptOutcome::may_repeat);
        if !eligible {
            return Err(RunEventRefusal::RetryNotEligible {
                call_id: call_id.clone(),
                failed,
                next,
            });
        }
        call.outstanding = Some(next);
        Ok(())
    }

    fn decide(
        &mut self,
        call_id: &ToolCallId,
        decision: &CallDecision,
        after: Option<&CheckRecord<AfterCheckVerdict>>,
    ) -> Result<(), RunEventRefusal> {
        let rank = self.decisions + 1;
        let call = self.call(call_id)?;
        if call.decision.is_some() {
            return Err(RunEventRefusal::DecidedTwice {
                call_id: call_id.clone(),
            });
        }
        if !decision_follows(call, decision, after) {
            return Err(RunEventRefusal::DecisionUnsupported {
                call_id: call_id.clone(),
            });
        }
        if let CallDecision::Final {
            declares: false, ..
        } = decision
        {
            call.declarations_issued = true;
            call.seated = true;
        }
        call.decision = Some((rank, decision.clone()));
        call.retry_timer = None;
        if matches!(decision, CallDecision::Aborted) {
            self.aborted = true;
        }
        self.decisions = rank;
        Ok(())
    }

    fn issue_declarations(&mut self, call_id: &ToolCallId) -> Result<(), RunEventRefusal> {
        let call = self.call(call_id)?;
        let Some((rank, CallDecision::Final { declares: true, .. })) = call.decision else {
            return Err(boundary(call_id));
        };
        if call.declarations_issued {
            return Err(boundary(call_id));
        }
        if !self.drain_frontier_open(rank) {
            return Err(RunEventRefusal::DrainFrontier {
                call_id: call_id.clone(),
            });
        }
        self.call(call_id)?.declarations_issued = true;
        Ok(())
    }

    /// Admit a final's declared start: only inside its issued, unsettled
    /// declarations, one start per call, and never under a key another
    /// start of the Run holds.
    fn admit_start(
        &mut self,
        call_id: &ToolCallId,
        start_key: &StartKey,
    ) -> Result<(), RunEventRefusal> {
        let reused = self.calls.values().any(|call| {
            call.start
                .as_ref()
                .is_some_and(|(admitted, _)| admitted == start_key)
        });
        if reused {
            return Err(RunEventRefusal::StartReused {
                start_key: start_key.clone(),
            });
        }
        let call = self.call(call_id)?;
        let declaring = matches!(
            call.decision,
            Some((_, CallDecision::Final { declares: true, .. }))
        );
        let deferred = call.decision.is_none()
            && match call.attempts.values().next_back() {
                Some(AttemptOutcome::Waiting(CompletionSource::DeferredStart {
                    start_key: recorded,
                    ..
                })) => recorded == start_key,
                Some(AttemptOutcome::Waiting(CompletionSource::Pending {
                    start: Some(start),
                    ..
                })) => &start.start_key == start_key,
                _ => false,
            };
        if !(deferred || declaring && call.declarations_issued)
            || call.seated
            || call.start.is_some()
        {
            return Err(RunEventRefusal::StartOrder {
                call_id: call_id.clone(),
                start_key: start_key.clone(),
            });
        }
        call.start = Some((start_key.clone(), StartProgress::Admitted));
        Ok(())
    }

    pub fn realization_invocation(&self, call_id: &ToolCallId) -> Option<&str> {
        match &self.calls.get(call_id)?.realization {
            Some((_, RealizationProgress::Issued(invocation_id))) => Some(invocation_id),
            _ => None,
        }
    }

    /// Admit a final's intent realization: only inside its issued,
    /// unsettled declarations, one realization per call, and never under a
    /// key another call of the Run holds.
    fn admit_realization(
        &mut self,
        call_id: &ToolCallId,
        key: &RealizationKey,
    ) -> Result<(), RunEventRefusal> {
        let reused = self.calls.values().any(|call| {
            call.realization
                .as_ref()
                .is_some_and(|(admitted, _)| admitted == key)
        });
        if reused {
            return Err(RunEventRefusal::RealizationOrder {
                call_id: call_id.clone(),
            });
        }
        let call = self.call(call_id)?;
        let admitting = matches!(
            call.decision,
            Some((_, CallDecision::Final { declares: true, .. }))
        ) && call.declarations_issued
            && !call.seated
            && call.realization.is_none();
        if !admitting {
            return Err(RunEventRefusal::RealizationOrder {
                call_id: call_id.clone(),
            });
        }
        call.realization = Some((key.clone(), RealizationProgress::Admitted));
        Ok(())
    }

    fn advance_start(
        &mut self,
        call_id: &ToolCallId,
        start_key: &StartKey,
        from: &StartProgress,
        to: StartProgress,
    ) -> Result<(), RunEventRefusal> {
        let call = self.call(call_id)?;
        match &mut call.start {
            Some((admitted, progress)) if admitted == start_key && progress == from => {
                *progress = to;
                Ok(())
            }
            _ => Err(RunEventRefusal::StartOrder {
                call_id: call_id.clone(),
                start_key: start_key.clone(),
            }),
        }
    }

    fn move_lifecycle(&mut self, to: RunLifecycle) -> Result<(), RunEventRefusal> {
        let from = self.lifecycle;
        if to <= from {
            return Err(RunEventRefusal::Lifecycle { from, to });
        }
        if to == RunLifecycle::Settled {
            for (call_id, call) in &self.calls {
                let protected_owed = match &call.decision {
                    None => true,
                    Some((_, CallDecision::Final { .. })) => !call.seated || !call.presented,
                    Some(_) => false,
                };
                let cancel_owed = matches!(call.decision, Some((_, CallDecision::Cancelled)))
                    && call.cancel == super::ExternalCancelPolicy::CancelExternalWork
                    && !call.cancel_discharged;
                if protected_owed
                    || cancel_owed
                    || call.outstanding.is_some()
                    || call
                        .start
                        .as_ref()
                        .is_some_and(|(_, progress)| progress != &StartProgress::Discharged)
                {
                    return Err(RunEventRefusal::UnsettledWork {
                        call_id: call_id.clone(),
                    });
                }
            }
        }
        self.lifecycle = to;
        Ok(())
    }
}

fn boundary(call_id: &ToolCallId) -> RunEventRefusal {
    RunEventRefusal::BoundaryOrder {
        call_id: call_id.clone(),
    }
}

/// Whether `decision` follows from the call's admission selection, its
/// recorded attempts and its after-check record.
fn decision_follows(
    call: &CallState,
    decision: &CallDecision,
    after: Option<&CheckRecord<AfterCheckVerdict>>,
) -> bool {
    let after_winner = after
        .and_then(CheckRecord::winner)
        .map(|reply| &reply.verdict);
    let after_allows =
        after.is_some() && matches!(after_winner, None | Some(AfterCheckVerdict::Allow));
    match decision {
        CallDecision::Final { source, .. } => {
            let source_recorded = match source {
                ResultSource::Cached => call.selection == BeforeSelection::Cached,
                ResultSource::Attempt { attempt } => matches!(
                    call.attempts.get(attempt),
                    Some(
                        AttemptOutcome::Completed(..)
                            | AttemptOutcome::Failed(_)
                            | AttemptOutcome::Interrupted
                            | AttemptOutcome::TimedOut { .. }
                            | AttemptOutcome::Cancelled { .. }
                    )
                ),
                ResultSource::DeferredCompletion { attempt, .. } => {
                    matches!(
                        call.attempts.get(attempt),
                        Some(
                            AttemptOutcome::Waiting(CompletionSource::Deferred { .. })
                                | AttemptOutcome::Waiting(CompletionSource::DeferredStart { .. })
                                | AttemptOutcome::Waiting(CompletionSource::Pending { .. })
                        )
                    )
                }
            };
            source_recorded && after_allows
        }
        CallDecision::Denied => match after_winner {
            Some(AfterCheckVerdict::Deny { .. }) => true,
            _ => after.is_none() && call.selection == BeforeSelection::Deny,
        },
        CallDecision::Aborted => match after_winner {
            Some(AfterCheckVerdict::AbortRun { .. }) => true,
            _ => after.is_none() && call.selection == BeforeSelection::AbortRun,
        },
        CallDecision::CheckCancelled => match after_winner {
            Some(AfterCheckVerdict::Cancel { .. }) => true,
            _ => after.is_none() && call.selection == BeforeSelection::Cancel,
        },
        CallDecision::Cancelled => after.is_none(),
    }
}

impl crate::store::DurableRecord for SegmentOrdinal {
    const SURFACE: crate::store::SurfaceFormat =
        crate::surface_format!(crate::artifact_referrer::ARTIFACT_REFERRER_KINDS_VERSION);
}
