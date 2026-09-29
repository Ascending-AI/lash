//! Replay-keyed observations and their host projections.

use serde::{Deserialize, Serialize};

use crate::{SessionStreamEvent, TurnActivity, TurnActivityId, TurnEvent};

/// One observation a drive or step publishes, keyed by the replay key it
/// belongs to and its ordinal under that key, so a replay can suppress or
/// deduplicate it. Observation is never a decision input (ADR 0105 §1).
///
/// `(key, ordinal)` is also the observation's identity on the host stream: an
/// activity's id is derived from it, never minted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DriveObservation {
    pub key: ReplayKey,
    pub ordinal: u32,
    pub event: ObservedEvent,
}

/// What one [`DriveObservation`] carries: an event on either of the two host
/// streams a turn publishes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ObservedEvent {
    /// A low-level session stream event.
    Session(SessionStreamEvent),
    /// An application-facing turn activity. `correlation_id` groups it with
    /// related activities (a stream block, a tool call, a code cell); `None`
    /// correlates it with itself.
    Activity {
        correlation_id: Option<TurnActivityId>,
        event: TurnEvent,
    },
    /// A session event already in its emitted form: a sink publishes it
    /// verbatim, with no projected activity — the projection was emitted as
    /// an activity where the event was first observed.
    RecordedSession(SessionStreamEvent),
    /// A turn activity already carrying its recorded identity; a sink
    /// publishes it verbatim. A recorded stream replays these — their ids
    /// were minted where they were recorded and are not re-derived.
    RecordedActivity(TurnActivity),
}

/// The application-facing activity a session event projects to, if any.
/// Sinks apply it on `ObservedEvent::Session` so a low-level event and its
/// semantic twin stay tied to one observation identity.
pub fn activity_projection(event: &SessionStreamEvent) -> Option<TurnEvent> {
    match event {
        SessionStreamEvent::TokenUsage {
            protocol_iteration,
            usage,
            cumulative,
        } => Some(TurnEvent::Usage {
            protocol_iteration: *protocol_iteration,
            usage: usage.clone(),
            cumulative: cumulative.clone(),
        }),
        SessionStreamEvent::LlmRequest {
            protocol_iteration, ..
        } => Some(TurnEvent::ModelRequestStarted {
            protocol_iteration: *protocol_iteration,
        }),
        SessionStreamEvent::RetryStatus {
            wait_seconds,
            attempt,
            max_attempts,
            reason,
            ..
        } => Some(TurnEvent::RetryStatus {
            wait_seconds: *wait_seconds,
            attempt: *attempt,
            max_attempts: *max_attempts,
            reason: reason.clone(),
        }),
        SessionStreamEvent::PluginEvent { plugin_id, event } => Some(TurnEvent::PluginRuntime {
            plugin_id: plugin_id.clone(),
            event: event.clone(),
        }),
        SessionStreamEvent::InjectedMessagesCommitted {
            messages,
            checkpoint,
        } => Some(TurnEvent::QueuedMessagesCommitted {
            messages: messages.clone(),
            checkpoint: *checkpoint,
        }),
        SessionStreamEvent::Error { message, .. } => Some(TurnEvent::Error {
            message: message.clone(),
        }),
        SessionStreamEvent::StoppedPartialAvailable { summary } => {
            Some(TurnEvent::StoppedPartialAvailable {
                summary: summary.clone(),
            })
        }
        SessionStreamEvent::TurnOutcome {
            outcome: crate::TurnOutcome::Finished(crate::TurnFinish::FinalValue { value }),
        } => Some(TurnEvent::FinalValue {
            value: value.clone(),
        }),
        SessionStreamEvent::TurnOutcome {
            outcome: crate::TurnOutcome::Finished(crate::TurnFinish::ToolValue { tool_name, value }),
        } => Some(TurnEvent::ToolValue {
            tool_name: tool_name.clone(),
            value: value.clone(),
        }),
        _ => None,
    }
}

/// The durable address of one recorded operation: an effect's replay key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReplayKey(String);

impl ReplayKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ReplayKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
