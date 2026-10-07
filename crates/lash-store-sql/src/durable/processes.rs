//! Process actors, cancel, terminal and cascade: the neutral statements of the `processes` domain (I0, FIG-5194).
//!
//! Owned by L6 (FIG-5175): its statements, and its table's DDL in each
//! dialect's `durable` module, are that lane's. This file and the matching
//! file in each dialect are the only places its SQL may appear
//! (`scripts/check-durable-sql.py`).
//!
//! The process actor's columns live on the registry's `processes` row: the
//! state revision, the driver's state, the cascade cursor and the epoch that
//! last wrote them. A live process is `running` or `waiting`.

crate::statements! {
    /// The process actor's statements both backends issue verbatim.
    pub struct ProcessActorStatements @ "durable_process" {
        /// Process `?1`'s actor columns, cancel request and status.
        row = "SELECT state_rev, driver_json, cancel_requested_at_ms, status, cascade_cursor,
                    written_epoch
             FROM processes WHERE process_id = ?1";

        /// Move live process `?1` from state revision `?2` to the next with
        /// driver state `?3`, written at epoch `?4`. No row when the
        /// revision moved or the process ended.
        advance = "UPDATE processes
             SET state_rev = state_rev + 1, driver_json = ?3, written_epoch = ?4
             WHERE process_id = ?1 AND state_rev = ?2 AND status IN ('running', 'waiting')
             RETURNING state_rev";

        /// Set process `?1`'s cascade cursor to `?2` (`NULL` once done),
        /// written at epoch `?3`.
        set_cursor = "UPDATE processes SET cascade_cursor = ?2, written_epoch = ?3
             WHERE process_id = ?1";

        /// Up to `?4` live processes `Until` the scope of kind `?1` and
        /// index id `?2` whose cancel was not yet requested, after process
        /// id `?3`, by id.
        pending_children = "SELECT process_id FROM processes
             WHERE lifetime = 'until' AND lifetime_scope_kind = ?1 AND lifetime_scope_id = ?2
               AND cancel_requested_at_ms IS NULL AND status IN ('running', 'waiting')
               AND process_id > ?3
             ORDER BY process_id
             LIMIT ?4";

        /// Up to `?4` live processes `Until` the scope of kind `?1` and
        /// index id `?2`, after process id `?3`, by id.
        live_children = "SELECT process_id FROM processes
             WHERE lifetime = 'until' AND lifetime_scope_kind = ?1 AND lifetime_scope_id = ?2
               AND status IN ('running', 'waiting')
               AND process_id > ?3
             ORDER BY process_id
             LIMIT ?4";
    }
}

crate::statements! {
    /// The actor statements of parks, control wakes and crash loops, both
    /// backends verbatim (L6 owns these `actors` columns).
    pub struct ActorParkStatements @ "durable_actor_park" {
        /// Wake actor `?1` at `?2` as a cancel or a redrive does: take the
        /// next mailbox position and make it ready when it was idle,
        /// waiting or parked. No row when it is unknown or terminal.
        control_wake = "UPDATE actors
             SET mail_seq = mail_seq + 1,
                 state = CASE WHEN state IN ('idle', 'waiting', 'parked') THEN 'ready'
                              ELSE state END,
                 ready_at_ms = CASE WHEN state IN ('idle', 'waiting', 'parked')
                                    THEN CAST(?2 AS BIGINT) ELSE ready_at_ms END,
                 next_due_ms = CASE WHEN state IN ('idle', 'waiting', 'parked') THEN NULL
                                    ELSE next_due_ms END
             WHERE actor_key = ?1 AND state <> 'terminal'
             RETURNING mail_seq, state, owner_node, owner_boot";

        /// Whether actor `?1` has an unacknowledged mail of kind `?2`.
        pending_mail_of_kind = "SELECT 1 FROM actor_mail m, actors a
             WHERE a.actor_key = ?1 AND m.actor_key = ?1 AND m.seq > a.acked_seq
               AND m.kind = ?2
             LIMIT 1";

        /// Release actor `?1` parked, its epoch bumped.
        park = "UPDATE actors
             SET state = 'parked', ready_at_ms = NULL, next_due_ms = NULL, epoch = epoch + 1,
                 owner_node = NULL, owner_boot = NULL
             WHERE actor_key = ?1
             RETURNING state";

        /// Record actor `?1`'s park, `?2`.
        set_park = "UPDATE actors SET park_json = ?2 WHERE actor_key = ?1";

        /// Clear actor `?1`'s park and failed activations. No row when it
        /// had no park.
        clear_park = "UPDATE actors SET park_json = NULL, failed_activations = 0
             WHERE actor_key = ?1 AND park_json IS NOT NULL
             RETURNING actor_key";

        /// Actor `?1`'s park and failed activations.
        park_of = "SELECT park_json, failed_activations FROM actors WHERE actor_key = ?1";
    }
}
