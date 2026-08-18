use std::sync::Arc;

use super::DurableProcessWorker;
use super::process::{
    ProcessAttach, ProcessAwaiter, ProcessChangeHub, ProcessEvent, ProcessEventSink,
    ProcessRegistry, watch_process_registry_with_sink,
};
use super::process_worker::ProcessAdmissionReport;
use crate::{PluginError, ProcessAwaitOutput};

/// Registry and run handle for process work owned outside
/// [`LashCore`](https://docs.rs/lash/latest/lash/struct.LashCore.html).
///
/// The registry non-terminal rows are the durable work queue. Hosts drive that
/// queue explicitly by calling [`claim_and_run_pending`](Self::claim_and_run_pending)
/// on each relevant event. Cross-process idempotency belongs to the registry
/// claim; there is no core-owned polling loop.
#[derive(Clone)]
pub struct ProcessWorkDriver {
    registry: Arc<dyn ProcessRegistry>,
    run_handle: Arc<dyn ProcessRunHandle>,
    awaiter: ProcessAwaiter,
    attach: Option<Arc<dyn ProcessAttach>>,
    hub: ProcessChangeHub,
}

impl ProcessWorkDriver {
    pub fn new(registry: Arc<dyn ProcessRegistry>, run_handle: Arc<dyn ProcessRunHandle>) -> Self {
        Self::new_with_sink(registry, run_handle, None)
    }

    /// Like [`new`](Self::new), but installs a host-facing
    /// [`ProcessEventSink`] on the registry decorator this driver wraps.
    ///
    /// The sink receives every appended event, best-effort, after its durable
    /// write — see [`ProcessEventSink`] for the freshness-not-truth contract.
    pub fn new_with_sink(
        registry: Arc<dyn ProcessRegistry>,
        run_handle: Arc<dyn ProcessRunHandle>,
        sink: Option<Arc<dyn ProcessEventSink>>,
    ) -> Self {
        let (registry, hub) = watch_process_registry_with_sink(registry, sink);
        Self::from_watched(registry, hub, run_handle)
    }

    pub fn from_watched(
        registry: Arc<dyn ProcessRegistry>,
        hub: ProcessChangeHub,
        run_handle: Arc<dyn ProcessRunHandle>,
    ) -> Self {
        let awaiter = ProcessAwaiter::new(Arc::clone(&registry), hub.clone());
        Self {
            registry,
            run_handle,
            awaiter,
            attach: None,
            hub,
        }
    }

    pub fn with_attach(mut self, attach: Arc<dyn ProcessAttach>) -> Self {
        self.attach = Some(attach);
        self
    }

    pub fn inline(registry: Arc<dyn ProcessRegistry>, worker: DurableProcessWorker) -> Self {
        Self::new(registry, Arc::new(InlineProcessRunHandle::new(worker)))
    }

    pub fn process_registry(&self) -> Arc<dyn ProcessRegistry> {
        Arc::clone(&self.registry)
    }

    pub fn change_hub(&self) -> ProcessChangeHub {
        self.hub.clone()
    }

    pub fn awaiter(&self) -> ProcessAwaiter {
        self.awaiter.clone()
    }

    /// Wait for `process_id` to reach a terminal state and return its outcome.
    ///
    /// This is the one way to wait on a started work item (ADR 0016): never a
    /// raw registry poll loop. The mechanism matches the deployment — an
    /// engine-native durable promise when a [`ProcessAttach`] is installed
    /// (Restate ingress attach), otherwise the in-process change hub plus
    /// bounded backoff point reads. An already-terminal process returns
    /// immediately.
    ///
    /// Callers must bound the wait themselves: a process that never terminates
    /// would otherwise pin the caller forever. Wrap it in
    /// [`tokio::time::timeout`].
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use lash_core::PluginError;
    /// use lash_core::runtime::ProcessWorkDriver;
    ///
    /// async fn wait(driver: &ProcessWorkDriver, process_id: &str) -> Result<(), PluginError> {
    ///     match tokio::time::timeout(Duration::from_secs(30), driver.await_terminal(process_id)).await {
    ///         Ok(Ok(output)) => {
    ///             // Terminal outcome (success / failure / cancelled). To reconcile
    ///             // the full event history, read `events_after(process_id, 0)`.
    ///             let _ = output;
    ///             Ok(())
    ///         }
    ///         Ok(Err(err)) => Err(err), // e.g. unknown process, or an attach error
    ///         Err(_elapsed) => Ok(()),  // bound exceeded; retry or surface to the caller
    ///     }
    /// }
    /// ```
    pub async fn await_terminal(
        &self,
        process_id: &str,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        let record = match self.registry.get_process(process_id).await {
            Ok(Some(record)) => record,
            Ok(None) => {
                return Err(PluginError::Session(format!(
                    "unknown process `{process_id}`"
                )));
            }
            Err(PluginError::ProcessNoLongerRetained {
                terminal_label,
                pruned_at_ms,
            }) => {
                return Ok(ProcessAwaitOutput::NoLongerRetained {
                    terminal_label,
                    pruned_at_ms,
                });
            }
            Err(error) => return Err(error),
        };
        if let Some(output) = record.outcome.as_ref() {
            return Ok(output.clone());
        }
        // Refuse before parking: nothing can ever terminalize a caller-departed
        // row, so the wait would never resolve (FIG-1383).
        if record.status == crate::ProcessStatus::CallerDeparted {
            return Err(PluginError::ProcessCallerDeparted {
                process_id: process_id.to_string(),
            });
        }
        crate::runtime::process_worker::release_process_execution_permit_while(async {
            if let Some(attach) = self.attach.as_ref() {
                return attach.await_terminal(process_id).await;
            }
            self.awaiter.await_terminal(process_id).await
        })
        .await
    }

    /// Wait for the first event of `event_type` on `process_id` with a sequence
    /// greater than `after_sequence`, returning it once it appears.
    ///
    /// Like [`await_terminal`](Self::await_terminal) this rides the awaiter's
    /// hub-plus-backoff point reads rather than a store poll loop, and callers
    /// bound the wait with [`tokio::time::timeout`]. Historical events already
    /// past `after_sequence` resolve immediately. This waits on a *non-terminal*
    /// milestone; for completion use [`await_terminal`](Self::await_terminal).
    pub async fn await_event(
        &self,
        process_id: &str,
        event_type: &str,
        after_sequence: u64,
    ) -> Result<ProcessEvent, PluginError> {
        crate::runtime::process_worker::release_process_execution_permit_while(
            self.awaiter
                .await_event(process_id, event_type, after_sequence),
        )
        .await
    }

    /// Admit this owner's claimable pending processes, returning what the drive
    /// admitted. See [`ProcessRunHandle::claim_and_run_pending`] for the
    /// admission-not-completion contract.
    pub async fn claim_and_run_pending(
        &self,
        reason: &str,
    ) -> Result<ProcessAdmissionReport, PluginError> {
        match self.run_handle.claim_and_run_pending().await {
            Ok(report) => Ok(report),
            Err(err) => {
                tracing::warn!("process work drive ({reason}) failed: {err}");
                Err(err)
            }
        }
    }
}

/// One pass admitting the registry's pending (non-terminal) processes to
/// execution.
///
/// Implementations fence execution per row — the inline handle claims the
/// single-owner [`ProcessLease`](crate::ProcessLease), the Restate handle
/// coalesces by workflow key — so a concurrent drive on another owner skips a
/// row already being executed and a process runs exactly once.
#[async_trait::async_trait]
pub trait ProcessRunHandle: Send + Sync {
    /// Admit every pending process this owner can take, returning the per-row
    /// [`ProcessAdmissionReport`] for this pass. Idempotent: rows already being
    /// executed and terminal rows are skipped.
    ///
    /// **Admission, not completion.** Implementations return once the rows are
    /// handed to their executor; a returned `Ok` says the pass started work, not
    /// that any row reached a terminal state, and rows past the pass's own
    /// intake may still be admitted afterwards. Faults that strand an admitted
    /// row are reported as
    /// [`ProcessWorkerFault`](crate::runtime::ProcessWorkerFault)s on the wired
    /// [`ProcessEventSink`]; a terminal outcome is awaited through
    /// [`ProcessWorkDriver::await_terminal`].
    ///
    /// **`Err` may follow partial admission.** A pass that reads the worklist in
    /// pages can fail a later page after earlier pages already handed rows to
    /// the executor. Those rows are running; the error says this pass stopped
    /// short, never that nothing was admitted. Per-row failures are reported as
    /// deferred rows with a typed
    /// [`BackendError`](crate::runtime::ProcessRecoveryAttemptDisposition::BackendError)
    /// disposition rather than failing the call, so a partial report survives.
    async fn claim_and_run_pending(&self) -> Result<ProcessAdmissionReport, PluginError>;
}

/// Inline run handle: admits rows to the worker's own lease-protected sweep
/// in-process.
///
/// Delegates to [`DurableProcessWorker::drive_pending_processes`], the existing
/// `list_non_terminal_page -> claim lease -> run -> complete -> release` loop, so the
/// inline execution reuses the same coordination point as controller-owned execution.
pub struct InlineProcessRunHandle {
    worker: DurableProcessWorker,
}

impl InlineProcessRunHandle {
    pub fn new(worker: DurableProcessWorker) -> Self {
        Self { worker }
    }
}

#[async_trait::async_trait]
impl ProcessRunHandle for InlineProcessRunHandle {
    async fn claim_and_run_pending(&self) -> Result<ProcessAdmissionReport, PluginError> {
        self.worker.drive_pending_processes().await
    }
}
