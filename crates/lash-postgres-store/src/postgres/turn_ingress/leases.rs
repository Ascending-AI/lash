//! `session_execution_leases` statements only PostgreSQL issues.

lash_store_sql::statements! {
    /// `session_execution_leases` statements only PostgreSQL issues.
    pub(crate) struct SessionExecutionLeasePostgresStatements @ "session_execution_lease" {
        /// Session `?1`'s lease row, locked for the caller's transaction.
        ///
        /// Every mutation path takes this lock: check-then-act on this row is
        /// not atomic under READ COMMITTED, so two first claims could otherwise
        /// both observe no live lease and both win. SQLite serializes writers
        /// globally and needs no counterpart, and the one PostgreSQL read that
        /// must *not* take it — an operator's diagnostic poll — uses the shared
        /// unlocked statement so watching a lane cannot delay it.
        select_by_session_for_update = "SELECT lease_owner_id, lease_token, lease_fencing_token,
                    lease_claimed_at_ms, lease_expires_at_ms,
                    lease_owner_incarnation_id, lease_executor_id, lease_term_ms
             FROM session_execution_leases
             WHERE session_id = ?1
             FOR UPDATE";
    }
}
