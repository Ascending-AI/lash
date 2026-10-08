//! Host controls for observers and recovery work.
use std::num::NonZeroUsize;
use std::time::Duration;

pub use lash_core::WorkCadencePolicy;
pub use lash_core::runtime::obligations::relay::{RelayPolicy, RelayPolicyError};
pub use lash_core::runtime::{
    CommitAdmissionPolicy, CommitAdmissionPolicyError, PollPacing, RuntimePacingPolicy,
};

/// Polling and buffering for facade observers. These values govern reads,
/// never actor execution or authority to finish deletion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObserverPacing {
    pub follow: PollPacing,
    pub admin: PollPacing,
    pub deletion: PollPacing,
    pub terminal: PollPacing,
    /// Activities buffered until the followed input has an admitted run.
    pub follow_buffer: NonZeroUsize,
    /// Activities queued for a send handle's event stream.
    pub send_channel: NonZeroUsize,
    /// Record/event-tail pairing attempts for a process snapshot.
    pub snapshot_read_attempts: NonZeroUsize,
}

impl ObserverPacing {
    /// Standard observer preset: follow/admin/deletion poll from 25ms to 1s,
    /// terminal reads from 20ms to 1s, buffer 4096 activities, queue 64 events,
    /// and attempt snapshot read repair twice.
    /// No workload measurement backs these values.
    pub const fn standard() -> Self {
        Self {
            follow: PollPacing::standard(),
            admin: PollPacing::standard(),
            deletion: PollPacing::standard(),
            terminal: PollPacing::terminal_standard(),
            follow_buffer: NonZeroUsize::MIN.saturating_add(4095),
            send_channel: NonZeroUsize::MIN.saturating_add(63),
            snapshot_read_attempts: NonZeroUsize::MIN.saturating_add(1),
        }
    }
}
impl Default for ObserverPacing {
    fn default() -> Self {
        Self::standard()
    }
}

/// Cadence and chunk size of the background artifact-cleanup due pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryPacing {
    interval: Duration,
    pub cleanup_page: NonZeroUsize,
}
impl RecoveryPacing {
    /// Standard recovery preset: one page of at most 256 cleanups every 10s.
    /// No workload measurement backs these values.
    pub const fn standard() -> Self {
        Self {
            interval: lash_core::runtime::obligations::RECOVERY_TICK,
            cleanup_page: NonZeroUsize::MIN.saturating_add(255),
        }
    }
    /// Require a positive background interval to avoid a busy loop.
    pub fn new(
        interval: Duration,
        cleanup_page: NonZeroUsize,
    ) -> Result<Self, lash_core::WorkCadenceError> {
        PollPacing::new(interval, interval)?;
        Ok(Self {
            interval,
            cleanup_page,
        })
    }
    pub const fn interval(self) -> Duration {
        self.interval
    }
}
impl Default for RecoveryPacing {
    fn default() -> Self {
        Self::standard()
    }
}
