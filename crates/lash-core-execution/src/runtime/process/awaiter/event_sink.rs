use crate::ProcessEvent;

/// Host-facing, best-effort push of each appended process event.
///
/// A sink is an optional freshness feed, **never a source of truth.** The
/// durable event log ([`crate::ProcessRegistry::event_page`]) is the only
/// complete record; a sink lets a host observe appends promptly without
/// polling, but it makes no delivery promise.
///
/// # Contract
///
/// - **Best-effort freshness, never truth.** The decorator installed by
///   [`super::watch_process_registry_with_sink`] or attached through
///   [`super::WatchedRegistry::add_event_sink`] calls [`emit`](Self::emit)
///   after a successful `append_event`, in that pod's per-process append order.
///   There is no buffering, no retry, and no delivery guarantee across pod
///   crashes or restarts: an event that was appended durably may never reach
///   the sink. Consumers that need completeness reconcile through `event_page`.
/// - **Emission cannot fail the write.** `emit` returns `()`, so a sink can
///   never fail or roll back an append. The decorator awaits `emit` inline, so
///   implementors must hand real I/O off to a channel or background task.
///
/// # Example: offload to a channel
///
/// ```
/// use lash_core::ProcessEvent;
/// use lash_core::runtime::ProcessEventSink;
/// use tokio::sync::mpsc;
///
/// struct ChannelSink {
///     events: mpsc::Sender<ProcessEvent>,
/// }
///
/// #[async_trait::async_trait]
/// impl ProcessEventSink for ChannelSink {
///     async fn emit(&self, event: &ProcessEvent) {
///         // Non-blocking: drop on a full channel rather than slow the append.
///         let _ = self.events.try_send(event.clone());
///     }
/// }
/// ```
#[async_trait::async_trait]
pub trait ProcessEventSink: Send + Sync {
    /// Observe one appended process event. Best-effort; see the trait contract.
    ///
    /// Must be fast and non-blocking — offload I/O to a channel/task internally.
    async fn emit(&self, event: &ProcessEvent);
}
