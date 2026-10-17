//! Bounded, content-free context for slow or failed operations.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::Duration;

use lash_protocol_types::llm::types::AttemptOutcome;
use lash_protocol_types::sync::MutexExt;
use lash_protocol_types::{ProcessId, SessionId, ToolCallId, TurnId};

use crate::{
    TraceDomainStatus, TraceEvent, TraceEventKind, TraceLanguageExecutionPayload,
    TraceLanguageExecutionStatus, TraceNodeFact, TraceProgramStepOutcome, TraceRecord,
    TraceRuntimeSubject, TraceSink, TraceSinkError, TraceTimestamp, TraceToolCallStatus,
    TraceTurnOutcome,
};

/// Host policy for retention and snapshot frequency, independent of content
/// policy.
///
/// Sealed: built through [`FlightRecorderSettings::new`], which refuses a
/// value outside its range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlightRecorderSettings {
    capacity: NonZeroUsize,
    slow_threshold: Option<Duration>,
    minimum_interval: Duration,
    snapshot_on_failure: bool,
}

impl FlightRecorderSettings {
    /// 256 records, a 5 s slow threshold, a 60 s minimum interval, and
    /// snapshots on failure.
    #[expect(
        clippy::expect_used,
        reason = "the documented default capacity, 256, is nonzero"
    )]
    pub fn standard() -> Self {
        use crate::TracePolicyField as F;
        Self {
            capacity: NonZeroUsize::new(F::RecorderCapacity.default_value() as usize)
                .expect("the default capacity is nonzero"),
            slow_threshold: Some(Duration::from_millis(
                F::RecorderSlowThreshold.default_value(),
            )),
            minimum_interval: Duration::from_millis(F::RecorderMinimumInterval.default_value()),
            snapshot_on_failure: true,
        }
    }

    /// Settings that retain at most `capacity` records, including a
    /// snapshot's triggering record (`flight_recorder.capacity`: 1..=16384);
    /// snapshot when a reported duration strictly exceeds `slow_threshold`,
    /// or never on duration when it is `None`
    /// (`flight_recorder.slow_threshold_ms`: 1 ms..=24 h); accept triggers at
    /// least `minimum_interval` apart in record time, older timestamps
    /// suppressed even at zero (`flight_recorder.minimum_interval_ms`:
    /// 0..=24 h); and snapshot records whose typed outcome is a failure when
    /// `snapshot_on_failure` is set.
    pub fn new(
        capacity: u64,
        slow_threshold: Option<Duration>,
        minimum_interval: Duration,
        snapshot_on_failure: bool,
    ) -> Result<Self, crate::TracePolicyError> {
        use crate::TracePolicyField as F;
        let millis = |duration: Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        F::RecorderCapacity.check(capacity)?;
        if let Some(threshold) = slow_threshold {
            F::RecorderSlowThreshold.check(millis(threshold))?;
        }
        F::RecorderMinimumInterval.check(millis(minimum_interval))?;
        let capacity = usize::try_from(capacity)
            .ok()
            .and_then(NonZeroUsize::new)
            .ok_or(crate::TracePolicyError::Range {
                field: F::RecorderCapacity,
                value: capacity,
                min: 1,
                max: usize::MAX as u64,
            })?;
        Ok(Self {
            capacity,
            slow_threshold,
            minimum_interval,
            snapshot_on_failure,
        })
    }

    pub fn capacity(&self) -> NonZeroUsize {
        self.capacity
    }

    pub fn slow_threshold(&self) -> Option<Duration> {
        self.slow_threshold
    }

    pub fn minimum_interval(&self) -> Duration {
        self.minimum_interval
    }

    pub fn snapshot_on_failure(&self) -> bool {
        self.snapshot_on_failure
    }
}

impl Default for FlightRecorderSettings {
    fn default() -> Self {
        Self::standard()
    }
}

/// Content-free terminal classification. Diagnostic text is never retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlightRecorderOutcome {
    Completed,
    Failed,
    Cancelled,
    Aborted,
    Interrupted,
    AgentFrameSwitch,
}

/// An allowlisted projection of a record. No event payload or arbitrary context
/// metadata is stored, even when the incoming record captures full content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlightRecorderRecord {
    pub record_id: String,
    pub kind: TraceEventKind,
    pub timestamp: TraceTimestamp,
    /// Reported duration, or endpoint difference for domain/attempt completions.
    /// Absent when the event has no timing evidence; no start map is retained.
    pub duration: Option<Duration>,
    pub outcome: Option<FlightRecorderOutcome>,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub graph_node_id: Option<String>,
    pub effect_id: Option<String>,
    pub llm_call_id: Option<String>,
    pub tool_call_id: Option<ToolCallId>,
    pub process_id: Option<ProcessId>,
    pub engine_execution_id: Option<String>,
    pub attempt: Option<u32>,
}

/// Recent metadata in append order, ending with the triggering record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlightRecorderSnapshot {
    pub records: Vec<FlightRecorderRecord>,
    /// Evictions since the previous snapshot (or construction for the first).
    /// Suppressed triggers do not reset this saturating counter.
    pub evicted_records: u64,
    /// Both flags can be true for the same trigger, which produces one snapshot.
    pub failed: bool,
    pub slow: bool,
}

#[derive(Default)]
struct RecorderState {
    records: VecDeque<FlightRecorderRecord>,
    evicted_records: u64,
    last_snapshot_at: Option<TraceTimestamp>,
}

/// Forwards every record unchanged and retains a bounded metadata ring.
///
/// The callback runs synchronously after forwarding, outside the ring lock,
/// including when the inner sink returns an error. A callback that panics
/// unwinds out of [`append`](TraceSink::append): the runtime's emitter
/// contains it as this sink's append failure, and no other sink sees it. Hosts should hand snapshots
/// to a bounded queue if exporting them is expensive. Concurrent appends are
/// ordered by acquisition of the ring lock; callbacks may run concurrently.
/// The decorator reads no clock: interval decisions use record timestamps.
pub struct FlightRecorderSink<S: TraceSink> {
    inner: S,
    settings: FlightRecorderSettings,
    state: Mutex<RecorderState>,
    on_snapshot: Box<dyn Fn(FlightRecorderSnapshot) + Send + Sync>,
}

impl<S: TraceSink> FlightRecorderSink<S> {
    pub fn new(
        inner: S,
        settings: FlightRecorderSettings,
        on_snapshot: impl Fn(FlightRecorderSnapshot) + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner,
            settings,
            state: Mutex::new(RecorderState::default()),
            on_snapshot: Box::new(on_snapshot),
        }
    }

    fn observe(&self, metadata: FlightRecorderRecord) -> Option<FlightRecorderSnapshot> {
        let failed = self.settings.snapshot_on_failure
            && metadata.outcome == Some(FlightRecorderOutcome::Failed);
        let slow = metadata
            .duration
            .zip(self.settings.slow_threshold)
            .is_some_and(|(duration, threshold)| duration > threshold);
        let mut state = self.state.lock_recover();
        if state.records.len() == self.settings.capacity.get() {
            state.records.pop_front();
            state.evicted_records = state.evicted_records.saturating_add(1);
        }
        let timestamp = metadata.timestamp;
        state.records.push_back(metadata);
        let interval_elapsed = state.last_snapshot_at.is_none_or(|last| {
            timestamp >= last && timestamp.duration_since(last) >= self.settings.minimum_interval
        });
        if !(failed || slow) || !interval_elapsed {
            return None;
        }
        state.last_snapshot_at = Some(timestamp);
        let snapshot = FlightRecorderSnapshot {
            records: state.records.iter().cloned().collect(),
            evicted_records: state.evicted_records,
            failed,
            slow,
        };
        state.evicted_records = 0;
        Some(snapshot)
    }
}

impl<S: TraceSink> TraceSink for FlightRecorderSink<S> {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        let result = self.inner.append(record);
        if let Some(snapshot) = self.observe(FlightRecorderRecord::of(record)) {
            (self.on_snapshot)(snapshot);
        }
        result
    }

    fn flush(&self) -> Result<(), TraceSinkError> {
        self.inner.flush()
    }
}

impl FlightRecorderRecord {
    fn of(record: &TraceRecord) -> Self {
        let mut metadata = Self {
            record_id: record.id.clone(),
            kind: record.event.kind(),
            timestamp: record.timestamp,
            duration: operation_duration(record),
            outcome: operation_outcome(&record.event),
            session_id: record.context.session_id.clone(),
            turn_id: record.context.turn_id.clone(),
            graph_node_id: record.context.graph_node_id.clone(),
            effect_id: record.context.effect_id.clone(),
            llm_call_id: record.context.llm_call_id.clone(),
            tool_call_id: None,
            process_id: None,
            engine_execution_id: None,
            attempt: None,
        };
        match &record.event {
            TraceEvent::ToolCallStarted { call_id, .. }
            | TraceEvent::ToolCallCompleted { call_id, .. } => {
                metadata.tool_call_id = Some(call_id.clone());
            }
            TraceEvent::StepBodyStarted { step } => {
                metadata.tool_call_id = Some(step.call_id.clone());
                metadata.process_id = Some(step.process_id.clone());
                metadata.attempt = Some(step.attempt);
            }
            TraceEvent::LanguageExecution { event, .. } => {
                match &event.identity.subject {
                    TraceRuntimeSubject::Process { process_id } => {
                        metadata.process_id = Some(process_id.clone());
                    }
                    TraceRuntimeSubject::Effect { effect_id, .. } => {
                        metadata.effect_id = Some(effect_id.clone());
                    }
                }
                metadata.engine_execution_id = event.identity.engine_execution_id.clone();
                metadata.attempt = event.identity.attempt();
                if let TraceLanguageExecutionPayload::Node {
                    fact:
                        TraceNodeFact::Started { call_id }
                        | TraceNodeFact::Completed { call_id }
                        | TraceNodeFact::Failed { call_id, .. },
                    ..
                } = &event.payload
                {
                    metadata.tool_call_id = call_id.clone();
                }
            }
            TraceEvent::LlmAttemptCompleted { attempt, .. } => {
                metadata.attempt = Some(attempt.ordinal);
            }
            _ => {}
        }
        metadata
    }
}

fn operation_duration(record: &TraceRecord) -> Option<Duration> {
    match &record.event {
        TraceEvent::ToolCallCompleted { duration_ms, .. }
        | TraceEvent::ExecCodeCompleted { duration_ms, .. }
        | TraceEvent::ExecCodeDiscarded { duration_ms, .. } => {
            Some(Duration::from_millis(*duration_ms))
        }
        TraceEvent::ExecCodeFailed { duration_ms, .. } => duration_ms.map(Duration::from_millis),
        TraceEvent::LlmCallCompleted { response, .. } => {
            Some(Duration::from_millis(response.duration_ms))
        }
        TraceEvent::PromptCompositionFailed { elapsed_ms, .. } => {
            Some(Duration::from_millis(*elapsed_ms))
        }
        TraceEvent::DomainCompleted { completion } => record
            .timestamp
            .unix_ms()
            .checked_sub(completion.started_at_ms)
            .map(Duration::from_millis),
        TraceEvent::LlmAttemptCompleted { observation, .. } => observation
            .ended_at_ms?
            .checked_sub(observation.started_at_ms?)
            .map(Duration::from_millis),
        _ => None,
    }
}

fn operation_outcome(event: &TraceEvent) -> Option<FlightRecorderOutcome> {
    use FlightRecorderOutcome as Outcome;
    if event.is_failed() {
        return Some(Outcome::Failed);
    }
    match event {
        TraceEvent::LlmCallCompleted { .. } | TraceEvent::ExecCodeCompleted { .. } => {
            Some(Outcome::Completed)
        }
        TraceEvent::ProgramStep {
            outcome: TraceProgramStepOutcome::Ok,
            ..
        } => Some(Outcome::Completed),
        TraceEvent::ExecCodeDiscarded { .. } => Some(Outcome::Cancelled),
        TraceEvent::ToolCallCompleted { output, .. } => Some(match output.status() {
            TraceToolCallStatus::Success => Outcome::Completed,
            TraceToolCallStatus::Failure => Outcome::Failed,
            TraceToolCallStatus::Cancelled | TraceToolCallStatus::Discarded => Outcome::Cancelled,
        }),
        TraceEvent::DomainCompleted { completion } => Some(match completion.status {
            TraceDomainStatus::Completed => Outcome::Completed,
            TraceDomainStatus::Failed => Outcome::Failed,
            TraceDomainStatus::Cancelled => Outcome::Cancelled,
        }),
        TraceEvent::TurnCompleted { outcome, .. } => Some(match outcome {
            TraceTurnOutcome::Completed { .. } => Outcome::Completed,
            TraceTurnOutcome::Failed { .. } => Outcome::Failed,
            TraceTurnOutcome::Cancelled { .. } => Outcome::Cancelled,
            TraceTurnOutcome::AgentFrameSwitch { .. } => Outcome::AgentFrameSwitch,
        }),
        TraceEvent::LlmAttemptCompleted { attempt, .. } => Some(match attempt.outcome {
            AttemptOutcome::Completed => Outcome::Completed,
            AttemptOutcome::Failed => Outcome::Failed,
            AttemptOutcome::Aborted => Outcome::Aborted,
            AttemptOutcome::Interrupted => Outcome::Interrupted,
        }),
        TraceEvent::LanguageExecution { event, .. } => match &event.payload {
            TraceLanguageExecutionPayload::ExecutionFinished { status, .. } => match status {
                TraceLanguageExecutionStatus::Completed => Some(Outcome::Completed),
                TraceLanguageExecutionStatus::Failed => Some(Outcome::Failed),
                TraceLanguageExecutionStatus::Cancelled => Some(Outcome::Cancelled),
                TraceLanguageExecutionStatus::Running => None,
            },
            TraceLanguageExecutionPayload::Node { fact, .. } => match fact {
                TraceNodeFact::Completed { .. } => Some(Outcome::Completed),
                TraceNodeFact::Failed { .. } => Some(Outcome::Failed),
                TraceNodeFact::Cancelled => Some(Outcome::Cancelled),
                _ => None,
            },
            TraceLanguageExecutionPayload::ExecutionStarted { .. }
            | TraceLanguageExecutionPayload::Ran { .. } => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests;
