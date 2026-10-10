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

use super::host_commands::unsettled_command_error;
use super::{AdminContext, AdminMutation, SessionCommandAdmin};
use crate::support::{
    EmbedError, Result, SessionError, ToolManifest, ToolRestoreReport, ToolState,
};
use lash_core::facade_support::ToolStateFacadeOps;
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

#[derive(Clone)]
/// Facade handle for tool administration.
pub struct ToolAdmin {
    pub(super) context: AdminContext,
}

impl ToolAdmin {
    /// The session's tool state as its durable records state it, read
    /// without building the session's capabilities (FIG-5134): what the
    /// durable head recorded, and the changes this admin's writers submitted
    /// that no run has applied yet.
    pub async fn state(&self) -> Result<SessionToolState> {
        let store = self.context.head_store()?;
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
            })
            .collect();
        Ok(SessionToolState {
            recorded: recorded_tool_state(&store).await?,
            pending,
        })
    }

    /// Toggle Tool Catalog membership for a tool. `present` adds it as a
    /// member; `!present` removes it. Membership is the execution gate.
    ///
    /// The change is a durable session command the next command run applies
    /// against the capabilities its transition builds (FIG-5134); this awaits
    /// its settlement and answers the generation it landed at. A tool id the
    /// head's recorded tool state does not name is refused at once, with a
    /// typed [`ReconfigureError`](crate::tools::ReconfigureError), and nothing
    /// is submitted.
    pub async fn set_membership(
        &self,
        tool_id: impl Into<lash_core::ToolId>,
        present: bool,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<u64>> {
        let idempotency_key = idempotency_key.into();
        let tool_id = tool_id.into();
        self.set_membership_many(&[(tool_id, present)], idempotency_key)
            .await
    }

    /// Applies multiple tool-membership updates atomically, as one durable
    /// command; see [`Self::set_membership`].
    pub async fn set_membership_many(
        &self,
        updates: &[(lash_core::ToolId, bool)],
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<u64>> {
        let idempotency_key = idempotency_key.into();
        let store = self.context.head_store()?;
        if let Some(mut recorded) = recorded_tool_state(&store).await? {
            for (tool_id, member) in updates {
                recorded
                    .set_membership(tool_id, *member)
                    .map_err(EmbedError::from)?;
            }
        }
        change_tool_state(
            &self.context,
            ToolStateChange::SetMembership {
                updates: updates
                    .iter()
                    .map(|(tool_id, member)| ToolMembershipUpdate {
                        tool_id: tool_id.clone(),
                        member: *member,
                    })
                    .collect(),
            },
            idempotency_key,
        )
        .await?
        .try_map(applied_generation)
    }

    /// The manifests of the Tool Catalog members the session's durable head
    /// recorded; empty before a run recorded any.
    pub async fn active_manifests(&self) -> Result<Vec<ToolManifest>> {
        let store = self.context.head_store()?;
        Ok(recorded_tool_state(&store)
            .await?
            .map(|state| state.tool_manifests())
            .unwrap_or_default())
    }

    pub fn advanced(&self) -> AdvancedToolAdmin {
        AdvancedToolAdmin {
            context: self.context.clone(),
        }
    }
}

#[derive(Clone)]
/// Facade handle for advanced tool administration.
pub struct AdvancedToolAdmin {
    pub(super) context: AdminContext,
}

impl AdvancedToolAdmin {
    /// Replace the entire tool-state snapshot.
    ///
    /// This is a generation-checked escape hatch for hosts that intentionally
    /// edit the full snapshot. Prefer `ToolAdmin` membership methods for
    /// ordinary tool policy changes. Like them it is a durable command,
    /// returning its status; a snapshot whose generation is not the
    /// head's recorded one is refused at once with
    /// [`ReconfigureError::GenerationMismatch`](crate::tools::ReconfigureError::GenerationMismatch).
    pub async fn apply_state(
        &self,
        state: ToolState,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<u64>> {
        let idempotency_key = idempotency_key.into();
        let store = self.context.head_store()?;
        if let Some(recorded) = recorded_tool_state(&store).await?
            && recorded.generation() != state.generation()
        {
            return Err(EmbedError::from(
                lash_core::facade_support::ReconfigureError::GenerationMismatch {
                    expected: state.generation(),
                    actual: recorded.generation(),
                },
            ));
        }
        change_tool_state(
            &self.context,
            ToolStateChange::Apply { state },
            idempotency_key,
        )
        .await?
        .try_map(applied_generation)
    }

    /// Restore a persisted tool-state snapshot, adopting its generation.
    ///
    /// Use this when re-applying a snapshot read from durable storage (session
    /// resume), not an edited delta: it reconstructs the exact persisted surface
    /// idempotently rather than requiring the snapshot to match the current
    /// generation. A cold resume of a session whose surface reached generation
    /// ≥ 2 needs this — [`apply_state`](Self::apply_state) would reject it.
    ///
    /// Persisted tools whose source is not currently registered (e.g. a
    /// detached MCP server) do not fail the restore: they are kept as orphaned
    /// non-members, listed in the returned [`ToolRestoreReport`], and rebind
    /// automatically when a source re-advertises the same tool. The restore is
    /// a durable command returning its status, and never refuses under
    /// [`ToolSourcePolicy::Require`](crate::tools::ToolSourcePolicy::Require).
    pub async fn restore_state(
        &self,
        state: ToolState,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<ToolRestoreReport>> {
        let idempotency_key = idempotency_key.into();
        change_tool_state(
            &self.context,
            ToolStateChange::Restore { state },
            idempotency_key,
        )
        .await?
        .try_map(|outcome| match outcome {
            ToolStateChangeOutcome::Restored { report } => Ok(report),
            ToolStateChangeOutcome::Refused { error } => Err(EmbedError::from(error)),
            outcome @ ToolStateChangeOutcome::Applied { .. } => Err(mismatched_outcome(&outcome)),
        })
    }
}

async fn recorded_tool_state(store: &lash_core::store::SessionStore) -> Result<Option<ToolState>> {
    Ok(lash_core::store::load_session_window_state(
        store,
        lash_core::store::WindowSelector::Current,
    )
    .await
    .map_err(EmbedError::Store)?
    .and_then(|loaded| loaded.state.tool_state_snapshot().cloned()))
}

/// Submit `change` and return its applied outcome or pending receipt.
async fn change_tool_state(
    context: &AdminContext,
    change: ToolStateChange,
    idempotency_key: String,
) -> Result<AdminMutation<ToolStateChangeOutcome>> {
    let receipt = SessionCommandAdmin {
        context: context.clone(),
    }
    .submit(
        lash_core::facade_support::SessionCommand::ChangeToolState {
            change: Box::new(change),
        },
        idempotency_key,
    )
    .await?;
    match Box::pin(
        SessionCommandAdmin {
            context: context.clone(),
        }
        .status(receipt),
    )
    .await?
    {
        lash_core::runtime::SessionCommandSettlement::Applied {
            outcome: lash_core::runtime::SessionCommandOutcome::ToolState { outcome },
            ..
        } => Ok(AdminMutation::Applied(outcome)),
        lash_core::runtime::SessionCommandSettlement::Pending(receipt) => {
            Ok(AdminMutation::Pending(receipt))
        }
        settlement => Err(unsettled_command_error(settlement)),
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
