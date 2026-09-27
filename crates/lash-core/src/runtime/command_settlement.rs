//! The settlement waiter's command drain (ADR 0109 §7): a waiter that finds
//! the session's lane free drains the command lane itself, each commit fenced
//! by the session's drive epoch, so a drive that admits meanwhile wins.

use super::*;

impl LashRuntime {
    /// The drive fence a settlement waiter's drain presents (ADR 0109 §7):
    /// the session's current fence, so a drive that seals an admission after
    /// this read refuses the drain's commits. The waiter never raises the
    /// epoch itself: a raise would supersede a drive mid-admission, and a
    /// superseded drive stops, leaving what it was asked for to nobody.
    /// Before the session's first seal there is no epoch to fence against,
    /// and a store that keeps no drive epoch has none; the lane lease alone
    /// orders such a drain against the first drive.
    pub(super) async fn settlement_drive_fence(
        &self,
        store: &dyn crate::RuntimePersistence,
    ) -> Result<Option<crate::store::DriveFence>, RuntimeError> {
        match crate::store::current_drive_fence(store, &self.state.session_id).await {
            Ok(fence) => Ok(fence),
            Err(
                crate::StoreError::DriveEpochUnavailable { .. }
                | crate::StoreError::UnsupportedStoreOperation { .. },
            ) => Ok(None),
            Err(error) => Err(super::runtime_error_from_store_commit(error)),
        }
    }

    /// Drain the command lane under `lease`, each commit fenced by
    /// `drive_fence`, until `receipt`'s command settled or the lane is
    /// empty. A commit a later drive admission refused ends the drain
    /// without error: that drive applies what is left.
    pub(super) async fn drain_commands_until_settled(
        &mut self,
        store: &dyn crate::RuntimePersistence,
        lease: &crate::SessionExecutionLeaseAuthority,
        drive_fence: Option<&crate::store::DriveFence>,
        receipt: &crate::SessionCommandReceipt,
    ) -> Result<(), RuntimeError> {
        let host = self.effect_host();
        let controller = host.scoped(crate::AdmittedScope::queue_drain(
            &self.state.session_id,
            "session-command",
        ))?;
        loop {
            match self
                .drain_next_session_command_fenced(
                    lease,
                    drive_fence,
                    tokio_util::sync::CancellationToken::new(),
                    controller.controller(),
                )
                .await
            {
                Ok(Some(_)) => {}
                Ok(None) => return Ok(()),
                Err(error) if error.code == RuntimeErrorCode::StoreCommitSuperseded => {
                    tracing::debug!(
                        session_id = %self.state.session_id,
                        batch_id = %receipt.batch_id,
                        error = %error,
                        "a drive admitted after the settlement waiter took the lane; the drive applies the command"
                    );
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
            let target_pending = store
                .list_queued_work(&receipt.session_id)
                .await
                .map_err(super::runtime_error_from_store_commit)?
                .iter()
                .any(|batch| batch.batch_id == receipt.batch_id);
            if !target_pending {
                return Ok(());
            }
        }
    }
}
