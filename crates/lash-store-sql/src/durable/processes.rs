//! Process actors, cancel, terminal and cascade: the neutral statements of the `processes` domain (I0, FIG-5194).
//!
//! Owned by L6 (FIG-5175): its statements, and its table's DDL in each
//! dialect's `durable` module, are that lane's. This file and the matching
//! file in each dialect are the only places its SQL may appear
//! (`scripts/check-durable-sql.py`).
//!
//! The process actor's columns live on the registry's `processes` row: the
//! state revision, the driver's state, the cascade cursor, the epoch that
//! last wrote them and the publication mark. A live process is `running` or
//! `waiting`.

crate::statements! {
    /// The process actor's statements both backends issue verbatim.
    pub struct ProcessActorStatements @ "durable_process" {
        /// Process `?1`'s actor columns, cancel request and status.
        row = "SELECT state_rev, driver_json, cancel_requested_at_ms, status, cascade_cursor,
                    written_epoch, published_event_sequence
             FROM processes WHERE process_id = ?1";

        /// Move live process `?1` from state revision `?2` to the next with
        /// driver state `?3`, written at epoch `?4`. No row when the
        /// revision moved or the process ended.
        advance = "UPDATE processes
             SET state_rev = state_rev + 1, driver_json = ?3, written_epoch = ?4
             WHERE process_id = ?1 AND state_rev = ?2 AND status IN ('running', 'waiting')
             RETURNING state_rev";

        /// Record component state/config postimages at an owner cut, leaving
        /// the engine revision unchanged (including before Started).
        component_formats = "UPDATE processes
             SET driver_json = ?3, written_epoch = ?4
             WHERE process_id = ?1 AND state_rev = ?2 AND status IN ('running', 'waiting')
             RETURNING state_rev";

        /// Move process `?1`'s publication mark forward to `?2`; an older
        /// mark leaves it as it is.
        publish = "UPDATE processes SET published_event_sequence = ?2
             WHERE process_id = ?1 AND published_event_sequence < ?2";

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

        /// Up to `?5` live processes, by id, in the `Until` subtree of the
        /// scope of kind `?1` and index id `?2` and, when they are set, of
        /// every turn scope whose index id starts with `?3` and every
        /// session-operation scope whose index id starts with `?4`: the
        /// scopes inside a session. The walk follows `Until` edges through
        /// every process row, ended or not, so a live process below an
        /// ended one is found. A process's scope id is its identity
        /// encoding, `process:<byte length>:<id>`; a process id is ASCII.
        live_descendants = "WITH RECURSIVE subtree AS (
                 SELECT process_id, status FROM processes
                 WHERE lifetime = 'until'
                   AND ((lifetime_scope_kind = ?1 AND lifetime_scope_id = ?2)
                     OR (lifetime_scope_kind = 'turn'
                         AND substr(lifetime_scope_id, 1, length(CAST(?3 AS TEXT)))
                             = CAST(?3 AS TEXT))
                     OR (lifetime_scope_kind = 'session_operation'
                         AND substr(lifetime_scope_id, 1, length(CAST(?4 AS TEXT)))
                             = CAST(?4 AS TEXT)))
                 UNION
                 SELECT child.process_id, child.status
                 FROM subtree parent
                 JOIN processes child
                   ON child.lifetime = 'until' AND child.lifetime_scope_kind = 'process'
                  AND child.lifetime_scope_id = 'process:'
                      || CAST(length(parent.process_id) AS TEXT) || ':' || parent.process_id
             )
             SELECT process_id FROM subtree
             WHERE status IN ('running', 'waiting')
             ORDER BY process_id
             LIMIT ?5";
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

        /// Whether actor `?1` has mail of kind `?3` or `?4` (a cancel or a
        /// session close) past mailbox position `?2`: a stop that reached
        /// it after an owner's read there, which its park must not strand.
        stop_mail_after = "SELECT 1 FROM actor_mail
             WHERE actor_key = ?1 AND seq > ?2 AND kind IN (?3, ?4)
             LIMIT 1";

        /// Release actor `?1` parked, its epoch bumped.
        park = "UPDATE actors
             SET state = 'parked', ready_at_ms = NULL, next_due_ms = NULL, epoch = epoch + 1,
                 owner_node = NULL, owner_boot = NULL
             WHERE actor_key = ?1
             RETURNING state";

        /// Set actor `?1`'s current park to `?2`; clearing a park also
        /// resets its failed activations, under the caller's actor fence.
        set_park = "UPDATE actors
             SET failed_activations = CASE WHEN CAST(?2 AS TEXT) IS NULL AND park_json IS NOT NULL
                                           THEN 0 ELSE failed_activations END,
                 park_json = ?2
             WHERE actor_key = ?1";

        /// Clear actor `?1`'s park and failed activations. No row when it
        /// had no park.
        clear_park = "UPDATE actors SET park_json = NULL, failed_activations = 0
             WHERE actor_key = ?1 AND park_json IS NOT NULL
             RETURNING actor_key";

        /// Actor `?1`'s park and failed activations.
        park_of = "SELECT park_json, failed_activations FROM actors WHERE actor_key = ?1";

        /// Every parked session actor's key and park, by key: the candidates
        /// of a boot's redrive (FIG-6310).
        parked_sessions = "SELECT actor_key, park_json FROM actors
             WHERE state = 'parked' AND kind = 'session'
             ORDER BY actor_key";

        /// Redrive actor `?1` at `?2` while it is still parked with park
        /// `?3`, as a control wake and a redrive together do: take the next
        /// mailbox position, make it ready, and clear its park and failed
        /// activations. No row when that park no longer stands.
        redrive_parked = "UPDATE actors
             SET mail_seq = mail_seq + 1, state = 'ready', ready_at_ms = ?2,
                 park_json = NULL, failed_activations = 0
             WHERE actor_key = ?1 AND state = 'parked' AND park_json = ?3
             RETURNING actor_key";
    }
}
