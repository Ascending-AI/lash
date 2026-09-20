//! `process_leases`: one row per process, carrying its execution lease.
//!
//! Every statement here is a fencing statement, and every one of them is
//! relocated from its backend with its predicate unchanged (FIG-3384). The
//! fencing decision itself moves into backend-neutral code under FIG-3381;
//! until then the predicate is exactly what each backend issued.
//!
//! [`LeaseStatements::release_claimed`] and
//! [`LeaseStatements::release_completed`] fence differently — the second also
//! compares the fencing token and clears the owner incarnation. That
//! difference is an open defect, FIG-3388, and is preserved here rather than
//! reconciled.

/// The table's unprefixed name.
pub const TABLE: &str = "process_leases";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "process_id, lease_owner_id, lease_owner_incarnation_id,
                lease_token, lease_fencing_token,
                lease_claimed_at_ms, lease_expires_at_ms";

/// One process's lease, as the lease projection reads it.
///
/// Every column but the key, which the caller already holds: a lease decision
/// compares the owner, the token, the fencing token and both instants, so
/// nothing here is spare.
pub const LEASE_COLUMNS: &str = "lease_owner_id, lease_token, lease_fencing_token,
                    lease_claimed_at_ms, lease_expires_at_ms,
                    lease_owner_incarnation_id";

/// [`LEASE_COLUMNS`] keyed by process, for the batch read that reports leases
/// for many processes at once and has to say which row is whose.
pub const KEYED_LEASE_COLUMNS: &str =
    "process_id, lease_owner_id, lease_token, lease_fencing_token,
                     lease_claimed_at_ms, lease_expires_at_ms, lease_owner_incarnation_id";

crate::statements! {
    /// `process_leases` statements both backends issue verbatim.
    ///
    /// Relocated unchanged; see the module docs for the FIG-3381 and FIG-3388
    /// boundaries.
    pub struct LeaseStatements @ "process_lease" {
        /// Extend `?1`'s lease to `?2` with no fence.
        ///
        /// Unfenced on both backends by design: the caller acquired the lease
        /// in the same transaction and is extending its own.
        extend_unfenced = "UPDATE process_leases SET lease_expires_at_ms = ?2 WHERE process_id = ?1";

        /// Renew `?1`'s lease to `?2` only while `?3` still holds it.
        renew_fenced = "UPDATE process_leases SET lease_expires_at_ms = ?2
                 WHERE process_id = ?1 AND lease_token = ?3";

        /// Release `?1`'s lease held under token `?2`.
        ///
        /// Fences on the lease token alone and leaves `lease_owner_incarnation_id`
        /// set. FIG-3388: this and [`LeaseStatements::release_completed`] fence
        /// differently, and reconciling them is that ticket's, not this one's.
        release_claimed = "UPDATE process_leases
                 SET lease_owner_id = NULL, lease_token = NULL,
                     lease_claimed_at_ms = 0, lease_expires_at_ms = 0
                 WHERE process_id = ?1 AND lease_token = ?2";

        /// Release `?1`'s lease held under token `?2` at fencing token `?3`,
        /// on the completion path.
        ///
        /// Fences on the token *and* the fencing token, and clears the owner
        /// incarnation as well. See [`LeaseStatements::release_claimed`].
        release_completed = "UPDATE process_leases
             SET lease_owner_id = NULL,
                 lease_owner_incarnation_id = NULL,
                 lease_token = NULL,
                 lease_claimed_at_ms = 0,
                 lease_expires_at_ms = 0
             WHERE process_id = ?1 AND lease_token = ?2 AND lease_fencing_token = ?3";
    }
}
