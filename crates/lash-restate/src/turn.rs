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
                    address: durable_address,
                    timeout_ms: None,
                },
            )
            .await
            .map_err(|err| RuntimeError::new("restate_turn_terminal_attach", err.to_string()))?;
        match resolution {
            Resolution::Ok(value) => serde_json::from_value(value)
                .map_err(|err| RuntimeError::new("restate_turn_terminal_decode", err.to_string())),
            Resolution::Cancelled => Err(RuntimeError::new(
                "turn_control_unknown_or_revoked",
                format!(
                    "terminal promise for turn `{}` in session `{}` was revoked",
                    address.turn_id, address.session_id
                ),
            )),
            other => Err(RuntimeError::new(
                "restate_turn_terminal_invalid_resolution",
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
