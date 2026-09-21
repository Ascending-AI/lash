//! `process_leases`: one row per process, carrying its execution lease.
//!
//! Every statement here is a fencing statement, and every one of them is
//! relocated from its backend with its predicate unchanged (FIG-3384). The
//! fencing decision itself lives in backend-neutral code under FIG-3381; the
//! predicate on each write is the backstop for that verdict, not the decision.

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
    /// Relocated unchanged; see the module docs for the FIG-3381 boundary.
    pub struct LeaseStatements @ "process_lease" {
        /// Extend `?1`'s lease to `?2` with no fence.
        ///
        /// Unfenced on both backends by design: the caller acquired the lease
        /// in the same transaction and is extending its own.
        extend_unfenced = "UPDATE process_leases SET lease_expires_at_ms = ?2 WHERE process_id = ?1";

        /// Renew `?1`'s lease to `?2` only while `?3` still holds it.
        renew_fenced = "UPDATE process_leases SET lease_expires_at_ms = ?2
                 WHERE process_id = ?1 AND lease_token = ?3";

        /// Release `?1`'s lease held under token `?2` at fencing token `?3`.
        ///
        /// The one release statement both release paths issue (FIG-3388): the
        /// shared `process_lease_verdict` is the decision and this predicate is
        /// its backstop. The `lease_fencing_token` conjunct is strictly
        /// redundant — the lease token's preimage already commits to the
        /// generation (`the_lease_token_preimage_is_pinned`) — and stays as
        /// defence in depth, matching the FIG-3381 backstop ruling.
        ///
        /// A released row keeps only its retained fencing token: every holder
        /// column clears so a stale incarnation cannot outlive the release.
        release = "UPDATE process_leases
             SET lease_owner_id = NULL,
                 lease_owner_incarnation_id = NULL,
                 lease_token = NULL,
                 lease_claimed_at_ms = 0,
                 lease_expires_at_ms = 0
             WHERE process_id = ?1 AND lease_token = ?2 AND lease_fencing_token = ?3";
    }
}
