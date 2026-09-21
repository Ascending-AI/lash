//! `session_execution_leases`: one row per session, naming the lane's holder.
//!
//! The fencing token on this row is the generation every claim in the family
//! pins itself to, so a claim outlives its claimant exactly as long as this
//! row keeps naming the same generation (ADR 0029).

/// The table's unprefixed name.
pub const TABLE: &str = "session_execution_leases";

/// Every column a reader decodes, in the order both backends' decoders index.
pub const COLUMNS: &str = "lease_owner_id, lease_token, lease_fencing_token,
     lease_claimed_at_ms, lease_expires_at_ms,
     lease_owner_incarnation_id, lease_executor_id, lease_term_ms";

/// Every column an acquisition writes.
pub const INSERT_COLUMNS: &str = "session_id, lease_owner_id, lease_owner_incarnation_id,
     lease_executor_id, lease_token, lease_fencing_token,
     lease_claimed_at_ms, lease_expires_at_ms, lease_term_ms";

crate::statements! {
    /// `session_execution_leases` statements both backends issue verbatim.
    ///
    /// Every lifecycle write fences on owner, incarnation, executor and lease
    /// token and deliberately not on the generation, which is commit authority
    /// rather than lock lifecycle; the shared verdicts in
    /// `lash_core::store_backend_support` decide, and these predicates are
    /// their backstop (FIG-3381).
    pub struct SessionExecutionLeaseStatements @ "session_execution_lease" {
        /// Session `?1`'s lease row.
        select_by_session = "SELECT lease_owner_id, lease_token, lease_fencing_token,
                    lease_claimed_at_ms, lease_expires_at_ms,
                    lease_owner_incarnation_id, lease_executor_id, lease_term_ms
             FROM session_execution_leases
             WHERE session_id = ?1";

        /// Take session `?1`'s lane for owner `?2`/`?3`, executor `?4`, lease
        /// token `?5`, at generation `?6`, claimed `?7`, expiring `?8` after a
        /// term of `?9`.
        ///
        /// The generation is supplied, not computed in SQL: a takeover is only
        /// decided after the previous row is read, and the increment belongs
        /// beside that decision.
        acquire = "INSERT INTO session_execution_leases (
                 session_id, lease_owner_id, lease_owner_incarnation_id, lease_executor_id,
                 lease_token, lease_fencing_token,
                 lease_claimed_at_ms, lease_expires_at_ms, lease_term_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (session_id) DO UPDATE SET
                 lease_owner_id = excluded.lease_owner_id,
                 lease_owner_incarnation_id = excluded.lease_owner_incarnation_id,
                 lease_executor_id = excluded.lease_executor_id,
                 lease_token = excluded.lease_token,
                 lease_fencing_token = excluded.lease_fencing_token,
                 lease_claimed_at_ms = excluded.lease_claimed_at_ms,
                 lease_expires_at_ms = excluded.lease_expires_at_ms,
                 lease_term_ms = excluded.lease_term_ms";

        /// Re-enter session `?1`'s own live lease with a fresh token `?2`,
        /// keeping the claim instant `?3` and the generation the row already
        /// holds: nobody is displaced, so no generation advances.
        reenter = "UPDATE session_execution_leases
             SET lease_token = ?2,
                 lease_claimed_at_ms = ?3,
                 lease_expires_at_ms = ?4,
                 lease_term_ms = ?5
             WHERE session_id = ?1";

        /// Extend session `?1`'s lease to `?6` for a term of `?7`, if
        /// `?2`/`?3`/`?4`/`?5` still hold it.
        renew = "UPDATE session_execution_leases
             SET lease_expires_at_ms = ?6,
                 lease_term_ms = ?7
             WHERE session_id = ?1
               AND lease_owner_id = ?2
               AND lease_owner_incarnation_id = ?3
               AND lease_executor_id = ?4
               AND lease_token = ?5";

        /// Hand session `?1`'s lane back, if `?2`/`?3`/`?4`/`?5` still hold it.
        ///
        /// The generation column survives a release: it is the monotonic
        /// counter the next claimant increments, and zeroing it would let a
        /// stale claim's generation match a fresh lease's.
        release = "UPDATE session_execution_leases
             SET lease_owner_id = NULL,
                 lease_owner_incarnation_id = NULL,
                 lease_executor_id = NULL,
                 lease_token = NULL,
                 lease_claimed_at_ms = 0,
                 lease_term_ms = 0,
                 lease_expires_at_ms = 0
             WHERE session_id = ?1
               AND lease_owner_id = ?2
               AND lease_owner_incarnation_id = ?3
               AND lease_executor_id = ?4
               AND lease_token = ?5";

        /// Delete session `?1`'s lease row, on session deletion.
        delete_by_session = "DELETE FROM session_execution_leases WHERE session_id = ?1";
    }
}
