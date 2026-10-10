//! A host's tool-state change, applied by the command lane (FIG-5134).
//!
//! A session builds its tool registry only when a run publishes its plugin
//! transition (FIG-4857), so a host's membership change, snapshot apply or
//! restore is a [`SessionCommand::ChangeToolState`](crate::SessionCommand):
//! durable when the host submits it, applied in lane order by the command
//! run against the capabilities that run's transition built, and settled in
//! the one commit that persists the changed tool state. A change that does
//! not apply against the live tool state settles
//! [`Refused`](ToolStateChangeOutcome::Refused) with its
//! typed [`ReconfigureError`](crate::ReconfigureError) and commits nothing of
//! it, so the lane never waits on it.

use super::LashRuntime;
use super::host_commands::CommandCommit;
use crate::{RuntimeError, RuntimeErrorCode, SessionError};
use lash_core_store::tool_state::facade_ops::ToolStateFacadeOps as _;
use lash_core_store::tool_state::{ToolStateChange, ToolStateChangeOutcome};

impl LashRuntime {
    pub(super) async fn apply_tool_state_command(
        &mut self,
        change: ToolStateChange,
        completion: crate::QueuedWorkCompletion,
        owner: &crate::ActorContext,
    ) -> Result<bool, RuntimeError> {
        let outcome = self.change_tool_state(change).await.map_err(|error| {
            RuntimeError::new(
                RuntimeErrorCode::SessionCommandRefreshTools,
                error.to_string(),
            )
        })?;
        let committed =
            Box::pin(
                self.commit_host_command(owner, &completion, None, None, |_, _| {
                    crate::runtime::SessionCommandOutcome::ToolState { outcome }
                }),
            )
            .await?;
        Ok(!matches!(committed, CommandCommit::Withdrawn))
    }

    /// Apply `change` to the live registry. A change the registry refuses
    /// leaves it unchanged and answers `Refused`.
    async fn change_tool_state(
        &mut self,
        change: ToolStateChange,
    ) -> Result<ToolStateChangeOutcome, SessionError> {
        self.reload_invalidated_resident_session_state_for_session()
            .await?;
        let next = match change {
            ToolStateChange::Restore { state } => {
                return match Box::pin(self.restore_tool_state(state)).await {
                    Ok(report) => Ok(ToolStateChangeOutcome::Restored { report }),
                    Err(error) => match collision_error(&error) {
                        Some(error) => Ok(ToolStateChangeOutcome::Refused { error }),
                        None => Err(error),
                    },
                };
            }
            ToolStateChange::Apply { state } => state,
            ToolStateChange::SetMembership { updates } => {
                let mut state = self.tool_state()?;
                for update in &updates {
                    if let Err(error) = state.set_membership(&update.tool_id, update.member) {
                        return Ok(ToolStateChangeOutcome::Refused { error });
                    }
                }
                state
            }
        };
        let Some(session) = self.session.as_mut() else {
            return Err(SessionError::Protocol(
                "runtime session not available".to_string(),
            ));
        };
        let registry = session.plugins().tool_registry();
        let (revision, preview) = registry.preview_reconfiguration();
        let generation = match preview.apply_state(next) {
            Ok(generation) => generation,
            Err(error) => return Ok(ToolStateChangeOutcome::Refused { error }),
        };
        if let Err(error) = session
            .validate_tool_registry(std::sync::Arc::new(preview.clone()))
            .await
        {
            let error = SessionError::Plugin(error);
            return match collision_error(&error) {
                Some(error) => Ok(ToolStateChangeOutcome::Refused { error }),
                None => Err(error),
            };
        }
        if let Err(error) = registry.publish_reconfiguration(revision, &preview) {
            return Ok(ToolStateChangeOutcome::Refused { error });
        }
        session.refresh_tool_catalog().await?;
        self.stamp_live_plugin_state()
            .map_err(|error| SessionError::Plugin(crate::PluginError::Runtime(error)))?;
        Ok(ToolStateChangeOutcome::Applied { generation })
    }
}

fn collision_error(error: &SessionError) -> Option<crate::ReconfigureError> {
    let SessionError::Plugin(error) = error else {
        return None;
    };
    let refusal = super::config_ops::catalog_config_refusal(error)?;
    match refusal.owner_refusal::<crate::CoreConfigRefusal>()? {
        crate::CoreConfigRefusal::ToolNamespaceCollision { root, binding } => {
            Some(crate::ReconfigureError::ToolNamespaceCollision { root, binding })
        }
        _ => None,
    }
}
