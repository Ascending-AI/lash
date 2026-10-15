//! Observation work budgets, independent of retained data lifetimes.

/// Optional observation work budgets, independent of retained data lifetimes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObservationWorkLimits {
    /// Provisional language publications queued in the session class.
    pub language_session_events: usize,
    /// Bytes each language publication class admits, including in-flight work.
    pub language_bytes: usize,
    /// Process publication rounds queued, including the round in flight.
    pub language_process_rounds: usize,
    pub replay_expiry_batch: std::num::NonZeroUsize,
    pub publisher_batch: std::num::NonZeroUsize,
    pub process_snapshot_event_tail: usize,
    pub session_dedup_ids: usize,
    /// Delivered event identities one process feed remembers, to drop a
    /// redelivery.
    pub process_dedup_ids: usize,
    /// Durable event pages one process observation snapshot may read at the
    /// standard occurrence retention (64 per node). Larger configured or
    /// recorded cuts multiply this by ceil(cut / 64). One indexed omission
    /// lookup is reserved independently, even when this budget is zero.
    pub process_effect_fold_pages: usize,
    /// Events per durable page that read asks for.
    pub process_effect_fold_page_size: std::num::NonZeroUsize,
    /// Committed facts one process feed republishes to bridge its consumer
    /// to the durable process. A consumer further behind rebuilds from the
    /// durable read view instead, and the feed publishes nothing. Keep it a
    /// small fraction of the replay window: what a feed republishes pushes
    /// other observers' provisional evidence out of that window.
    pub process_reconcile_bridge_events: usize,
}
impl ObservationWorkLimits {
    /// Standard preset: expire 64 sessions per store call, publish 32 records
    /// per task poll, read 32 process tail events, remember 4096 session
    /// event IDs and 4096 process event IDs, fold a process's effects from
    /// at most 64 pages of 256 events and bridge a process feed across at
    /// most 256 committed facts. These exact values have no supporting
    /// workload measurements.
    pub const fn standard() -> Self {
        Self {
            language_session_events: 256,
            language_bytes: 4 * 1024 * 1024,
            language_process_rounds: 2,
            replay_expiry_batch: std::num::NonZeroUsize::MIN.saturating_add(63),
            publisher_batch: std::num::NonZeroUsize::MIN.saturating_add(31),
            process_snapshot_event_tail: 32,
            session_dedup_ids: 4096,
            process_dedup_ids: 4096,
            process_effect_fold_pages: 64,
            process_effect_fold_page_size: std::num::NonZeroUsize::MIN.saturating_add(255),
            process_reconcile_bridge_events: 256,
        }
    }
}
impl Default for ObservationWorkLimits {
    fn default() -> Self {
        Self::standard()
    }
}
