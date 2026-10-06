//! The durability engine's tables (ruling #74): `nodes`, `actors` and
//! `actor_mail`, and one submodule per domain the runtime lanes add to the
//! fenced commit (I0, FIG-5194).
//!
//! This module, its domain submodules and the matching modules of each
//! backend are the only places their SQL may appear
//! (`scripts/check-durable-sql.py`). Every statement both backends
//! can issue verbatim is here; a backend's own module holds the few that
//! fork (row locks, the claim, array binding) and the backend's DDL.
//!
//! Instants are always bound (`?`): the backend reads its clock once per
//! transaction and every statement in it uses that one reading.
//!
//! # The rules the statements keep
//!
//! - An actor's `epoch` changes only by claim, reap and release, and each
//!   bumps it. An owner write is applied only after [`ActorStatements::fence`]
//!   matched the owner's epoch on an owned row, as the transaction's first
//!   engine statement.
//! - Every append and every wake takes the next `mail_seq`; the owner's
//!   acknowledgement moves `acked_seq`. An actor has mail exactly when
//!   `mail_seq > acked_seq`, so a wake cannot be lost between a read and an
//!   acknowledgement.
//! - A release leaves an actor ready when it still has mail, unless it ends.

pub mod park_events;
pub mod processes;
pub mod run_records;
pub mod session_close;
pub mod snapshots;
pub mod turns;
pub mod waits;

/// The node table's unprefixed name.
pub const NODES_TABLE: &str = "nodes";

/// The actor table's unprefixed name.
pub const ACTORS_TABLE: &str = "actors";

/// The mailbox table's unprefixed name.
pub const MAIL_TABLE: &str = "actor_mail";

crate::statements! {
    /// `nodes` statements both backends issue verbatim.
    pub struct NodeStatements @ "durable_node" {
        /// Register boot `?2` of node `?1`, decoding the format sets in JSON
        /// array `?3`, at `?4` with its lease expiring at `?5`.
        insert = "INSERT INTO nodes
                 (node_id, boot_id, formats_json, registered_at_ms, heartbeat_expires_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)";

        /// Delete every boot of node `?1`, returning each.
        delete_boots = "DELETE FROM nodes WHERE node_id = ?1 RETURNING boot_id";

        /// Delete boot `?2` of node `?1`; no row when it holds no lease.
        delete_boot = "DELETE FROM nodes WHERE node_id = ?1 AND boot_id = ?2 RETURNING boot_id";

        /// Delete every node whose lease expired before `?1`, returning each.
        delete_expired = "DELETE FROM nodes WHERE heartbeat_expires_at_ms < ?1
             RETURNING node_id, boot_id";

        /// Renew boot `?2` of node `?1` to `?3`; no row when it holds no lease.
        renew = "UPDATE nodes SET heartbeat_expires_at_ms = ?3
             WHERE node_id = ?1 AND boot_id = ?2
             RETURNING heartbeat_expires_at_ms";
    }
}

crate::statements! {
    /// `actors` statements both backends issue verbatim.
    pub struct ActorStatements @ "durable_actor" {
        /// Create actor `?1` of kind `?2` in format set `?3`, ready at `?4`.
        /// No row when it exists.
        create = "INSERT INTO actors
                 (actor_key, kind, state, epoch, ready_at_ms, next_due_ms, formats,
                  state_revision, mail_seq, acked_seq, created_at_ms)
             VALUES (?1, ?2, 'ready', 0, ?4, NULL, ?3, 0, 0, 0, ?4)
             ON CONFLICT (actor_key) DO NOTHING
             RETURNING actor_key";

        /// The owner fence: bump owned actor `?1`'s state revision only at
        /// epoch `?2`. No row means the caller is not the owner.
        fence = "UPDATE actors SET state_revision = state_revision + 1
             WHERE actor_key = ?1 AND epoch = ?2 AND state = 'owned'
             RETURNING state_revision, mail_seq, acked_seq";

        /// Actor `?1`'s epoch and state, for a refusal's account.
        epoch_of = "SELECT epoch, state FROM actors WHERE actor_key = ?1";

        /// Release every actor boot `?2` of node `?1` owns, ready at `?3`,
        /// with its epoch bumped.
        release_owned_by = "UPDATE actors
             SET state = 'ready', ready_at_ms = ?3, epoch = epoch + 1,
                 owner_node = NULL, owner_boot = NULL
             WHERE owner_node = ?1 AND owner_boot = ?2 AND state = 'owned'
             RETURNING actor_key, epoch";

        /// Every actor boot `?2` of node `?1` owns, with its epoch.
        owned_by = "SELECT actor_key, epoch FROM actors
             WHERE owner_node = ?1 AND owner_boot = ?2 AND state = 'owned'
             ORDER BY actor_key";

        /// Acknowledge actor `?1`'s mailbox through `?2`.
        ack = "UPDATE actors SET acked_seq = ?2 WHERE actor_key = ?1 AND acked_seq < ?2";

        /// Release actor `?1` to state `?2` (`idle` or `waiting`) due at
        /// `?3`, or to `ready` at `?4` when it still has mail; the epoch is
        /// bumped.
        release = "UPDATE actors
             SET state = CASE WHEN mail_seq > acked_seq THEN 'ready'
                              ELSE CAST(?2 AS TEXT) END,
                 ready_at_ms = CASE WHEN mail_seq > acked_seq THEN CAST(?4 AS BIGINT)
                                    ELSE NULL END,
                 next_due_ms = CASE WHEN mail_seq > acked_seq THEN NULL
                                    ELSE CAST(?3 AS BIGINT) END,
                 epoch = epoch + 1, owner_node = NULL, owner_boot = NULL
             WHERE actor_key = ?1
             RETURNING state";

        /// End actor `?1`: terminal, its mailbox acknowledged, its epoch
        /// bumped.
        end = "UPDATE actors
             SET state = 'terminal', ready_at_ms = NULL, next_due_ms = NULL,
                 acked_seq = mail_seq, epoch = epoch + 1,
                 owner_node = NULL, owner_boot = NULL
             WHERE actor_key = ?1
             RETURNING state";

        /// Wake actor `?1` at `?2`: take the next mailbox position, and make
        /// it ready when it was idle or waiting. No row when it is unknown
        /// or terminal.
        wake = "UPDATE actors
             SET mail_seq = mail_seq + 1,
                 state = CASE WHEN state IN ('idle', 'waiting') THEN 'ready' ELSE state END,
                 ready_at_ms = CASE WHEN state IN ('idle', 'waiting') THEN CAST(?2 AS BIGINT)
                                    ELSE ready_at_ms END,
                 next_due_ms = CASE WHEN state IN ('idle', 'waiting') THEN NULL
                                    ELSE next_due_ms END
             WHERE actor_key = ?1 AND state <> 'terminal'
             RETURNING mail_seq, state, owner_node, owner_boot";

        /// Owned actor `?1` at epoch `?2` with its unacknowledged mail, one
        /// row per mail (or one row with NULL mail columns): the owner's
        /// fenced read, in one snapshot.
        open = "SELECT a.epoch, a.state, a.state_revision, a.acked_seq, a.mail_seq,
                    m.seq, m.kind, m.body, m.appended_at_ms
             FROM actors a
             LEFT JOIN actor_mail m ON m.actor_key = a.actor_key AND m.seq > a.acked_seq
             WHERE a.actor_key = ?1
             ORDER BY m.seq";

        /// Actor `?1`'s row and pending mail count.
        snapshot = "SELECT a.kind, a.state, a.epoch, a.owner_node, a.owner_boot,
                    a.mail_seq, a.acked_seq, a.next_due_ms, a.state_revision, a.formats,
                    (SELECT COUNT(*) FROM actor_mail m WHERE m.actor_key = a.actor_key)
                        AS pending_mail
             FROM actors a
             WHERE a.actor_key = ?1";
    }
}

crate::statements! {
    /// `actor_mail` statements both backends issue verbatim.
    pub struct MailStatements @ "durable_mail" {
        /// Append mail `?2` of kind `?3` with body `?4` to actor `?1` at `?5`.
        append = "INSERT INTO actor_mail (actor_key, seq, kind, body, appended_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)";

        /// Delete actor `?1`'s mail through `?2`.
        delete_through = "DELETE FROM actor_mail WHERE actor_key = ?1 AND seq <= ?2";

        /// Delete all of actor `?1`'s mail.
        delete_all = "DELETE FROM actor_mail WHERE actor_key = ?1";
    }
}
