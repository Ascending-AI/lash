//! One host-owned aggregate deadline, carried with a phase's recorded outcome.
use super::PluginCallbackIdentity;
use lash_execution_contracts::plugin_error::PluginError;
use std::time::Duration;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControlPhaseDeadline {
    phase: String,
    bound_ms: u64,
    expires_at_ms: u64,
}

impl ControlPhaseDeadline {
    pub(crate) fn new(clock: &dyn crate::Clock, phase: &str, bound: Duration) -> Self {
        let bound_ms = u64::try_from(bound.as_millis()).unwrap_or(u64::MAX);
        Self {
            phase: phase.into(),
            bound_ms,
            expires_at_ms: clock.timestamp_ms().saturating_add(bound_ms),
        }
    }

    fn expired(&self, callback: &PluginCallbackIdentity) -> PluginError {
        PluginError::Runtime(crate::RuntimeError::new(crate::RuntimeErrorCode::Plugin,
            format!("control phase `{}` expired after {} ms without callback `{callback}` answering", self.phase, self.bound_ms))
            .with_cause(crate::RuntimeErrorCause::ControlPhaseExpired {
                expiry: Box::new(lash_protocol_types::ControlPhaseExpired {
                    phase: self.phase.clone(), bound_ms: self.bound_ms,
                    plugin: callback.owner.clone(), callback: callback.key.clone(),
                }),
            }))
    }

    /// Invoke only while time remains. The timer cannot pre-empt a callback
    /// that runs native code without yielding; dropping its future is no
    /// guarantee that independently spawned work stopped.
    pub(crate) async fn run<O, F, Fut>(
        &self,
        clock: &dyn crate::Clock,
        callback: &PluginCallbackIdentity,
        invoke: F,
    ) -> Result<O, PluginError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<O, PluginError>>,
    {
        let remaining = self.expires_at_ms.saturating_sub(clock.timestamp_ms());
        if remaining == 0 {
            return Err(self.expired(callback));
        }
        let deadline = clock.now() + Duration::from_millis(remaining);
        tokio::select! {
            biased;
            () = clock.sleep_until(deadline) => Err(self.expired(callback)),
            result = async { invoke().await } => {
                if clock.timestamp_ms() >= self.expires_at_ms { Err(self.expired(callback)) } else { result }
            }
        }
    }
}
