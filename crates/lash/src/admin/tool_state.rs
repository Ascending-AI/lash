//! Host tool administration without a built session (FIG-5134).
//!
//! An open builds no capabilities (FIG-4857): a session's tool registry exists
//! only once a run publishes its plugin transition. Tool administration
//! therefore reads and writes durable state, never the registry:
//!
//! - [`ToolAdmin::state`](crate::ToolAdmin::state) reads the tool state the
//!   session's durable head recorded, beside the changes a host submitted that
//!   no run has applied yet, typed as pending.
//! - A membership change, snapshot apply or restore is a
//!   [`SessionCommand::ChangeToolState`](lash_core::facade_support::SessionCommand::ChangeToolState):
//!   refused at once when it is invalid against the recorded snapshot,
//!   otherwise durable, applied in lane order by the command run, and awaited
//!   to its settlement.

use super::*;
use lash_core::facade_support::{ToolMembershipUpdate, ToolStateChange, ToolStateChangeOutcome};

/// A session's tool state as its durable records state it: what the head
/// recorded, and the host changes accepted but not applied yet.
#[derive(Clone, Debug)]
pub struct SessionToolState {
    recorded: Option<ToolState>,
    pending: Vec<PendingToolStateChange>,
}

impl SessionToolState {
    /// The tool state the session's durable head recorded. `None` until a
    /// run built the session's capabilities and committed their state.
    pub fn recorded(&self) -> Option<&ToolState> {
        self.recorded.as_ref()
    }

    /// Changes a host submitted that no run has applied yet, in the order the
    /// command lane applies them. None of them is reflected in
    /// [`recorded`](Self::recorded).
    pub fn pending(&self) -> &[PendingToolStateChange] {
        &self.pending
    }
}

/// A tool-state change the session accepted durably and has not applied.
#[derive(Clone, Debug)]
pub struct PendingToolStateChange {
    /// The command's receipt: settle or withdraw it through
    /// [`SessionCommandAdmin`](crate::SessionCommandAdmin).
    pub receipt: lash_core::facade_support::SessionCommandReceipt,
    pub change: ToolStateChange,
}

impl SessionAdmin {
    /// The session's store, which every durable admin read answers from.
    pub(super) fn head_store(&self) -> Result<lash_core::store::SessionStore> {
        self.runtime.observe().queue_store.clone().ok_or_else(|| {
            EmbedError::Session(SessionError::Protocol(
                "a durable admin read needs a store-backed session".to_string(),
            ))
        })
    }

    async fn recorded_tool_state(
        store: &lash_core::store::SessionStore,
    ) -> Result<Option<ToolState>> {
        Ok(lash_core::store::load_session_window_state(
            store,
            lash_core::store::WindowSelector::Current,
        )
        .await
        .map_err(EmbedError::Store)?
        .and_then(|loaded| loaded.state.tool_state_snapshot().cloned()))
    }

    pub(super) async fn tool_state(&self) -> Result<SessionToolState> {
        let store = self.head_store()?;
        // The lane is read before the head: a change that applies between
        // the two reads shows as pending and applied, never as neither.
        let mut open = store
            .list_open_queued_work()
            .await
            .map_err(EmbedError::Store)?;
        open.sort_by_key(|batch| batch.enqueue_seq);
        let pending = open
            .into_iter()
            .filter_map(|batch| match batch.payload {
                lash_core::runtime::QueuedWorkPayload::SessionCommand { command } => match *command
                {
                    lash_core::facade_support::SessionCommand::ChangeToolState { change } => {
                        Some(PendingToolStateChange {
                            receipt: lash_core::facade_support::SessionCommandReceipt {
                                session_id: batch.session_id,
                                batch_id: batch.batch_id,
                                source_key: batch.source_key.unwrap_or_default(),
                            },
                            change: *change,
                        })
                    }
                    _ => None,
                },
                lash_core::runtime::QueuedWorkPayload::ProcessWake { .. } => None,
            })
            .collect();
        Ok(SessionToolState {
            recorded: Self::recorded_tool_state(&store).await?,
            pending,
        })
    }

    /// Submit `change` and await its settlement.
    async fn change_tool_state(&self, change: ToolStateChange) -> Result<ToolStateChangeOutcome> {
        let receipt = self
            .submit_session_command(
                lash_core::facade_support::SessionCommand::ChangeToolState {
                    change: Box::new(change),
                },
                format!("tool-state:{}", uuid::Uuid::new_v4()),
            )
            .await?;
        match Box::pin(self.await_command_settlement(receipt, None)).await? {
            lash_core::runtime::SessionCommandSettlement::Applied {
                outcome: lash_core::runtime::SessionCommandOutcome::ToolState { outcome },
                ..
            } => Ok(outcome),
            settlement => Err(unsettled_command_error(settlement)),
        }
    }

    pub(super) async fn apply_tool_state(&self, state: ToolState) -> Result<u64> {
        let store = self.head_store()?;
        if let Some(recorded) = Self::recorded_tool_state(&store).await?
            && recorded.generation() != state.generation()
        {
            return Err(EmbedError::from(
                lash_core::facade_support::ReconfigureError::GenerationMismatch {
                    expected: state.generation(),
                    actual: recorded.generation(),
                },
            ));
        }
        applied_generation(
            self.change_tool_state(ToolStateChange::Apply { state })
                .await?,
        )
    }

    pub(super) async fn restore_tool_state(&self, state: ToolState) -> Result<ToolRestoreReport> {
        match self
            .change_tool_state(ToolStateChange::Restore { state })
            .await?
        {
            ToolStateChangeOutcome::Restored { report } => Ok(report),
            ToolStateChangeOutcome::Refused { error } => Err(EmbedError::from(error)),
            outcome @ ToolStateChangeOutcome::Applied { .. } => Err(mismatched_outcome(&outcome)),
        }
    }

    pub(super) async fn set_tool_membership_many(
        &self,
        updates: &[(lash_core::ToolId, bool)],
    ) -> Result<u64> {
        let store = self.head_store()?;
        if let Some(mut recorded) = Self::recorded_tool_state(&store).await? {
            for (tool_id, member) in updates {
                recorded
                    .set_membership(tool_id, *member)
                    .map_err(EmbedError::from)?;
            }
        }
        applied_generation(
            self.change_tool_state(ToolStateChange::SetMembership {
                updates: updates
                    .iter()
                    .map(|(tool_id, member)| ToolMembershipUpdate {
                        tool_id: tool_id.clone(),
                        member: *member,
                    })
                    .collect(),
            })
            .await?,
        )
    }

    pub(super) async fn active_tool_manifests(&self) -> Result<Vec<ToolManifest>> {
        let store = self.head_store()?;
        Ok(Self::recorded_tool_state(&store)
            .await?
            .map(|state| state.tool_manifests())
            .unwrap_or_default())
    }
}

fn applied_generation(outcome: ToolStateChangeOutcome) -> Result<u64> {
    match outcome {
        ToolStateChangeOutcome::Applied { generation } => Ok(generation),
        ToolStateChangeOutcome::Refused { error } => Err(EmbedError::from(error)),
        outcome @ ToolStateChangeOutcome::Restored { .. } => Err(mismatched_outcome(&outcome)),
    }
}

fn mismatched_outcome(outcome: &ToolStateChangeOutcome) -> EmbedError {
    EmbedError::Session(SessionError::Protocol(format!(
        "a tool-state change settled with another change's outcome: {outcome:?}"
    )))
}
