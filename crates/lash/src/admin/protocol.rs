use super::{AdminContext, AdminMutation, SessionStateAdmin};
use crate::support::{EmbedError, Result, SessionError};

#[derive(Clone)]
/// Facade handle for protocol administration.
pub struct ProtocolAdmin {
    pub(super) context: AdminContext,
}

impl ProtocolAdmin {
    /// Record a protocol session extension durably (FIG-5134). It is a host
    /// append of the extension's session nodes, applied by the session's
    /// command lane; a pending receipt follows its settlement. The protocol reads the
    /// nodes when they land and replays them whenever it rebuilds the
    /// session, so the extension holds for every later run.
    pub async fn apply_session_extension(
        &self,
        extension: lash_core::ProtocolSessionExtension,
        idempotency_key: impl Into<String>,
    ) -> Result<AdminMutation<()>> {
        let idempotency_key = idempotency_key.into();
        let fleet = self
            .context
            .runtime
            .observe()
            .queue_store
            .as_ref()
            .map(lash_core::store::SessionStore::fleet_format)
            .ok_or_else(|| {
                EmbedError::Session(SessionError::Protocol(
                    "a session extension is recorded on a store-backed session".to_string(),
                ))
            })?;
        match Box::pin(
            SessionStateAdmin {
                context: self.context.clone(),
            }
            .append_session_nodes(lash_core::AppendSessionNodesRequest {
                operation_id: idempotency_key,
                nodes: extension.session_nodes(fleet),
                requires_ancestor_node_id: None,
            }),
        )
        .await?
        {
            AdminMutation::Applied(lash_core::AppendSessionNodesOutcome::Appended { .. }) => {
                Ok(AdminMutation::Applied(()))
            }
            AdminMutation::Pending(receipt) => Ok(AdminMutation::Pending(receipt)),
            outcome => Err(EmbedError::Session(SessionError::Protocol(format!(
                "a session extension requires no ancestor, yet its append settled {outcome:?}"
            )))),
        }
    }
}
