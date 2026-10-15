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
//! - A claim counts one more failed activation when no commit advanced the
//!   actor since the previous claim. The owner fence bumps the revision on
//!   every commit, so a `ready` release that writes nothing else keeps the
//!   claim's mark ([`ActorStatements::unadvanced`]).

pub mod graph;
pub mod pages;
pub mod processes;
pub mod prompts;
pub mod run_records;
pub mod session_close;
pub mod session_mail;
pub mod snapshots;
pub mod turns;
pub mod waits;

/// The node table's unprefixed name.
pub const NODES_TABLE: &str = "nodes";

/// The actor table's unprefixed name.
pub const ACTORS_TABLE: &str = "actors";

/// PostgreSQL's one-call owner commits' recorded answers (FIG-5936): the
/// table only that backend's executor names.
pub const OWNER_COMMIT_LATCHES_TABLE: &str = "owner_commit_latches";

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

        /// Mark boot `?2` of node `?1` draining; no row when it holds no
        /// lease.
        mark_draining = "UPDATE nodes SET draining = TRUE
             WHERE node_id = ?1 AND boot_id = ?2
             RETURNING boot_id";

        /// The decoded format sets, as JSON arrays, of every node whose lease
        /// is live at `?1`.
        live_decodes = "SELECT formats_json FROM nodes
             WHERE heartbeat_expires_at_ms >= ?1
             ORDER BY node_id";
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

        /// Every actor boot `?2` of node `?1` owns, with its epoch, format
        /// set and claim requirements.
        owned_by = "SELECT actor_key, epoch, formats, requires FROM actors
             WHERE owner_node = ?1 AND owner_boot = ?2 AND state = 'owned'
             ORDER BY actor_key";

        /// Record that actor `?1`'s state is written in format set `?2`.
        stamp_formats = "UPDATE actors SET formats = ?2 WHERE actor_key = ?1";

        /// Record actor `?1`'s claim requirements: the canonical JSON array
        /// `?2`, or `NULL` for none (FIG-6034 D3).
        stamp_requires = "UPDATE actors SET requires = ?2 WHERE actor_key = ?1";

        /// Up to `?3` actors in format set `?1` that have not ended, by key,
        /// after key `?2`.
        in_formats = "SELECT actor_key FROM actors
             WHERE formats = ?1 AND state <> 'terminal' AND actor_key > ?2
             ORDER BY actor_key
             LIMIT ?3";

        /// Acknowledge actor `?1`'s mailbox through `?2`.
        ack = "UPDATE actors SET acked_seq = ?2 WHERE actor_key = ?1 AND acked_seq < ?2";

        /// Release actor `?1` to state `?2` (`idle`, `waiting` or `ready`)
        /// due at `?3`, or to `ready` at `?4` when it still has mail; the
        /// epoch is bumped.
        release = "UPDATE actors
             SET state = CASE WHEN mail_seq > acked_seq OR CAST(?2 AS TEXT) = 'ready'
                              THEN 'ready' ELSE CAST(?2 AS TEXT) END,
                 ready_at_ms = CASE WHEN mail_seq > acked_seq OR CAST(?2 AS TEXT) = 'ready'
                                    THEN CAST(?4 AS BIGINT) ELSE NULL END,
                 next_due_ms = CASE WHEN mail_seq > acked_seq OR CAST(?2 AS TEXT) = 'ready'
                                    THEN NULL ELSE CAST(?3 AS BIGINT) END,
                 epoch = epoch + 1, owner_node = NULL, owner_boot = NULL
             WHERE actor_key = ?1
             RETURNING state";

        /// Keep actor `?1`'s claim mark across a commit that advanced
        /// nothing, when no commit advanced it since its claim either: the
        /// fence bumped the revision, so the mark moves with it and the next
        /// claim counts one more failed activation.
        unadvanced = "UPDATE actors SET claimed_revision = state_revision
             WHERE actor_key = ?1 AND claimed_revision = state_revision - 1";

        /// End actor `?1`: terminal, its mailbox acknowledged, its epoch
        /// bumped.
        end = "UPDATE actors
             SET state = 'terminal', ready_at_ms = NULL, next_due_ms = NULL,
                 acked_seq = mail_seq, epoch = epoch + 1,
                 owner_node = NULL, owner_boot = NULL
             WHERE actor_key = ?1
             RETURNING state";

        /// Wake actor `?1` at `?2`: take the next mailbox position, and make
        /// it ready when it was idle or waiting. A parked actor stays parked:
        /// only a control wake readies it. No row when it is unknown or
        /// terminal.
        wake = "UPDATE actors
             SET mail_seq = mail_seq + 1,
                 state = CASE WHEN state IN ('idle', 'waiting') THEN 'ready' ELSE state END,
                 ready_at_ms = CASE WHEN state IN ('idle', 'waiting') THEN CAST(?2 AS BIGINT)
                                    ELSE ready_at_ms END,
                 next_due_ms = CASE WHEN state IN ('idle', 'waiting') THEN NULL
                                    ELSE next_due_ms END
             WHERE actor_key = ?1 AND state <> 'terminal'
             RETURNING mail_seq, state, owner_node, owner_boot";

        /// Actor `?1`'s row and pending mail count.
        snapshot = "SELECT a.kind, a.state, a.epoch, a.owner_node, a.owner_boot,
                    a.mail_seq, a.acked_seq, a.next_due_ms, a.state_revision, a.formats,
                    (SELECT COUNT(*) FROM actor_mail m WHERE m.actor_key = a.actor_key)
                        AS pending_mail,
                    a.park_json, a.failed_activations
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

crate::statements! {
    /// Read-only claim facts: one statement, including both cancel exceptions.
    pub struct ClaimFactsStatements @ "durable_claim_facts" {
        read = "SELECT n.boot_id, n.draining, a.state, a.epoch, a.state_revision,
                    a.ready_at_ms, a.next_due_ms, a.formats, a.requires,
                    EXISTS (SELECT 1 FROM actor_mail m
                        WHERE m.actor_key = a.actor_key AND m.seq > a.acked_seq AND m.kind = ?3),
                    EXISTS (SELECT 1 FROM turn_cancel_requests c
                        WHERE c.session_id = substr(a.actor_key, 3) AND a.kind = 'session'
                          AND c.mode = 'immediate'),
                    EXISTS (SELECT 1 FROM actor_mail m
                        WHERE m.actor_key = a.actor_key AND m.seq > a.acked_seq AND m.kind = ?4),
                    EXISTS (SELECT 1 FROM session_close s
                        WHERE s.session_id = substr(a.actor_key, 3) AND a.kind = 'session'
                          AND (s.done_step IS NULL OR s.done_step <> 'tombstone'))
                FROM (SELECT ?1 AS node_id, ?2 AS actor_key) requested
                LEFT JOIN nodes n ON n.node_id = requested.node_id
                LEFT JOIN actors a ON a.actor_key = requested.actor_key";
    }
}

#[cfg(feature = "testing")]
crate::statements! {
    /// Targeted test-writer acquisition; production has no such authority seam.
    pub struct TestActorStatements @ "durable_test_actor" {
        /// Acquire exactly actor `?1` at its observed epoch `?2`, for boot
        /// `?4` of node `?3`. Never replace a live owner or a terminal.
        acquire_idle = "UPDATE actors
             SET state = 'owned', epoch = epoch + 1, owner_node = ?3, owner_boot = ?4,
                 ready_at_ms = NULL, next_due_ms = NULL
             WHERE actor_key = ?1 AND epoch = ?2
               AND state IN ('idle', 'ready', 'waiting', 'parked')
             RETURNING epoch";
    }
}
