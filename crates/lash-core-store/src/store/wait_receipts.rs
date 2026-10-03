//! Durable requests and resolutions of engine-owned waits.
use super::{SessionId, StoreError, StoreTransition};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineWaitKind {
    Event,
    Timer,
    Process,
    ToolCompletion,
}
impl EngineWaitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Event => "await_event",
            Self::Timer => "sleep",
            Self::Process => "process_await",
            Self::ToolCompletion => "tool_completion",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WaitRequestReceipt {
    pub wait_id: String,
    pub owner_key: String,
    pub session_id: Option<SessionId>,
    pub request_digest: String,
    pub kind: EngineWaitKind,
    pub scope: Option<lash_trace::DurableTraceScope>,
    pub context: lash_trace::TraceContext,
    pub started_at_ms: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WaitResolutionReceipt {
    pub wait_id: String,
    pub resolution_digest: String,
    pub resolution: serde_json::Value,
    pub resolved_at_ms: u64,
}
#[async_trait::async_trait]
pub trait WaitReceiptStore: Send + Sync {
    /// Retain the first request; return its disposition after commit.
    async fn record_wait_request(
        &self,
        request: &WaitRequestReceipt,
    ) -> Result<StoreTransition<WaitRequestReceipt>, StoreError>;
    /// Retain the first resolution; a different digest is refused.
    async fn record_wait_resolution(
        &self,
        resolution: &WaitResolutionReceipt,
    ) -> Result<StoreTransition<WaitResolutionReceipt>, StoreError>;
    /// Mark wait and tool receipts eligible for retention only after their owner has retired.
    async fn retire_observation_receipts(
        &self,
        owner_key: &str,
        retired_at_ms: u64,
    ) -> Result<(), StoreError>;
}
pub fn require_wait_request_matches(
    existing: &WaitRequestReceipt,
    offered: &WaitRequestReceipt,
) -> Result<(), StoreError> {
    if existing.wait_id == offered.wait_id
        && existing.owner_key == offered.owner_key
        && existing.session_id == offered.session_id
        && existing.request_digest == offered.request_digest
        && existing.kind == offered.kind
    {
        Ok(())
    } else {
        Err(StoreError::WaitReceiptConflict {
            wait_id: offered.wait_id.clone(),
        })
    }
}
pub fn require_wait_resolution_matches(
    existing: &WaitResolutionReceipt,
    offered: &WaitResolutionReceipt,
) -> Result<(), StoreError> {
    if existing.wait_id == offered.wait_id
        && existing.resolution_digest == offered.resolution_digest
    {
        Ok(())
    } else {
        Err(StoreError::WaitReceiptConflict {
            wait_id: offered.wait_id.clone(),
        })
    }
}
