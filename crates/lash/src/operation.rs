//! Identity of one operation entering Lash from outside an execution.

/// A host operation's idempotency identity, shared by every retry.
///
/// Construct this from the ingress request's durable identity, before calling
/// Lash. A new identity starts a new operation, even with identical content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationId(pub(crate) lash_core::StartKey);

impl OperationId {
    pub fn new(identity: impl AsRef<[u8]>) -> Self {
        Self(lash_core::StartKey::for_host(identity))
    }

    /// Mint a new operation. Retain it to retry this operation.
    pub fn mint() -> Self {
        Self::new(uuid::Uuid::new_v4().as_bytes())
    }

    pub(crate) fn bind(
        &self,
        actor: &lash_core::ActorContext,
    ) -> crate::Result<lash_core::ActorContext> {
        Ok(actor.scoped(lash_core::AdmittedScope::runtime_operation(
            self.0.to_string(),
        ))?)
    }

    pub(crate) fn key_start(
        &self,
        request: crate::process::ProcessStartRequest,
    ) -> crate::process::ProcessStartRequest {
        lash_execution_contracts::process::model::key_start_request(request, Some(self.0.clone()))
    }
}
