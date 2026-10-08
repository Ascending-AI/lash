//! Configurable SQLite coordination and working-memory policies.

use std::num::NonZeroUsize;
use std::time::Duration;

/// Operational policy applied by a store's connections, listeners and transactions.
/// File handles sharing a checkpoint worker use the smallest checkpoint interval,
/// busy timeout and page threshold requested during that worker's lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqliteOperationalSettings {
    /// Statements cached per writer/reader/listener; zero disables caching.
    pub statement_cache_capacity: usize,
    /// Read-only readers, release probes and wake listeners' busy timeout.
    pub readonly_busy_timeout: Duration,
    /// Read-only page cache; negative KiB or positive pages, as SQLite defines it.
    pub readonly_cache_size: i32,
    /// Background checkpoint pacing (must be positive).
    pub checkpoint_interval: Duration,
    /// Checkpoint connection's busy timeout; zero refuses contention immediately.
    pub checkpoint_busy_timeout: Duration,
    /// First WAL-conversion retry wait (must be positive).
    pub wal_retry_initial: Duration,
    /// Largest WAL-conversion retry wait (at least the initial wait).
    pub wal_retry_max: Duration,
    /// Attempts to take a liveness lock whose path is deleted under it.
    pub liveness_lock_attempts: NonZeroUsize,
    /// Cross-process wake polling (must be positive).
    pub wake_poll: Duration,
    /// Lifetime of disposable wake hints; correctness also uses the claim poll.
    pub wake_retention: Duration,
    /// Budget to acquire a listener's boot lock.
    pub wake_lock_timeout: Duration,
    /// Poll interval for a busy boot lock (must be positive).
    pub lock_poll: Duration,
    /// First wake-listener reopen wait (must be positive).
    pub wake_reopen_initial: Duration,
    /// Largest wake-listener reopen wait (at least the initial wait).
    pub wake_reopen_max: Duration,
    /// Poll interval for migration/finalize ownership and a pinned WAL (positive).
    pub migration_poll: Duration,
    /// Checkpoint references decoded or queried in one working batch.
    pub checkpoint_ref_chunk: NonZeroUsize,
    /// Node ids queried in one commit-planning batch.
    pub occupied_node_chunk: NonZeroUsize,
    /// Graph rows inserted in one statement.
    pub graph_insert_chunk: NonZeroUsize,
    /// Process events hydrated and released per page.
    pub process_event_release_page: NonZeroUsize,
}

impl Default for SqliteOperationalSettings {
    fn default() -> Self {
        Self::standard()
    }
}

impl SqliteOperationalSettings {
    /// Standard production preset: 256 cached statements (FIG-3975 found
    /// the previous 16-entry cache evicted the store's statement mix).
    /// Readonly: 1 s busy / -500 KiB cache. Checkpoint: 100 ms / 20 ms busy.
    /// WAL retry: 1–50 ms. Locks: 8 attempts / 10 ms poll. Wakes: 25 ms poll,
    /// 10 s retention, 5 s lock wait, 25 ms–1 s reopen. Migration poll: 10 ms.
    /// Working chunks: checkpoint refs and occupied ids 16,384, inserts 512,
    /// released events 256. Ref chunks bound requests around 1 MiB; the
    /// remaining numerical choices have no workload measurements behind them.
    pub fn standard() -> Self {
        let count = |n| NonZeroUsize::new(n).unwrap_or(NonZeroUsize::MIN);
        Self {
            statement_cache_capacity: 256,
            readonly_busy_timeout: Duration::from_secs(1),
            readonly_cache_size: -500,
            checkpoint_interval: Duration::from_millis(100),
            checkpoint_busy_timeout: Duration::from_millis(20),
            wal_retry_initial: Duration::from_millis(1),
            wal_retry_max: Duration::from_millis(50),
            liveness_lock_attempts: count(8),
            wake_poll: Duration::from_millis(25),
            wake_retention: Duration::from_secs(10),
            wake_lock_timeout: Duration::from_secs(5),
            lock_poll: Duration::from_millis(10),
            wake_reopen_initial: Duration::from_millis(25),
            wake_reopen_max: Duration::from_secs(1),
            migration_poll: Duration::from_millis(10),
            checkpoint_ref_chunk: count(16_384),
            occupied_node_chunk: count(16_384),
            graph_insert_chunk: count(512),
            process_event_release_page: count(256),
        }
    }

    /// Development: 64 cached statements, readonly -250 KiB cache, ref and
    /// occupied-id chunks 256, graph inserts and event pages 32. Other values
    /// are standard. The smaller capacities are unmeasured local conveniences.
    pub fn development() -> Self {
        Self {
            statement_cache_capacity: 64,
            readonly_cache_size: -250,
            checkpoint_ref_chunk: NonZeroUsize::new(256).unwrap_or(NonZeroUsize::MIN),
            occupied_node_chunk: NonZeroUsize::new(256).unwrap_or(NonZeroUsize::MIN),
            graph_insert_chunk: NonZeroUsize::new(32).unwrap_or(NonZeroUsize::MIN),
            process_event_release_page: NonZeroUsize::new(32).unwrap_or(NonZeroUsize::MIN),
            ..Self::standard()
        }
    }

    pub(crate) fn validate(self) -> rusqlite::Result<()> {
        for (field, value) in [
            ("checkpoint_interval", self.checkpoint_interval),
            ("wal_retry_initial", self.wal_retry_initial),
            ("wal_retry_max", self.wal_retry_max),
            ("wake_poll", self.wake_poll),
            ("lock_poll", self.lock_poll),
            ("wake_reopen_initial", self.wake_reopen_initial),
            ("wake_reopen_max", self.wake_reopen_max),
            ("migration_poll", self.migration_poll),
        ] {
            if value.is_zero() {
                return Err(rusqlite::Error::InvalidParameterName(format!(
                    "{field} must be positive"
                )));
            }
        }
        if self.wal_retry_initial > self.wal_retry_max
            || self.wake_reopen_initial > self.wake_reopen_max
        {
            return Err(rusqlite::Error::InvalidParameterName(
                "retry initial wait exceeds maximum".into(),
            ));
        }
        Ok(())
    }
}
