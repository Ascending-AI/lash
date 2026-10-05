//! The first request and completed result of a logical tool call.
use super::{SessionId, StoreError};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolRequestReceipt {
    pub owner: lash_trace::TraceToolOwner,
    pub request_key: String,
    pub payload_digest: String,
    pub payload: serde_json::Value,
    pub scope: Option<lash_trace::DurableTraceScope>,
    pub context: lash_trace::TraceContext,
    pub requested_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCompletionReceipt {
    pub owner: lash_trace::TraceToolOwner,
    pub request_key: String,
    pub payload_digest: String,
    pub result: serde_json::Value,
    pub intent_outcomes: serde_json::Value,
    pub completed_at_ms: u64,
}

pub fn require_tool_request_matches(
    existing: &ToolRequestReceipt,
    offered: &ToolRequestReceipt,
) -> Result<(), StoreError> {
    if existing.owner == offered.owner
        && existing.request_key == offered.request_key
        && existing.payload_digest == offered.payload_digest
    {
        Ok(())
    } else {
        Err(StoreError::ToolRequestConflict {
            owner: offered.owner.clone(),
            request_key: offered.request_key.clone(),
        })
    }
}

impl ToolRequestReceipt {
    pub fn session_id(&self) -> Option<SessionId> {
        owner_session(&self.owner)
    }
    pub fn owner_key(&self) -> Result<String, StoreError> {
        let scope = match &self.owner {
            lash_trace::TraceToolOwner::Turn {
                session_id,
                turn_id,
            } => crate::ExecutionScope::turn(session_id, turn_id),
            lash_trace::TraceToolOwner::Process { process_id } => {
                crate::ExecutionScope::process(process_id)
            }
            lash_trace::TraceToolOwner::Operation {
                session_id,
                operation_id,
            } => crate::ExecutionScope::session_operation(session_id, operation_id.clone()),
        };
        serde_json::to_string(&scope).map_err(|error| StoreError::Backend(error.to_string()))
    }
}

impl ToolCompletionReceipt {
    pub fn session_id(&self) -> Option<SessionId> {
        owner_session(&self.owner)
    }
}

fn owner_session(owner: &lash_trace::TraceToolOwner) -> Option<SessionId> {
    match owner {
        lash_trace::TraceToolOwner::Turn { session_id, .. }
        | lash_trace::TraceToolOwner::Operation { session_id, .. } => Some(session_id.clone()),
        lash_trace::TraceToolOwner::Process { .. } => None,
    }
}
