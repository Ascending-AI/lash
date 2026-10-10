//! An idle session an earlier build wrote, carried to this build's formats
//! outside a turn (ADR 0106 §2, ADR 0115 §3.5; FIG-5787).
//!
//! A session holds kernel-versioned state between turns: its bindings and
//! the functions it saved. Its actor stays in the format set of the build
//! that wrote it until a turn is admitted, so a session that runs no turn
//! through the one-release window would be stranded under the build that
//! closes it. The operator's sweep wakes it; the session actor of this
//! build, with nothing to admit, restores the session from its head, which
//! carries what the restore reads forward, and commits what it recaptured
//! with this build's format set in one commit.

use crate::facade_support::RuntimeSessionStateFacadeOps as _;
use crate::store::RuntimeCommit;
use crate::{LashRuntime, RuntimeError, RuntimeErrorCode};

use super::turn_boundary::{
    ExecutionStateUpdate, capture_execution_state_update, execution_state_capture_error,
};

impl LashRuntime {
    /// Restore the session from its committed head, recapture its code
    /// executor's state when the restore carried any of it forward, and
    /// commit that with the session's format set `formats` as the session
    /// actor `owner`'s commit; with nothing recaptured, record the format
    /// set alone.
    ///
    /// # Errors
    ///
    /// [`RuntimeError`] when the session does not restore under this build,
    /// or the commit is refused; nothing was written.
    pub async fn carry_session_state(
        &mut self,
        owner: &crate::ActorContext,
        formats: lash_durable::FormatSet,
    ) -> Result<(), RuntimeError> {
        Box::pin(self.materialize_command_session(owner)).await?;
        let update = match self.session.as_mut() {
            Some(session) => capture_execution_state_update(session)
                .await
                .map_err(execution_state_capture_error)
                .map_err(super::runtime_error_from_store_commit)?,
            None => ExecutionStateUpdate::Clean,
        };
        if update == ExecutionStateUpdate::Clean {
            let stamped = async {
                let mut tx = owner.begin().await?;
                tx.stamp_formats(formats);
                owner
                    .commit(tx, lash_durable::CommitLabel::SESSION_COMMAND)
                    .await
            };
            return stamped.await.map(|_| ()).map_err(|error| {
                RuntimeError::new(
                    RuntimeErrorCode::StoreCommitFailed,
                    format!("the session's carry to this build's formats: {error}"),
                )
            });
        }
        let fleet_format = self.fleet_format();
        // One carry per head the session stands on: a later window's carry
        // of the same session is an operation of its own.
        let operation = crate::OperationId::new(
            self.state
                .session_operation_scope(format!("kernel-carry-{}", self.state.head_revision)),
            "session-carry",
        );
        let state = &mut self.state;
        update
            .apply(state)
            .map_err(super::runtime_error_from_store_commit)?;
        if let Some(session) = self.session.as_ref() {
            state.capture_plugin_states(session.plugins(), fleet_format)?;
        }
        let (commit, _persisted_node_ids) =
            RuntimeCommit::persisted_state_with_operation_and_budget(
                state,
                operation,
                self.host.core.durability.commit_budget,
                fleet_format,
            )
            .map_err(super::runtime_error_from_store_commit)?;
        let committed = super::durable::head_commit::commit_in(
            owner,
            commit,
            lash_durable::CommitLabel::SESSION_COMMAND,
            self.host.core.tracing.metrics(),
            Some(formats),
        )
        .await;
        // The resident session gives way to the durable head, which a landed
        // commit moved.
        self.invalidate_resident_session_state();
        committed.map_err(super::durable::head_commit::HeadCommitError::into_runtime_error)?;
        self.reload_invalidated_resident_session_state().await
    }
}
