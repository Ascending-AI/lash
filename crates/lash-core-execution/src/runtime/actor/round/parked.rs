//! Snapshot discovery of admitted calls through their pending completion waits.

use lash_durable::domain::WaitPurpose;
use lash_durable::{ActorKey, DurableError, DurableInstant, StoreFailure, StoreFailureKind};

use crate::runtime::actor::waits::PinnedKey;
use crate::{Backend, ProcessId, SessionId, ToolCallId, ToolId};

/// The actor whose admitted calls a host may discover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallOwner {
    Session(SessionId),
    Process(ProcessId),
}

/// A pending admitted call. Its key is a bearer capability; the host authorizes discovery.
#[derive(Clone, PartialEq, Eq)]
pub struct ParkedCall {
    pub key: PinnedKey,
    pub owner: CallOwner,
    pub call_id: ToolCallId,
    pub tool_id: ToolId,
    /// The deadline recorded when the completion wait was admitted.
    pub deadline: Option<DurableInstant>,
}

/// List the owner's pending completion waits without running an actor. Each
/// wait row names its call and tool, so the read costs what is parked, never
/// the owner's retained runs.
///
/// # Errors
/// A failed store read.
pub async fn parked(backend: &Backend, owner: CallOwner) -> Result<Vec<ParkedCall>, DurableError> {
    let actor = match &owner {
        CallOwner::Session(id) => ActorKey::session(id.as_str()),
        CallOwner::Process(id) => ActorKey::process(id.as_str()),
    }
    .map_err(|error| corrupt(error.to_string()))?;
    Ok(backend
        .durable()
        .pending_waits(&actor)
        .await?
        .into_iter()
        .filter_map(|wait| match wait.purpose {
            WaitPurpose::ToolCompletion {
                call,
                tool,
                deadline,
            } => Some(ParkedCall {
                key: PinnedKey::of(&wait.id),
                owner: owner.clone(),
                call_id: call,
                tool_id: tool,
                deadline,
            }),
            _ => None,
        })
        .collect())
}

fn corrupt(message: String) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Corrupt,
        message,
    })
}
