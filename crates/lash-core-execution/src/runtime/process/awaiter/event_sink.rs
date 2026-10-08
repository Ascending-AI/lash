use crate::ProcessEvent;

/// Host-facing push of each appended process event.
///
/// A sink is a freshness feed, **never a source of truth.** The durable
/// event log ([`crate::ProcessRegistry::event_page`]) is the only complete
/// record; a sink lets a host observe appends promptly without polling.
///
/// # Contract
///
/// - **At least once, from a durable mark.** The decorator attached through
///   [`super::WatchedRegistry::add_event_sink`] calls [`emit`](Self::emit)
///   after a successful append through it, and after a durable process
///   commit that appended to the log on the node that owns the process
///   ([`super::WatchedRegistry::publish_committed`]). Every event committed
///   to a durable process's log reaches the sinks of the node that owns the
///   process when it is published: each owner records, with the next
///   transition it commits and with its end, how far its node's sinks were
///   handed the log (the process's publication mark), and a node that takes
///   the process over publishes from that mark, not from the log's end. An
///   event an owner committed and died before publishing is delivered by
///   its successor.
/// - **Each event once per node, in sequence order.** On one node every
///   path that emits a process's events shares one mark, which only moves
///   forward and outlives the process's terminal, so no sink of that node
///   hears an event twice.
/// - **Redelivery across nodes and crashes.** A node that takes a process
///   over, after its owner died or stopped or after the process waited
///   and was claimed elsewhere, hands again what its predecessor published
///   and did not record yet; an append through another node's decorator
///   also reaches that node's sinks. Each event carries a stable
///   identity, its `(process_id, sequence)`: the same pair is always the
///   same event, so a consumer that must act once per event dedupes on it.
/// - **Emission cannot fail the write.** `emit` returns `()`, so a sink can
///   never fail or roll back an append. The decorator awaits `emit` inline,
///   so implementors must hand real I/O off to a channel or background
///   task. An event a sink drops (a full channel, say) is not handed again:
///   the decorator counts it delivered. Consumers that need completeness
///   reconcile through `event_page`.
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
    /// Observe one appended process event, at least once; see the trait
    /// contract.
    ///
    /// Must be fast and non-blocking — offload I/O to a channel/task internally.
    async fn emit(&self, event: &ProcessEvent);
}
