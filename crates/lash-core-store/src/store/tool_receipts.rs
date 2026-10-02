//! The first request and completed result of a session-owned tool call.
use super::{SessionId, StoreError};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolRequestReceipt {
    pub session_id: SessionId,
    pub request_key: String,
    pub payload_digest: String,
    pub payload: serde_json::Value,
    pub scope: Option<lash_trace::DurableTraceScope>,
    pub context: lash_trace::TraceContext,
    pub requested_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCompletionReceipt {
    pub session_id: SessionId,
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
    if existing.session_id == offered.session_id
        && existing.request_key == offered.request_key
        && existing.payload_digest == offered.payload_digest
    {
        Ok(())
    } else {
        Err(StoreError::ToolRequestConflict {
            session_id: offered.session_id.clone(),
            request_key: offered.request_key.clone(),
        })
    }
}
