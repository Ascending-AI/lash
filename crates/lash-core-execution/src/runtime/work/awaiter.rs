use crate::ProcessId;
use std::sync::Arc;
use std::time::Duration;

use crate::{PluginError, ProcessAwaitOutput, ProcessEvent, ProcessRegistry, WorkCadencePolicy};

use super::super::process::ProcessChangeHub;

/// Waits on a process registry for a process's terminal state and events
/// (ADR 0016).
///
/// It performs narrow point reads (`get_process`, `event_page`) and wakes
/// promptly from the composition-owned change hub. Callers still bound every
/// wait with their cancellation select or [`tokio::time::timeout`].
#[derive(Clone)]
pub struct ProcessRegistryAwaiter {
    registry: Arc<dyn ProcessRegistry>,
    hub: Option<ProcessChangeHub>,
    work_cadence: WorkCadencePolicy,
}

impl ProcessRegistryAwaiter {
    pub fn new(registry: Arc<dyn ProcessRegistry>, hub: ProcessChangeHub) -> Self {
        Self {
            registry,
            hub: Some(hub),
            work_cadence: WorkCadencePolicy::default(),
        }
    }

    pub fn new_with_work_cadence(
        registry: Arc<dyn ProcessRegistry>,
        hub: ProcessChangeHub,
        work_cadence: WorkCadencePolicy,
    ) -> Self {
        Self {
            registry,
            hub: Some(hub),
            work_cadence,
        }
    }

    /// An awaiter over `registry` with no change hub: it polls on the default
    /// cadence.
    pub fn for_registry(registry: Arc<dyn ProcessRegistry>) -> Self {
        Self {
            registry,
            hub: None,
            work_cadence: WorkCadencePolicy::default(),
        }
    }

    pub fn with_work_cadence(mut self, work_cadence: WorkCadencePolicy) -> Self {
        self.work_cadence = work_cadence;
        self
    }

    pub async fn await_terminal(
        &self,
        process_id: &ProcessId,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        if let Some(output) = self.try_terminal(process_id).await? {
            return Ok(output);
        }
        self.wait_for(process_id, || self.try_terminal(process_id))
            .await
    }

    pub async fn await_event(
        &self,
        process_id: &ProcessId,
        kind: crate::ProcessEventKind,
        after_sequence: u64,
    ) -> Result<ProcessEvent, PluginError> {
        if let Some(event) = self.read_event(process_id, kind, after_sequence).await? {
            return Ok(event);
        }
        self.wait_for(process_id, || {
            self.read_event(process_id, kind, after_sequence)
        })
        .await
    }

    async fn wait_for<T, F, Fut>(
        &self,
        process_id: &ProcessId,
        mut check: F,
    ) -> Result<T, PluginError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<Option<T>, PluginError>>,
    {
        let mut backoff = self.work_cadence.poll_initial;
        if let Some(hub) = self.hub.as_ref() {
            let mut subscription = hub.subscribe(process_id);
            loop {
                if let Some(item) = check().await? {
                    return Ok(item);
                }
                tokio::select! {
                    changed = subscription.changed() => {
                        match changed {
                            Ok(()) => backoff = self.work_cadence.poll_initial,
                            Err(_) => break,
                        }
                    }
                    _ = tokio::time::sleep(backoff) => {
                        backoff = next_backoff(backoff, self.work_cadence.poll_max);
                    }
                }
            }
        }
        loop {
            if let Some(item) = check().await? {
                return Ok(item);
            }
            tokio::time::sleep(backoff).await;
            backoff = next_backoff(backoff, self.work_cadence.poll_max);
        }
    }

    async fn try_terminal(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<ProcessAwaitOutput>, PluginError> {
        let record = match self.registry.get_process(process_id).await {
            Ok(Some(record)) => record,
            Ok(None) => {
                return Err(PluginError::ProcessUnknown {
                    process_id: process_id.clone(),
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
        if let Some(output) = record.outcome() {
            return Ok(Some(output));
        }
        Ok(None)
    }

    async fn read_event(
        &self,
        process_id: &ProcessId,
        kind: crate::ProcessEventKind,
        after_sequence: u64,
    ) -> Result<Option<ProcessEvent>, PluginError> {
        let limit = std::num::NonZeroUsize::new(128).unwrap_or(std::num::NonZeroUsize::MIN);
        let mut after_sequence = after_sequence;
        loop {
            let outcome = self
                .registry
                .event_page_after(
                    process_id,
                    after_sequence,
                    limit,
                    crate::ProcessEventQueryMode::Full,
                )
                .await?;
            let page = match outcome {
                crate::ProcessEventReadOutcome::Retained(page) => page,
                crate::ProcessEventReadOutcome::NoLongerRetained(
                    crate::ProcessEventHistoryRetention::Pruned {
                        terminal_label,
                        pruned_at_ms,
                    },
                ) => {
                    return Err(PluginError::ProcessNoLongerRetained {
                        terminal_label,
                        pruned_at_ms,
                    });
                }
                // The event asked for may lie in the released prefix: say so
                // rather than answer from the events after it.
                crate::ProcessEventReadOutcome::NoLongerRetained(
                    crate::ProcessEventHistoryRetention::Released { released_through },
                ) => {
                    return Err(PluginError::ProcessEventsReleased {
                        process_id: process_id.clone(),
                        released_through,
                    });
                }
            };
            let crate::ProcessEventPageEvents::Full(events) = page.events else {
                unreachable!("full process event query returned a lite page");
            };
            if let Some(event) = events.into_iter().find(|event| event.kind() == kind) {
                return Ok(Some(event));
            }
            after_sequence = match page.more {
                crate::ProcessEventPageMore::Complete => return Ok(None),
                crate::ProcessEventPageMore::More { after_sequence } => after_sequence,
            };
        }
    }
}

fn next_backoff(current: Duration, maximum: Duration) -> Duration {
    current.saturating_mul(2).min(maximum)
}

#[cfg(test)]
mod tests;
