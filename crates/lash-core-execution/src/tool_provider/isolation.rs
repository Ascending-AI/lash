//! How an isolated tool's call becomes the process that runs it.
//!
//! An isolated call has no inline body. Its manifest names the registered
//! [`ProcessEngine`](crate::ProcessEngine) that runs it
//! ([`ToolManifest::isolation_engine`](crate::ToolManifest::isolation_engine)),
//! and the tool enters a catalog only where that engine is registered, so a
//! call is never refused for it. Admission asks the call's provider for the
//! engine's start payload and the environment and engine settings the
//! process runs under (D6), holds that environment under the call's
//! execution claim, records the answer with the call's admission, and starts
//! that one process under a lash-derived start key before any ordinary body
//! could run. Replay and recovery read the recorded binding and never ask
//! the provider again. Cancellation of that process is cooperative: a
//! host needing hard isolation builds it into its own engine.

use crate::ToolId;
use lash_protocol_types::ToolCallId;

/// An isolated call, as admission asks its provider for its start.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct IsolatedProcessRequest<'a> {
    pub tool_id: &'a ToolId,
    /// The call's lash-minted id: the same on every redelivery.
    pub call_id: &'a ToolCallId,
    /// The call's arguments as issued, before any transform or preparation.
    pub args: &'a serde_json::Value,
    /// The process the call runs inside, with its recorded environment and
    /// settings; `None` outside a process.
    pub enclosing: Option<&'a crate::EnclosingProcess>,
    /// The calling turn's recorded run; `None` inside a process.
    pub run: Option<&'a crate::ResolvedRun>,
}

/// What an isolated call's process runs: its engine's start payload, the
/// environment it runs under and the engine settings, all stated by the
/// provider (D6). Nothing is inferred from the calling session.
#[derive(Clone, Debug, PartialEq)]
pub struct IsolatedStart {
    pub payload: serde_json::Value,
    pub env: crate::ProcessEnvChoice,
    pub engine_settings: Option<crate::EngineSettings>,
}
