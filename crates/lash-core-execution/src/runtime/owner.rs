//! Who a dispatch runs for.

use crate::{FrameNodeId, PluginError, ProcessId, RuntimeOwner, SessionId};

/// Who a dispatch runs for: a session on the agent frame its execution was
/// admitted on, or a process. A process has no frame, and no session of its
/// own: session-only operations refuse it with
/// [`PluginError::NotASessionRuntime`] rather than borrow its originator's.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionOwner {
    SessionFrame {
        session_id: SessionId,
        agent_frame_id: FrameNodeId,
    },
    Process {
        process_id: ProcessId,
    },
}

impl ExecutionOwner {
    pub fn runtime_owner(&self) -> RuntimeOwner {
        match self {
            Self::SessionFrame { session_id, .. } => RuntimeOwner::Session(session_id.clone()),
            Self::Process { process_id } => RuntimeOwner::Process(process_id.clone()),
        }
    }

    /// The session this dispatch runs in, or
    /// [`PluginError::NotASessionRuntime`] naming `operation`.
    pub fn require_session(&self, operation: &'static str) -> Result<&SessionId, PluginError> {
        match self {
            Self::SessionFrame { session_id, .. } => Ok(session_id),
            Self::Process { process_id } => Err(not_a_session_runtime(operation, process_id)),
        }
    }

    /// The agent frame this dispatch was admitted on, or
    /// [`PluginError::NotASessionRuntime`] naming `operation`.
    pub fn require_agent_frame(
        &self,
        operation: &'static str,
    ) -> Result<&FrameNodeId, PluginError> {
        match self {
            Self::SessionFrame { agent_frame_id, .. } => Ok(agent_frame_id),
            Self::Process { process_id } => Err(not_a_session_runtime(operation, process_id)),
        }
    }

    /// The session this dispatch runs in, when it runs in one.
    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            Self::SessionFrame { session_id, .. } => Some(session_id),
            Self::Process { .. } => None,
        }
    }

    /// The agent frame this dispatch was admitted on, when it runs in a
    /// session.
    pub fn agent_frame_id(&self) -> Option<&FrameNodeId> {
        match self {
            Self::SessionFrame { agent_frame_id, .. } => Some(agent_frame_id),
            Self::Process { .. } => None,
        }
    }

    /// The process this dispatch runs for, when a process owns it.
    pub fn process_id(&self) -> Option<&ProcessId> {
        match self {
            Self::SessionFrame { .. } => None,
            Self::Process { process_id } => Some(process_id),
        }
    }
}

pub(crate) fn not_a_session_runtime(
    operation: &'static str,
    process_id: &ProcessId,
) -> PluginError {
    PluginError::NotASessionRuntime {
        operation: operation.to_string(),
        process_id: process_id.clone(),
    }
}
