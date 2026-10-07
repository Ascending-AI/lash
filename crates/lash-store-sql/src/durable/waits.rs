//! Waits, keyed promises and timers: the neutral statements of the `waits` domain (I0, FIG-5194).
//!
//! Owned by L5 (FIG-5173): its statements, and its table's DDL in each
//! dialect's `durable` module, are that lane's. This file and the matching
//! file in each dialect are the only places its SQL may appear
//! (`scripts/check-durable-sql.py`).
//!
//! A wait's `deadline_ms`, `kind`, `owner_actor` and `key_version` are
//! written once, by `pin`; every other statement changes a row only while it
//! is `pending`, so the first resolution wins.

/// The table's unprefixed name.
pub const TABLE: &str = "waits";

crate::statements! {
    /// `waits` statements both backends issue verbatim.
    pub struct WaitStatements @ "durable_wait" {
        /// Pin wait `?1` of owner `?2` in scope `?3`: kind `?4`, host
        /// resolvable `?5`, target process `?6`, deadline `?7`, key version
        /// `?8`, minted at epoch `?9`.
        pin = "INSERT INTO waits
                 (wait_id, owner_actor, owner_scope, kind, host_resolvable, target_process,
                  state, deadline_ms, key_version, created_epoch)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8, ?9)";

        /// Settle owner `?2`'s pending wait `?1` whose deadline passed by
        /// `?4`: a timer resolves with digest `?3`, any other kind times out.
        due = "UPDATE waits
             SET state = CASE WHEN kind = 'timer' THEN 'resolved' ELSE 'timed_out' END,
                 resolution_digest = CASE WHEN kind = 'timer' THEN CAST(?3 AS TEXT) ELSE NULL END,
                 resolved_at_ms = ?4
             WHERE wait_id = ?1 AND owner_actor = ?2 AND state = 'pending'
               AND deadline_ms <= ?4";

        /// Revoke every pending wait of scope `?1` at `?2`, returning each
        /// owner.
        revoke_scope = "UPDATE waits SET state = 'revoked', resolved_at_ms = ?2
             WHERE owner_scope = ?1 AND state = 'pending'
             RETURNING owner_actor";

        /// Resolve every pending process-terminal wait on process `?1` with
        /// digest `?2` and resolution `?3` at `?4`, returning each owner.
        resolve_process_terminal = "UPDATE waits
             SET state = 'resolved', resolution_digest = ?2, resolution_ref = ?3,
                 resolved_at_ms = ?4
             WHERE target_process = ?1 AND kind = 'process_terminal' AND state = 'pending'
             RETURNING owner_actor";

        /// Resolve every pending child-session wait on process `?1` with
        /// digest `?2` and resolution `?3` at `?4`, returning each owner.
        resolve_child_session = "UPDATE waits
             SET state = 'resolved', resolution_digest = ?2, resolution_ref = ?3,
                 resolved_at_ms = ?4
             WHERE target_process = ?1 AND kind = 'child_session' AND state = 'pending'
             RETURNING owner_actor";

        /// Resolve pending wait `?1` with digest `?2` and resolution `?3` at
        /// `?4`; no row when it is not pending.
        resolve = "UPDATE waits
             SET state = 'resolved', resolution_digest = ?2, resolution_ref = ?3,
                 resolved_at_ms = ?4
             WHERE wait_id = ?1 AND state = 'pending'
             RETURNING owner_actor";

        /// Owner `?1`'s pending waits.
        pending = "SELECT wait_id, owner_actor, owner_scope, kind, target_process, state,
                    deadline_ms, resolution_digest, resolution_ref, resolved_at_ms,
                    key_version, created_epoch
             FROM waits WHERE owner_actor = ?1 AND state = 'pending'
             ORDER BY wait_id";

        /// Wait `?1`.
        one = "SELECT wait_id, owner_actor, owner_scope, kind, target_process, state,
                    deadline_ms, resolution_digest, resolution_ref, resolved_at_ms,
                    key_version, created_epoch
             FROM waits WHERE wait_id = ?1";
    }
}
