//! Snapshot discovery of admitted calls through their pending completion waits.

use std::collections::BTreeMap;

use lash_durable::domain::RunRecordKind;
use lash_durable::{ActorKey, DurableError, DurableInstant, StoreFailure, StoreFailureKind};

use super::records::AdmitBody;
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

/// Join the owner's pending waits with admission records without running an actor.
///
/// # Errors
/// A failed store read or an undecodable admission.
pub async fn parked(backend: &Backend, owner: CallOwner) -> Result<Vec<ParkedCall>, DurableError> {
    let actor = match &owner {
        CallOwner::Session(id) => ActorKey::session(id.as_str()),
        CallOwner::Process(id) => ActorKey::process(id.as_str()),
    }
    .map_err(|error| corrupt(error.to_string()))?;
    let reads = backend.durable();
    let pending: BTreeMap<_, _> = reads
        .pending_waits(&actor)
        .await?
        .into_iter()
        .filter(|wait| wait.purpose.kind() == lash_durable::domain::WaitKind::ToolCompletion)
        .map(|wait| (wait.id, wait))
        .collect();
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    let owners = reads.run_record_owners(&actor).await?;
    let mut calls = BTreeMap::new();
    for run_owner in owners {
        for row in reads.run_records(&run_owner).await? {
            if row.kind != RunRecordKind::Admit {
                continue;
            }
            let admission: AdmitBody = serde_json::from_str(&row.record_json)
                .map_err(|error| corrupt(error.to_string()))?;
            for member in admission.members {
                let draft = member.draft().map_err(|error| corrupt(error.to_owned()))?;
                if let Some(pinned) = draft.pinned_wait()
                    && let Some(wait) = pending.get(&pinned.id)
                {
                    calls.insert(
                        pinned.id,
                        ParkedCall {
                            key: PinnedKey::new(pinned.id.to_hex()),
                            owner: owner.clone(),
                            call_id: draft.call().clone(),
                            tool_id: draft.tool().clone(),
                            deadline: wait.purpose.deadline(),
                        },
                    );
                }
            }
        }
    }
    Ok(calls.into_values().collect())
}

fn corrupt(message: String) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Corrupt,
        message,
    })
}
