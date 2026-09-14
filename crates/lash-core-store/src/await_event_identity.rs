//! Durable await-event wait identity.

use crate::{ExecutionScope, ProcessId, RuntimeError};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AwaitEventWaitIdentity {
    ToolCompletion {
        tool_call_id: String,
    },
    ProcessSignal {
        process_id: ProcessId,
        signal_name: String,
        ordinal: u64,
    },
    /// Reserved first-writer-wins cancellation-versus-completion gate for a
    /// foreground turn.
    TurnCancelGate,
    /// Reserved terminal publication promise for a foreground turn.
    TurnTerminal,
    Custom {
        key: String,
    },
    /// Reserved first-writer-wins escalation promise for a foreground turn:
    /// written only by an immediate request that found the cancellation gate
    /// already holding an after-step request.
    TurnCancelEscalation,
}
impl AwaitEventWaitIdentity {
    /// Constructs the stable wait identity effect-host implementors use to resolve a deferred tool
    /// call by its call ID.
    pub fn tool_completion(tool_call_id: impl Into<String>) -> Self {
        Self::ToolCompletion {
            tool_call_id: tool_call_id.into(),
        }
    }

    /// Constructs the stable wait identity effect-host implementors use to resolve one named
    /// process signal without colliding with other signals or attempts.
    pub fn process_signal(
        process_id: impl Into<ProcessId>,
        signal_name: impl Into<String>,
        ordinal: u64,
    ) -> Self {
        Self::ProcessSignal {
            process_id: process_id.into(),
            signal_name: signal_name.into(),
            ordinal,
        }
    }

    pub fn validate(&self) -> Result<(), RuntimeError> {
        let invalid = match self {
            Self::ToolCompletion { tool_call_id } => tool_call_id.trim().is_empty(),
            Self::ProcessSignal {
                process_id,
                signal_name,
                ordinal,
            } => process_id.trim().is_empty() || signal_name.trim().is_empty() || *ordinal == 0,
            Self::TurnCancelGate | Self::TurnTerminal | Self::TurnCancelEscalation => false,
            Self::Custom { key } => key.trim().is_empty(),
        };
        if invalid {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::InvalidAwaitEventWaitIdentity,
                "await-event wait identity requires non-empty stable ids",
            ));
        }
        Ok(())
    }

    /// Lets effect-host implementors distinguish the reserved turn-control wait from ordinary tool
    /// and application waits.
    pub fn is_turn_control(&self) -> bool {
        matches!(
            self,
            Self::TurnCancelGate | Self::TurnTerminal | Self::TurnCancelEscalation
        )
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AwaitEventKey {
    pub scope: ExecutionScope,
    pub wait: AwaitEventWaitIdentity,
    pub key_id: String,
    pub signature: String,
}
impl AwaitEventKey {
    /// Derives the deterministic promise key effect-host implementors use to rendezvous durable
    /// wait resolution with its execution scope and wait identity.
    pub fn promise_key(&self) -> String {
        format!("lash-await-event:{}", self.key_id)
    }
}
