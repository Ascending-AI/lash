//! Admitted calls through their completion waits: the snapshot discovery of
//! the pending ones, and the rows a fold reads each admitted park from.

use std::collections::BTreeMap;

use lash_durable::domain::{OwnerKey, RunRecordKind, RunRecordRow, WaitPurpose, WaitRow};
use lash_durable::{
    ActorKey, DurableError, DurableInstant, DurableReads, StoreFailure, StoreFailureKind,
};

use super::PinnedWait;
use super::records::AdmitBody;
use crate::runtime::actor::waits::{ParkDeadline, PinnedKey, WaitDeadline, WaitId};
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

/// The completion waits an owner's admissions pinned, as their rows hold
/// them. A wait's purpose is written once, with its row, so these rows are
/// the one place an admitted park's deadline is recorded: a fold reads it
/// from here and refuses an admission whose wait is missing or is not its
/// call's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PinnedWaits {
    rows: BTreeMap<WaitId, WaitRow>,
}

impl PinnedWaits {
    /// The waits the admissions among `rows` pinned, read once each. An
    /// admission that does not decode names none: its fold refuses it.
    ///
    /// # Errors
    /// A failed store read.
    pub async fn read(
        reads: &dyn DurableReads,
        rows: &[RunRecordRow],
    ) -> Result<Self, DurableError> {
        let mut waits = Self::default();
        for row in rows.iter().filter(|row| row.kind == RunRecordKind::Admit) {
            let Ok(body) = serde_json::from_str::<AdmitBody>(&row.record_json) else {
                continue;
            };
            for id in body
                .members
                .iter()
                .filter_map(|member| member.pinned_wait())
            {
                if let Some(wait) = reads.wait(&id).await? {
                    waits.rows.insert(id, wait);
                }
            }
        }
        Ok(waits)
    }

    /// These waits and `rows`, each under its id.
    #[must_use]
    pub fn with(mut self, rows: impl IntoIterator<Item = WaitRow>) -> Self {
        self.rows.extend(rows.into_iter().map(|row| (row.id, row)));
        self
    }

    /// What `owner`'s admission of `call` of `tool` pinned for its park:
    /// the deadline of `pinned`'s row.
    ///
    /// # Errors
    /// The wait has no row here, wakes another actor, or is not the
    /// completion of this call of this tool.
    pub(super) fn park(
        &self,
        owner: &OwnerKey,
        call: &ToolCallId,
        tool: &ToolId,
        pinned: PinnedWait,
    ) -> Result<ParkDeadline, &'static str> {
        let row = self
            .rows
            .get(&pinned.id)
            .ok_or("a pinned completion wait has no row")?;
        if row.owner != owner.actor() {
            return Err("a pinned completion wait wakes another actor");
        }
        match &row.purpose {
            WaitPurpose::ToolCompletion {
                call: waited,
                tool: of,
                deadline,
            } if waited == call && of == tool => Ok(deadline
                .map_or(ParkDeadline::UntilScopeEnd, |at| {
                    ParkDeadline::At(WaitDeadline::at_instant(at))
                })),
            _ => Err("a pinned completion wait is not its call's completion"),
        }
    }
}

fn corrupt(message: String) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Corrupt,
        message,
    })
}
