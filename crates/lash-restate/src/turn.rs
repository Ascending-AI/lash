//! Foreground-turn attachment.
//!
//! One responsibility: let a process outside the turn's handler observe that
//! turn by attaching to its reserved terminal keyed promise.

use lash_core::{
    AwaitEventWaitIdentity, Resolution, RuntimeError, facade_support::TurnAddress,
    facade_support::TurnAttach, facade_support::TurnTerminal,
};

use crate::durable_wait::{
    RestateDurableWaitAddress, RestateDurableWaitAwaitRequest,
    restate_await_event_key_for_authority,
};
use crate::ingress::{RestateAuthorityId, RestateConnection, RestateIngressClient};

/// Restate ingress attachment to a turn's reserved terminal keyed promise.
#[derive(Clone)]
pub struct RestateTurnAttach {
    ingress: RestateIngressClient,
    authority_id: RestateAuthorityId,
}

impl RestateTurnAttach {
    pub fn new(connection: impl Into<RestateConnection>, authority_id: RestateAuthorityId) -> Self {
        Self {
            ingress: RestateIngressClient::new(connection),
            authority_id,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(connection: impl Into<RestateConnection>) -> Self {
        Self::new(
            connection,
            RestateAuthorityId::new("lash-restate-tests").expect("valid test authority"),
        )
    }
}

#[async_trait::async_trait]
impl TurnAttach for RestateTurnAttach {
    async fn await_terminal(&self, address: &TurnAddress) -> Result<TurnTerminal, RuntimeError> {
        address.execution_scope().validate()?;
        let key = restate_await_event_key_for_authority(
            &self.authority_id,
            &address.execution_scope(),
            AwaitEventWaitIdentity::TurnTerminal,
        )?;
        let durable_address = RestateDurableWaitAddress::for_key(&key);
        let workflow_key = durable_address.workflow_key.clone();
        let resolution = self
            .ingress
            .call_workflow_json::<_, Resolution>(
                crate::LashService::DurableWaitWorkflow.name(),
                &workflow_key,
                "await_resolution",
                &RestateDurableWaitAwaitRequest {
                    key,
                    deadline: None,
                },
            )
            .await
            .map_err(|err| {
                let code = if err.is_timeout() {
                    lash_core::RuntimeErrorCode::EngineTurnTerminalAttachCeilingElapsed
                } else {
                    lash_core::RuntimeErrorCode::EngineTurnTerminalAttach
                };
                // A shared handler: a deployment that never bound the
                // durable-wait workflow fails every attach this way, and so
                // does a promise whose invocation the engine no longer holds.
                // Name both rather than leaving an operator to read a bare
                // status out of a transport error — or to be sent after a
                // deployment that is fine.
                let message = if err.is_service_unregistered() {
                    crate::ingress::unresolvable_call_target_message(
                        crate::LashService::DurableWaitWorkflow.name(),
                        "await_resolution",
                        &err,
                    )
                } else {
                    err.to_string()
                };
                RuntimeError::new(code, message)
            })?;
        match resolution {
            Resolution::Ok(value) => serde_json::from_value(value).map_err(|err| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::EngineTurnTerminalDecode,
                    err.to_string(),
                )
            }),
            Resolution::Cancelled => Err(RuntimeError::new(
                lash_core::RuntimeErrorCode::TurnControlUnknownOrRevoked,
                format!(
                    "terminal promise for turn `{}` in session `{}` was revoked",
                    address.turn_id, address.session_id
                ),
            )),
            other => Err(RuntimeError::new(
                lash_core::RuntimeErrorCode::EngineTurnTerminalInvalidResolution,
                format!(
                    "terminal promise for turn `{}` in session `{}` resolved with {other:?}",
                    address.turn_id, address.session_id
                ),
            )),
        }
    }
}
