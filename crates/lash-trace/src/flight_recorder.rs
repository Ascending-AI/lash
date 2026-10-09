//! Bounded, content-free context for slow or failed operations.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use lash_sansio::llm::types::AttemptOutcome;
use lash_sansio::sync::MutexExt;
use lash_sansio::{ProcessId, SessionId, ToolCallId, TurnId};

use crate::{
    TraceDomainStatus, TraceEvent, TraceEventKind, TraceLanguageExecutionPayload,
    TraceLanguageExecutionStatus, TraceNodeFact, TraceProgramStepOutcome, TraceRecord,
    TraceRuntimeSubject, TraceSink, TraceSinkError, TraceToolCallStatus, TraceTurnOutcome,
};

/// Host policy for retention and snapshot frequency, independent of content policy.
#[derive(Clone, Debug)]
pub struct FlightRecorderSettings {
    /// Maximum retained records, including a snapshot's triggering record.
    pub capacity: NonZeroUsize,
    /// Snapshot when a reported operation duration strictly exceeds this value.
    pub slow_threshold: Duration,
    /// Minimum separation in record timestamps between accepted triggers.
    /// Older timestamps are suppressed, even when this is zero.
    pub minimum_interval: Duration,
    /// Snapshot records whose typed outcome marks a failure.
    pub snapshot_on_failure: bool,
}

#[expect(
    clippy::expect_used,
    reason = "the literal default capacity, 256, is nonzero"
)]
impl Default for FlightRecorderSettings {
    fn default() -> Self {
        Self {
            capacity: NonZeroUsize::new(256).expect("the default capacity is nonzero"),
            slow_threshold: Duration::from_secs(5),
            minimum_interval: Duration::from_secs(60),
            snapshot_on_failure: true,
        }
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
    pub timestamp: DateTime<Utc>,
    /// Reported duration, or endpoint difference for domain/attempt completions.
    /// Absent when the event has no timing evidence; no start map is retained.
    pub duration: Option<Duration>,
    pub outcome: Option<FlightRecorderOutcome>,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub graph_node_id: Option<String>,
    pub parent_graph_node_id: Option<String>,
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
    last_snapshot_at: Option<DateTime<Utc>>,
}

/// Forwards every record unchanged and retains a bounded metadata ring.
///
/// The callback runs synchronously after forwarding, outside the ring lock,
/// including when the inner sink returns an error. Hosts should hand snapshots
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
            .is_some_and(|duration| duration > self.settings.slow_threshold);
        let mut state = self.state.lock_recover();
        if state.records.len() == self.settings.capacity.get() {
            state.records.pop_front();
            state.evicted_records = state.evicted_records.saturating_add(1);
        }
        let timestamp = metadata.timestamp;
        state.records.push_back(metadata);
        let interval_elapsed = state.last_snapshot_at.is_none_or(|last| {
            timestamp
                .signed_duration_since(last)
                .to_std()
                .is_ok_and(|elapsed| elapsed >= self.settings.minimum_interval)
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
            parent_graph_node_id: record.context.parent_graph_node_id.clone(),
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
        | TraceEvent::ExecCodeCompleted { duration_ms, .. } => {
            Some(Duration::from_millis(*duration_ms))
        }
        TraceEvent::LlmCallCompleted { response, .. } => {
            Some(Duration::from_millis(response.duration_ms))
        }
        TraceEvent::PromptCompositionFailed { elapsed_ms, .. } => {
            Some(Duration::from_millis(*elapsed_ms))
        }
        TraceEvent::DomainCompleted { completion } => {
            let ended = u64::try_from(record.timestamp.timestamp_millis()).ok()?;
            ended
                .checked_sub(completion.started_at_ms)
                .map(Duration::from_millis)
        }
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
        TraceEvent::ToolCallCompleted { output, .. } => Some(match output.status() {
            TraceToolCallStatus::Success => Outcome::Completed,
            TraceToolCallStatus::Failure => Outcome::Failed,
            TraceToolCallStatus::Cancelled => Outcome::Cancelled,
        }),
        TraceEvent::DomainCompleted { completion } => Some(match completion.status {
            TraceDomainStatus::Completed => Outcome::Completed,
            TraceDomainStatus::Failed => Outcome::Failed,
            TraceDomainStatus::Cancelled => Outcome::Cancelled,
        }),
        TraceEvent::TurnCompleted { outcome } => Some(match outcome {
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
            TraceLanguageExecutionPayload::ExecutionStarted => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests;
