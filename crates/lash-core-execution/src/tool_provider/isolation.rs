//! How a provider binds an isolated tool to the process that runs it (D04).
//!
//! An isolated call has no inline body: admission asks the call's provider
//! which registered [`ProcessEngine`](crate::ProcessEngine) runs it, records
//! that answer with the call's admission, and starts that one process under a
//! lash-derived start key before any ordinary body could run. Replay and
//! recovery read the recorded binding and never ask the provider again.

use crate::{ProcessExecutionBoundary, ToolId};
use lash_sansio::ToolCallId;

/// An isolated call, as admission asks its provider to bind it.
#[derive(Clone, Copy, Debug)]
pub struct IsolatedProcessRequest<'a> {
    pub tool_id: &'a ToolId,
    /// The call's lash-minted id: the same on every redelivery.
    pub call_id: &'a ToolCallId,
    /// The call's arguments as issued, before any transform or preparation.
    pub args: &'a serde_json::Value,
}

/// The registered process engine an isolated call runs in, and its start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IsolatedProcessBinding {
    /// [`ProcessEngine::kind`](crate::ProcessEngine::kind) of an engine
    /// registered with the host.
    pub engine: String,
    /// The engine's start payload.
    pub payload: serde_json::Value,
    /// The boundary the call promises.
    /// [`ProcessExecutionBoundary::WorkerProcess`] requires the engine's
    /// [`PhysicalProcessWorker`](crate::PhysicalProcessWorker); admission
    /// refuses the call otherwise.
    pub boundary: ProcessExecutionBoundary,
}
