use super::*;

/// Shared lease-fencing predicate for process leases.
///
/// A lease holder may keep operating only while the *currently stored* lease
/// still carries the holder's `lease_token` and has not expired. The
/// `current` argument is whatever lease (if any) is presently persisted for
/// the key; `expected_token` / `now` are the holder's claimed token and the
/// current clock. Returns `true` when the holder still owns a live lease.
///
/// This is the one place the "stored token matches and is unexpired" rule
/// lives for process leasing.
pub(crate) fn guard_lease(current: Option<&ProcessLease>, expected_token: &str, now: u64) -> bool {
    match current {
        Some(current) => current.lease_token == expected_token && current.expires_at_epoch_ms > now,
        None => false,
    }
}
