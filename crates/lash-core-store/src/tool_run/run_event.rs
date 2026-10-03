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

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU32;

use lash_sansio::ToolCallId;
use serde::{Deserialize, Serialize};

use super::admission::{RecordedRetryPolicy, RoundAdmission};
pub use super::aggregate::{AggregateConsumer, AggregateLeaf, AggregatePlan};
use super::material::{MaterialEntry, MaterialRef};
use super::tool_hooks::{AfterCheckVerdict, BeforeSelection, CheckRecord};
use crate::ProcessId;
use crate::await_event_identity::AwaitEventKey;
use crate::effect_opener::EffectOpener;
use crate::process_identity::StartKey;

/// The ordinal of an attempt of one logical call, from 1. A crash
/// redelivery keeps it; only a reported retry advances it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

/// What one attempt's body returned (X).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum AttemptResult {
    /// A completed result.
    Done { output: MaterialRef },
    /// Parked on a Deferred source; the source's seal supplies the result.
    Deferred { source: AwaitEventKey },
    /// A failure the body reported. Only a `retryable` one may be retried,
    /// and only under the call's recorded retry policy.
    Failed {
        output: MaterialRef,
        retryable: bool,
    },
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
    /// A check, or the Run's cancellation, cancelled the call.
    Cancelled,
    /// A check returned AbortRun: the call fails and the Run stops.
    Aborted,
}

/// The lifecycle of a logical Run, at its owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
        result: AttemptResult,
    },
    /// Eligibility and backoff are fixed before registering a durable timer.
    RetryTimerRegistered {
        call_id: ToolCallId,
        failed: AttemptOrdinal,
        next: AttemptOrdinal,
        backoff_ms: u64,
    },
    /// K9: a reported retryable failure, its backoff and the registration
    /// of the next attempt, as one schedule entry.
    RetryScheduled {
        call_id: ToolCallId,
        failed: AttemptOrdinal,
        next: AttemptOrdinal,
        backoff_ms: u64,
    },
    /// D: the call's one decision and its rank, with the after-check
    /// record when a result candidate existed.
    Decided {
        call_id: ToolCallId,
        rank: u64,
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
    /// The start's process is registered under its key.
    StartLaunched {
        call_id: ToolCallId,
        start_key: StartKey,
        process_id: ProcessId,
    },
    /// The start's recorded cancel policy is followed and its consumer hold
    /// released. `cancelled` when a cancellation of the Run made that policy
    /// cancel the process.
    StartDischarged {
        call_id: ToolCallId,
        start_key: StartKey,
        cancelled: bool,
    },
    /// V: the call's model-facing presentation.
    Presented {
        call_id: ToolCallId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        presentation: Option<MaterialRef>,
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
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunAttemptEntry {
    pub call_id: ToolCallId,
    pub attempt: AttemptOrdinal,
    pub result: AttemptResult,
    pub materials: Vec<MaterialEntry>,
}

/// Why the fold refused a record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
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
    #[error("rank {rank} is not above the last rank {last}")]
    RankOrder { rank: u64, last: u64 },
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

#[derive(Clone, Debug)]
struct CallState {
    cancel: super::ExternalCancelPolicy,
    cancel_discharged: bool,
    selection: BeforeSelection,
    retry: RecordedRetryPolicy,
    /// The attempt issued and not yet recorded.
    outstanding: Option<AttemptOrdinal>,
    attempts: BTreeMap<AttemptOrdinal, AttemptResult>,
    retry_timer: Option<(AttemptOrdinal, AttemptOrdinal, u64)>,
    decision: Option<(u64, CallDecision)>,
    declarations_issued: bool,
    /// The declared start, admitted with the declarations.
    start: Option<(StartKey, StartProgress)>,
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
    last_rank: Option<u64>,
    calls: BTreeMap<ToolCallId, CallState>,
    aggregates: BTreeMap<String, AggregatePlan>,
    elapsed: std::collections::BTreeSet<(String, u32)>,
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
            last_rank: None,
            calls: BTreeMap::new(),
            aggregates: BTreeMap::new(),
            elapsed: std::collections::BTreeSet::new(),
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

    /// The rank the Run's next decision takes: one above the last, from 1.
    #[must_use]
    pub fn next_rank(&self) -> u64 {
        self.last_rank.map_or(1, |last| last + 1)
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
            RunEvent::Decided {
                call_id,
                rank,
                decision,
                after,
            } => self.decide(call_id, *rank, decision, after.as_ref()),
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
                call.seated = true;
                Ok(())
            }
            RunEvent::StartAdmitted { call_id, start_key } => self.admit_start(call_id, start_key),
            RunEvent::StartLaunched {
                call_id, start_key, ..
            } => self.advance_start(
                call_id,
                start_key,
                &StartProgress::Admitted,
                StartProgress::Launched,
            ),
            RunEvent::StartDischarged {
                call_id, start_key, ..
            } => self.advance_start(
                call_id,
                start_key,
                &StartProgress::Launched,
                StartProgress::Discharged,
            ),
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
                    retry: member.policy.retry.clone(),
                    outstanding: (selection == BeforeSelection::Execute)
                        .then_some(AttemptOrdinal::FIRST),
                    attempts: BTreeMap::new(),
                    retry_timer: None,
                    decision: None,
                    declarations_issued: false,
                    start: None,
                    seated: false,
                    presented: false,
                    consumed: false,
                    incorporated: false,
                },
            );
        }
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
        let within_policy = match &call.retry {
            RecordedRetryPolicy::Never => false,
            RecordedRetryPolicy::Reported { max_attempts, .. } => next.get() <= max_attempts.get(),
        };
        let eligible = !stopped
            && within_policy
            && call.decision.is_none()
            && call.outstanding.is_none()
            && failed.next() == Some(next)
            && call.attempts.keys().next_back() == Some(&failed)
            && matches!(
                call.attempts.get(&failed),
                Some(AttemptResult::Failed {
                    retryable: true,
                    ..
                })
            );
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
        rank: u64,
        decision: &CallDecision,
        after: Option<&CheckRecord<AfterCheckVerdict>>,
    ) -> Result<(), RunEventRefusal> {
        if let Some(last) = self.last_rank
            && rank <= last
        {
            return Err(RunEventRefusal::RankOrder { rank, last });
        }
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
        self.last_rank = Some(rank);
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
        if !declaring || !call.declarations_issued || call.seated || call.start.is_some() {
            return Err(RunEventRefusal::StartOrder {
                call_id: call_id.clone(),
                start_key: start_key.clone(),
            });
        }
        call.start = Some((start_key.clone(), StartProgress::Admitted));
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
                if protected_owed || cancel_owed || call.outstanding.is_some() {
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
                    Some(AttemptResult::Done { .. } | AttemptResult::Failed { .. })
                ),
                ResultSource::DeferredCompletion { attempt, .. } => {
                    matches!(
                        call.attempts.get(attempt),
                        Some(AttemptResult::Deferred { .. })
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
        // A check's Cancel, or the Run's own cancellation of an undecided
        // call.
        CallDecision::Cancelled => {
            matches!(after_winner, Some(AfterCheckVerdict::Cancel { .. })) || after.is_none()
        }
    }
}
