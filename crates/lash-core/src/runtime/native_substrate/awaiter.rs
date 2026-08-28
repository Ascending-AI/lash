use std::sync::Arc;
use std::time::Duration;

use crate::{PluginError, ProcessAwaitOutput, ProcessEvent, ProcessRegistry};

use super::super::process::ProcessChangeHub;

const AWAIT_BACKOFF_MIN: Duration = Duration::from_millis(25);
const AWAIT_BACKOFF_MAX: Duration = Duration::from_secs(1);

/// Native waiter for process terminal state and events (ADR 0016).
///
/// It performs narrow point reads (`get_process`, `events_after`) and wakes
/// promptly from the composition-owned change hub. Callers still bound every
/// wait with their cancellation select or [`tokio::time::timeout`].
#[derive(Clone)]
pub(crate) struct NativeProcessAwaiter {
    registry: Arc<dyn ProcessRegistry>,
    hub: Option<ProcessChangeHub>,
}

impl NativeProcessAwaiter {
    pub(crate) fn new(registry: Arc<dyn ProcessRegistry>, hub: ProcessChangeHub) -> Self {
        Self {
            registry,
            hub: Some(hub),
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn for_registry(registry: Arc<dyn ProcessRegistry>) -> Self {
        Self {
            registry,
            hub: None,
        }
    }

    pub(crate) async fn await_terminal(
        &self,
        process_id: &str,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        if let Some(output) = self.try_terminal(process_id).await? {
            return Ok(output);
        }
        crate::runtime::process_worker::release_process_execution_permit_while(
            self.wait_for(process_id, || self.try_terminal(process_id)),
        )
        .await
    }

    pub(crate) async fn await_event(
        &self,
        process_id: &str,
        event_type: &str,
        after_sequence: u64,
    ) -> Result<ProcessEvent, PluginError> {
        if let Some(event) = self
            .read_event(process_id, event_type, after_sequence)
            .await?
        {
            return Ok(event);
        }
        crate::runtime::process_worker::release_process_execution_permit_while(
            self.wait_for(process_id, || {
                self.read_event(process_id, event_type, after_sequence)
            }),
        )
        .await
    }

    async fn wait_for<T, F, Fut>(&self, process_id: &str, mut check: F) -> Result<T, PluginError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<Option<T>, PluginError>>,
    {
        let mut backoff = AWAIT_BACKOFF_MIN;
        if let Some(hub) = self.hub.as_ref() {
            let mut rx = hub.subscribe(process_id);
            loop {
                if let Some(item) = check().await? {
                    return Ok(item);
                }
                tokio::select! {
                    changed = rx.changed() => {
                        match changed {
                            Ok(()) => backoff = AWAIT_BACKOFF_MIN,
                            Err(_) => break,
                        }
                    }
                    _ = tokio::time::sleep(backoff) => {
                        backoff = next_backoff(backoff);
                    }
                }
            }
        }
        loop {
            if let Some(item) = check().await? {
                return Ok(item);
            }
            tokio::time::sleep(backoff).await;
            backoff = next_backoff(backoff);
        }
    }

    async fn try_terminal(
        &self,
        process_id: &str,
    ) -> Result<Option<ProcessAwaitOutput>, PluginError> {
        let record = match self.registry.get_process(process_id).await {
            Ok(Some(record)) => record,
            Ok(None) => {
                return Err(PluginError::ProcessUnknown {
                    process_id: process_id.to_string(),
                });
            }
            Err(PluginError::ProcessNoLongerRetained {
                terminal_label,
                pruned_at_ms,
            }) => {
                return Ok(Some(ProcessAwaitOutput::NoLongerRetained {
                    terminal_label,
                    pruned_at_ms,
                }));
            }
            Err(error) => return Err(error),
        };
        if record.status == crate::ProcessStatus::CallerDeparted {
            return Err(PluginError::ProcessCallerDeparted {
                process_id: process_id.to_string(),
            });
        }
        Ok(record.outcome)
    }

    async fn read_event(
        &self,
        process_id: &str,
        event_type: &str,
        after_sequence: u64,
    ) -> Result<Option<ProcessEvent>, PluginError> {
        Ok(self
            .registry
            .events_after(process_id, after_sequence)
            .await?
            .into_iter()
            .find(|event| event.event_type == event_type))
    }
}

fn next_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(AWAIT_BACKOFF_MAX)
}

#[cfg(test)]
mod tests;
