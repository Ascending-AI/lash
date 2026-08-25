//! Foreground-turn attachment and deployment wiring.
//!
//! One responsibility: let a process outside the turn's handler observe that
//! turn — attach to its reserved terminal keyed promise and hand a host the
//! bundled effect host plus turn work driver that make out-of-process
//! cancellation and terminal attachment work.

use std::sync::Arc;

use lash_core::{
    AwaitEventWaitIdentity, Resolution, RuntimeError, facade_support::TurnAddress,
    facade_support::TurnAttach, facade_support::TurnTerminal, facade_support::TurnWorkDriver,
};

use crate::durable_wait::{
    RestateDurableWaitAddress, RestateDurableWaitAwaitRequest, restate_await_event_key,
};
use crate::effect_host::RestateEffectHost;
use crate::ingress::{RestateConnection, RestateIngressClient};

/// Restate ingress attachment to a turn's reserved terminal keyed promise.
#[derive(Clone)]
pub struct RestateTurnAttach {
    ingress: RestateIngressClient,
}

impl RestateTurnAttach {
    pub fn new(connection: impl Into<RestateConnection>) -> Self {
        Self {
            ingress: RestateIngressClient::new(connection),
        }
    }
}

#[async_trait::async_trait]
impl TurnAttach for RestateTurnAttach {
    async fn await_terminal(&self, address: &TurnAddress) -> Result<TurnTerminal, RuntimeError> {
        address.execution_scope().validate()?;
        let key = restate_await_event_key(
            &address.execution_scope(),
            AwaitEventWaitIdentity::TurnTerminal,
        )?;
        let durable_address = RestateDurableWaitAddress::for_key(&key);
        let workflow_key = durable_address.workflow_key.clone();
        let resolution = self
            .ingress
            .call_workflow_json::<_, Resolution>(
                "LashDurableWaitWorkflow",
                &workflow_key,
                "await_resolution",
                &RestateDurableWaitAwaitRequest {
                    key,
                    timeout_ms: None,
                },
            )
            .await
            .map_err(|err| {
                let code = if err.is_timeout() {
                    lash_core::RuntimeErrorCode::RestateTurnTerminalAttachCeilingElapsed
                } else {
                    lash_core::RuntimeErrorCode::RestateTurnTerminalAttach
                };
                // A shared handler: a deployment that never bound the
                // durable-wait workflow fails every attach this way, and so
                // does a promise whose invocation the engine no longer holds.
                // Name both rather than leaving an operator to read a bare
                // status out of a transport error — or to be sent after a
                // deployment that is fine.
                let message = if err.is_service_unregistered() {
                    crate::ingress::unresolvable_call_target_message(
                        "LashDurableWaitWorkflow",
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
                    lash_core::RuntimeErrorCode::RestateTurnTerminalDecode,
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
                lash_core::RuntimeErrorCode::RestateTurnTerminalInvalidResolution,
                format!(
                    "terminal promise for turn `{}` in session `{}` resolved with {other:?}",
                    address.turn_id, address.session_id
                ),
            )),
        }
    }
}

/// Bundled Restate wiring for foreground-turn control.
///
/// Use the returned effect host to configure Lash turn execution and the
/// returned driver for out-of-process cancellation/terminal attachment. Bind
/// `LashDurableWaitWorkflowImpl` and `LashDurableWaitIndexImpl` on the endpoint;
/// no Restate Admin API access is involved.
pub struct RestateTurnDeployment {
    effect_host: Arc<RestateEffectHost>,
    driver: TurnWorkDriver,
    attach: Arc<RestateTurnAttach>,
}

impl RestateTurnDeployment {
    pub fn new(connection: impl Into<RestateConnection>) -> Self {
        let connection = connection.into();
        let effect_host = Arc::new(RestateEffectHost::new(connection.clone()));
        let attach = Arc::new(RestateTurnAttach::new(connection));
        let driver = TurnWorkDriver::new(effect_host.clone()).with_attach(attach.clone());
        Self {
            effect_host,
            driver,
            attach,
        }
    }

    pub fn effect_host(&self) -> Arc<RestateEffectHost> {
        Arc::clone(&self.effect_host)
    }

    pub fn turn_work_driver(&self) -> TurnWorkDriver {
        self.driver.clone()
    }

    pub fn turn_attach(&self) -> Arc<RestateTurnAttach> {
        Arc::clone(&self.attach)
    }
}
