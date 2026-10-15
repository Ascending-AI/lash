//! Host-readable process facts. Internal record serde retains ToolValue's trust envelope.

use super::events::{AbandonEvidence, ProcessEventKind, ProcessLifecycleFact, ProcessTerminal};
use super::model::{
    ProcessEnvRef, ProcessExternalRef, ProcessId, ProcessIdentity, ProcessInput,
    ProcessLifecycleState, ProcessOriginator, ProcessWaits, SessionId,
};
use super::observation::{ObservedProcess, ObservedProcessEvent, ProcessParkState};
use lash_sansio::CancelRequest;
use serde::{Deserialize, Serialize};

/// A process outcome for a host to store or display. Each value, failure or
/// cancellation is [`crate::ToolCallOutput::value_for_projection`]. The variant
/// distinguishes a successful JSON object from a failure with the same fields.
/// Output controls and model views belong to the internal [`ProcessTerminal`]
/// record, whose serde retains ToolValue's trust envelope for replay.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProjectedProcessTerminal {
    Completed { value: serde_json::Value },
    Failed { failure: serde_json::Value },
    Cancelled { cancellation: serde_json::Value },
    Abandoned { evidence: Box<AbandonEvidence> },
}

impl ProcessTerminal {
    /// The host-readable outcome to store or display. Serializing `self`
    /// directly retains the internal output, including controls and model views.
    pub fn projection(&self) -> ProjectedProcessTerminal {
        match self {
            Self::Settled { output } => {
                let value = output.value_for_projection();
                match &output.outcome {
                    crate::ToolCallOutcome::Success(_) => {
                        ProjectedProcessTerminal::Completed { value }
                    }
                    crate::ToolCallOutcome::Failure(_) => {
                        ProjectedProcessTerminal::Failed { failure: value }
                    }
                    crate::ToolCallOutcome::Cancelled(_) => ProjectedProcessTerminal::Cancelled {
                        cancellation: value,
                    },
                }
            }
            Self::Abandoned { evidence, .. } => ProjectedProcessTerminal::Abandoned {
                evidence: evidence.clone(),
            },
        }
    }
}

/// Host-readable lifecycle facts; terminal output uses the same projection as
/// [`ProcessTerminal::projection`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProjectedProcessLifecycleState {
    Running {},
    Waiting {
        waits: ProcessWaits,
    },
    Terminal {
        outcome: ProjectedProcessTerminal,
        occurred_at_ms: u64,
    },
}

impl ProcessLifecycleState {
    /// Host-readable lifecycle state, including the projected terminal outcome.
    pub fn projection(&self) -> ProjectedProcessLifecycleState {
        match self {
            Self::Running {} => ProjectedProcessLifecycleState::Running {},
            Self::Waiting { waits } => ProjectedProcessLifecycleState::Waiting {
                waits: waits.clone(),
            },
            Self::Terminal {
                outcome,
                occurred_at_ms,
            } => ProjectedProcessLifecycleState::Terminal {
                outcome: outcome.projection(),
                occurred_at_ms: *occurred_at_ms,
            },
        }
    }
}

/// The complete observed record for a host to store or display, with projected
/// terminal output. Its serde shape is separate from lash's internal record;
/// hosts define their own wire protocols.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectedObservedProcess {
    pub process_id: ProcessId,
    /// Sequence of the newest event folded into this observed record.
    pub last_event_sequence: u64,
    /// The one state the process is in, as its row holds it: running,
    /// waiting with everything it is blocked on (each wait names what it
    /// waits for and the node that waits, never a key that would resolve
    /// it), or terminal with its typed outcome and the time of the
    /// committed fact that ended it.
    pub lifecycle: ProjectedProcessLifecycleState,
    /// The recorded lifetime decision: what ends the process.
    pub lifetime: crate::LifetimeDecision,
    /// The recorded ancestry, nearest first; empty for a root start.
    pub ancestry: crate::Ancestry,
    pub identity: ProcessIdentity,
    pub created_at_ms: u64,
    /// When the newest folded event occurred. A fact appended after the
    /// process ended moves it; the terminal time is in `lifecycle`.
    pub updated_at_ms: u64,
    /// The first accepted process cancellation request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel_request: Option<CancelRequest>,
    pub input: ProcessInput,
    pub originator: ProcessOriginator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_ref: Option<ProcessEnvRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<crate::CausalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<ProcessExternalRef>,
    /// Whether the actor park was read, and why it is parked when it is.
    pub park: ProcessParkState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<SessionId>,
}

impl ObservedProcess {
    /// Projects this record for host storage or display. Direct record serde
    /// retains lash's ToolValue trust envelope for internal persistence.
    pub fn projection(&self) -> ProjectedObservedProcess {
        ProjectedObservedProcess {
            process_id: self.process_id.clone(),
            last_event_sequence: self.last_event_sequence,
            lifecycle: self.lifecycle.projection(),
            lifetime: self.lifetime.clone(),
            ancestry: self.ancestry.clone(),
            identity: self.identity.clone(),
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            cancel_request: self.cancel_request.clone(),
            input: self.input.clone(),
            originator: self.originator.clone(),
            env_ref: self.env_ref.clone(),
            caused_by: self.caused_by.clone(),
            external_ref: self.external_ref.clone(),
            park: self.park.clone(),
            child_session_id: self.child_session_id.clone(),
        }
    }
}

impl ProcessLifecycleFact {
    /// Host-readable event payload. [`Self::payload`] is the internal record
    /// used for storage and decoding; a host stores or displays this form.
    pub fn payload_for_projection(&self) -> serde_json::Value {
        match self {
            Self::Terminal { outcome, authority } => {
                let mut payload = serde_json::json!({ "await_output": outcome.projection() });
                if let Some(authority) = authority {
                    payload["completion_authority"] = serde_json::json!(authority);
                }
                payload
            }
            _ => self.payload(),
        }
    }
}

/// A host-readable event, with the same projected outcome as its observed row.
/// Direct [`ObservedProcessEvent`] serde remains the internal record form.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectedProcessEvent {
    pub sequence: u64,
    pub event_type: ProcessEventKind,
    pub occurred_at_ms: u64,
    pub payload: serde_json::Value,
}

impl ObservedProcessEvent {
    /// Projects this event for host storage or display, without changing the
    /// internal event payload used for persistence and replay.
    pub fn projection(&self) -> ProjectedProcessEvent {
        ProjectedProcessEvent {
            sequence: self.sequence,
            event_type: self.fact.kind(),
            occurred_at_ms: self.occurred_at_ms,
            payload: self.fact.payload_for_projection(),
        }
    }
}
