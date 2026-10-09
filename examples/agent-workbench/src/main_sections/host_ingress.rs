//! What the workbench does with its own administrative authority: cancel a
//! process and reclaim a deleted session's finished work.
//!
//! Each act runs under a context the core's session administration mints for
//! one runtime operation, the same authority any host acting on external
//! ingress (a webhook, an operator control) uses. The engine then executes
//! what the act recorded: a cancel request is the process's mail.
use super::*;
use lash::ProcessId;
use lash::SessionId;

impl AppState {
    /// A context for one runtime operation of this host, `operation`.
    async fn host_operation(
        &self,
        operation: String,
    ) -> Result<lash::runtime::ActorContext, AppError> {
        self.core
            .session_administration()
            .await
            .effect_host()
            .scoped(lash::runtime::AdmittedScope::runtime_operation(operation))
            // Audited: constructing this scope only validates the local runtime-operation id.
            .map_err(AppError::internal)
    }

    /// Request the cancellation of `process_id` on behalf of `session_id`.
    pub(crate) async fn cancel_process(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        operation_id: &str,
    ) -> Result<lash::persistence::ProcessCancelReceipt, AppError> {
        let operation = self
            .host_operation(format!("workbench-process-cancel:{operation_id}"))
            .await?;
        let receipt = self
            .core
            .processes()
            .cancel(process_id, operation)
            .await
            // Audited: process cancellation uses the global process registry and never consults a session tombstone.
            .map_err(AppError::internal)?;
        self.trace_for_session(
            session_id,
            "process.cancel_requested",
            json!({
                "operation_id": operation_id,
                "process_id": process_id,
                "receipt": receipt,
            }),
        );
        Ok(receipt)
    }

    /// Reclaim the terminal process rows `session_id` originated, as the
    /// retention half of deleting that session (FIG-989).
    ///
    /// Process rows are runtime-global and record their creating session only
    /// as provenance, so the session's close detaches them rather than
    /// deleting them. The bound is identity, not age: the originating session
    /// is gone and its awaits with it, so every one of its terminal rows is
    /// eligible once the tombstone commits. Live processes are untouched: the
    /// lever deletes only terminal rows. The workbench's process-end notices
    /// follow the change feed only for the host-originated processes its
    /// trigger deliveries start, which this filter never selects: no
    /// projection watermark guards it, and no delivered process is pruned
    /// before its delivery is bound.
    pub(crate) async fn prune_processes_originated_by(
        &self,
        session_id: &SessionId,
    ) -> Result<lash::process::ProcessPruneReport, lash::EmbedError> {
        self.core
            .processes()
            .prune(
                u64::MAX,
                Some(&lash::persistence::ProcessListFilter {
                    status: lash::process::ProcessStatusFilter::Any,
                    originator: Some(lash::process::ProcessOriginatorFilter::session(
                        session_id.clone(),
                    )),
                    ..lash::persistence::ProcessListFilter::default()
                }),
                lash::process::ProjectionWatermark::NoProjector,
            )
            .await
        // Audited: process retention reads and writes the global registry and never consults a session tombstone.
    }
}
