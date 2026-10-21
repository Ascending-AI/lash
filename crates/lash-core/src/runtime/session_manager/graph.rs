use super::*;
use crate::runtime::session_api::{
    SessionCommandEnqueueError, enqueue_stored_session_command, stored_session_command_status,
};
use lash_execution_contracts::plugin_error::PluginError;

impl CurrentOwnerCapability {
    pub(in crate::runtime::session_manager) async fn append_session_nodes(
        &self,
        session_id: &SessionId,
        request: crate::AppendSessionNodesRequest,
    ) -> Result<crate::SessionGraphAppendResult, PluginError> {
        if request.operation_id.trim().is_empty() {
            return Err(PluginError::Runtime(crate::RuntimeError::new(
                crate::RuntimeErrorCode::SessionCommandIdempotencyKey,
                "session graph append requires a non-empty stable operation_id",
            )));
        }
        let session = self.require_session("append_session_nodes")?;
        if *session_id != session.session_id {
            return Err(PluginError::Session(format!(
                "unknown session `{session_id}`"
            )));
        }
        if let CurrentSnapshot::ReadModel { graph_appends, .. } = &session.snapshot {
            // Applied here means visible in the turn draft. Durability
            // follows the running turn's acknowledged commit.
            return graph_appends
                .record(session_id, &request)
                .map(|outcome| crate::SessionGraphAppendResult::Applied { outcome });
        }
        let store = session.store.as_ref().ok_or_else(|| {
            PluginError::Session("session graph mutation requires a runtime store".to_owned())
        })?;
        let operation_id = request.operation_id.clone();
        let receipt = enqueue_stored_session_command(
            store,
            session_id,
            crate::SessionCommand::AppendSessionNodes {
                request: Box::new(request),
            },
            &operation_id,
            None,
        )
        .await
        .map_err(|error| match error {
            SessionCommandEnqueueError::ChangedContent(_) => {
                let operation = super::super::state::boundary_operation(
                    session_id,
                    &operation_id,
                    "append-session-nodes",
                );
                match operation.storage_key() {
                    Ok(operation_key) => PluginError::AppendOperationIdentityConflict {
                        session_id: session_id.clone(),
                        operation_key,
                    },
                    Err(error) => error.into(),
                }
            }
            SessionCommandEnqueueError::Runtime(error) => PluginError::Runtime(error),
        })?;
        // This is one status operation, never a wait for the applying owner:
        // a caller may be a callback that owner's continuation needs to finish.
        match stored_session_command_status(store, receipt)
            .await
            .map_err(PluginError::Runtime)?
        {
            crate::runtime::SessionCommandSettlement::Applied {
                outcome: crate::runtime::SessionCommandOutcome::AppendSessionNodes { outcome },
                ..
            } => Ok(crate::SessionGraphAppendResult::Applied { outcome }),
            crate::runtime::SessionCommandSettlement::Pending(receipt) => {
                Ok(crate::SessionGraphAppendResult::Pending { receipt })
            }
            crate::runtime::SessionCommandSettlement::Applied {
                outcome: crate::runtime::SessionCommandOutcome::Failed { refusal },
                ..
            } => Err(PluginError::Runtime(refusal.into())),
            crate::runtime::SessionCommandSettlement::Rejected(error) => {
                Err(PluginError::Runtime(error))
            }
            crate::runtime::SessionCommandSettlement::Cancelled(receipt) => {
                Err(crate::StoreError::SessionCommandWithdrawn {
                    session_id: receipt.session_id,
                    batch_id: receipt.batch_id,
                }
                .into())
            }
            settlement => Err(PluginError::Runtime(crate::RuntimeError::new(
                crate::RuntimeErrorCode::SessionCommandRun,
                format!("an append command settled without its append outcome: {settlement:?}"),
            ))),
        }
    }
}
