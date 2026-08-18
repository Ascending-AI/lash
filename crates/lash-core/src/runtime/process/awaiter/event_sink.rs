use crate::ProcessEvent;
use crate::runtime::process_worker::ProcessWorkerFault;

/// Host-facing, best-effort push of each appended process event.
///
/// A sink is an optional freshness feed, **never a source of truth.** The
/// durable event log ([`crate::ProcessRegistry::events_after`]) is the only
/// complete record; a sink lets a host observe appends promptly without
/// polling, but it makes no delivery promise.
///
/// # Contract
///
/// - **Best-effort freshness, never truth.** The decorator installed by
///   [`super::watch_process_registry_with_sink`] calls [`emit`](Self::emit)
///   after a successful `append_event`, in that pod's per-process append order.
///   There is no buffering, no retry, and no delivery guarantee across pod
///   crashes or restarts: an event that was appended durably may never reach
///   the sink. Consumers that need completeness reconcile from `events_after`.
/// - **Worker faults ride the same surface.** A drive of pending processes
///   *admits* rows and returns; a fault that strands an admitted row afterwards
///   arrives through [`emit_worker_fault`](Self::emit_worker_fault). That method
///   is on this unconditional trait rather than on a feature-gated metrics
///   recorder, so the signal exists in every build; the default implementation
///   ignores it for sinks that only want events.
/// - **Emission cannot fail the write.** `emit` returns `()`, so a sink can
///   never fail or roll back an append. The decorator awaits `emit` inline, so
///   implementors must hand real I/O off to a channel or background task.
///
/// # Example: offload to a channel
///
/// ```
/// use lash_core::ProcessEvent;
/// use lash_core::runtime::{ProcessEventSink, ProcessWorkerFault};
/// use tokio::sync::mpsc;
///
/// struct ChannelSink {
///     events: mpsc::Sender<ProcessEvent>,
///     faults: mpsc::Sender<ProcessWorkerFault>,
/// }
///
/// #[async_trait::async_trait]
/// impl ProcessEventSink for ChannelSink {
///     async fn emit(&self, event: &ProcessEvent) {
///         // Non-blocking: drop on a full channel rather than slow the append.
///         let _ = self.events.try_send(event.clone());
///     }
///
///     // Faults are not events: a drive admits rows and returns, so this is the
///     // only surface left that can say a row was stranded afterwards. Taking
///     // the default here silently drops that.
///     async fn emit_worker_fault(&self, fault: &ProcessWorkerFault) {
///         let _ = self.faults.try_send(fault.clone());
///     }
/// }
/// ```
#[async_trait::async_trait]
pub trait ProcessEventSink: Send + Sync {
    /// Observe one appended process event. Best-effort; see the trait contract.
    ///
    /// Must be fast and non-blocking — offload I/O to a channel/task internally.
    async fn emit(&self, event: &ProcessEvent);

    /// Observe one process worker fault: a row an admission pass admitted but a
    /// backend or execution failure left non-terminal, or a worklist scan that
    /// stopped short of the whole queue.
    ///
    /// Same discipline as [`emit`](Self::emit) — fast, non-blocking,
    /// best-effort, and it can never fail the drive. The default ignores the
    /// fault, so an event-only sink is unaffected.
    async fn emit_worker_fault(&self, fault: &ProcessWorkerFault) {
        let _ = fault;
    }
}
