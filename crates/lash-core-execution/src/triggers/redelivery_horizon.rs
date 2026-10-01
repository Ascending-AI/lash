//! The redelivery horizon of a reclaimed trigger occurrence (FIG-4573).

/// How long a reclaimed occurrence's tombstone refuses its identity: the
/// redelivery horizon (FIG-4573), seven days.
///
/// A redelivery with no journal to answer it is a resubmission: the
/// tool-intent ingress runs again for whoever presents the admitted handle,
/// on a new invocation, and a host that journals nothing runs the emission
/// again on every retry. Lash cannot observe when the last one arrives.
/// Restate owns journal and idempotency retention (ADR 0025), and those bound
/// when a duplicate stops attaching to its first invocation, not when
/// duplicates stop. So the horizon is Lash's contract instead of a
/// measurement: an emission is redelivered no later than this after its
/// occurrence was reclaimed, and an identity presented later is a new
/// emission. It is a constant and not a host lever, so no reclaim cutoff or
/// store configuration can shorten the guard.
pub const TRIGGER_OCCURRENCE_REDELIVERY_HORIZON_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// The instant before which a reclaim pass at `cutoff_epoch_ms`, run at
/// `now_epoch_ms`, compacts occurrence tombstones.
///
/// The earlier of the host's cutoff and the redelivery horizon's far edge: a
/// cutoff can keep a tombstone longer than
/// [`TRIGGER_OCCURRENCE_REDELIVERY_HORIZON_MS`] and never shorter. `u64::MAX`
/// reclaims every armed occurrence and still compacts no tombstone inside the
/// horizon, the one the pass itself writes included.
pub fn trigger_occurrence_tombstone_compaction_bound(
    cutoff_epoch_ms: u64,
    now_epoch_ms: u64,
) -> u64 {
    cutoff_epoch_ms.min(now_epoch_ms.saturating_sub(TRIGGER_OCCURRENCE_REDELIVERY_HORIZON_MS))
}
