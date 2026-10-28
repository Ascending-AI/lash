//! PostgreSQL scheduling, fences and node-liveness statements.
lash_store_sql::statements! {
    /// The engine statements only PostgreSQL issues.
    pub(super) struct PostgresDurableStatements @ "durable_postgres" {
        /// Boot `?2` of node `?1`'s lease, locked against a concurrent reap
        /// for the rest of the transaction: its draining flag, or no row.
        node_live = "SELECT draining FROM nodes WHERE node_id = ?1 AND boot_id = ?2 FOR SHARE";

        /// Actor `?1`'s row, share-locked: it waits for the owner commit
        /// in flight, whose fence holds the row, to end. Autocommit, so the
        /// lock ends with the statement.
        await_owner_commit = "SELECT 1 FROM actors WHERE actor_key = ?1 FOR SHARE";

        /// Give up to `?5` actors claimable at `?1` in a format set of `?4`
        /// whose claim requirements, hard requirements among them, are
        /// served by `?7`, including retired owners; process actors in
        /// another set or with another requirement with mail of kind `?6`
        /// (a cancel) pending; and session actors in a set of `?4` with
        /// another requirement whose open turn holds an `immediate` cancel
        /// request, with mail of kind `?8` (a close) pending, or whose close
        /// has not reached its tombstone (FIG-6072); to boot `?3` of node
        /// `?2`, oldest first, bumping each epoch; returns each with the
        /// state that made it claimable, its format set and its
        /// requirements. A claim that finds no commit advanced the actor
        /// since the previous claim counts one more failed activation; any
        /// such commit resets the count.
        claim = "WITH c AS (
                 SELECT p.actor_key, p.state, p.formats, p.requires
                 FROM actors p
                 WHERE ((p.state = 'ready' AND p.ready_at_ms <= ?1)
                     OR (p.state = 'waiting' AND p.next_due_ms <= ?1))
                   AND ((p.formats = ANY(?4)
                         AND (p.requires IS NULL
                             OR NOT EXISTS (
                                 SELECT 1 FROM jsonb_array_elements_text(p.requires::jsonb) r(value)
                                 WHERE NOT EXISTS (
                                     SELECT 1 FROM unnest(?7::text[]) s(value)
                                     WHERE s.value = r.value OR (
                                         left(r.value, 15) = 'helper-archive:'
                                         AND length(r.value) = 90 AND length(s.value) = 90
                                         AND left(r.value, 80) = left(s.value, 80)
                                         AND substr(r.value, 81) ~ '^[0-9]{10}$'
                                         AND substr(s.value, 81) ~ '^[0-9]{10}$'
                                         AND substr(r.value, 81) <= '4294967295'
                                         AND substr(s.value, 81) <= '4294967295'
                                         AND substr(s.value, 81) >= substr(r.value, 81)) OR (
                                         left(r.value, 15) = 'helper-outside:'
                                         AND length(r.value) >= 80
                                         AND s.value = 'helper-library:' || substr(r.value, 16, 64)) OR (
                                         left(r.value, 9) = 'callback:'
                                         AND right(s.value, 1) = '/'
                                         AND left(r.value, length(s.value)) = s.value
                                         AND strpos(substr(r.value, length(s.value) + 1), '/') = 0)))))
                     OR (p.kind = 'process' AND EXISTS (
                         SELECT 1 FROM actor_mail m
                         WHERE m.actor_key = p.actor_key AND m.seq > p.acked_seq
                           AND m.kind = ?6))
                     OR (p.kind = 'session' AND p.formats = ANY(?4)
                         AND (EXISTS (
                                 SELECT 1 FROM turn_cancel_requests c
                                 WHERE c.session_id = substr(p.actor_key, 3)
                                   AND c.mode = 'immediate')
                             OR EXISTS (
                                 SELECT 1 FROM actor_mail m
                                 WHERE m.actor_key = p.actor_key AND m.seq > p.acked_seq
                                   AND m.kind = ?8)
                             OR EXISTS (
                                 SELECT 1 FROM session_close s
                                 WHERE s.session_id = substr(p.actor_key, 3)
                                   AND (s.done_step IS NULL OR s.done_step <> 'tombstone')))))
                 ORDER BY COALESCE(p.ready_at_ms, p.next_due_ms), p.actor_key
                 LIMIT ?5
                 FOR UPDATE OF p SKIP LOCKED
             )
             UPDATE actors AS a
             SET state = 'owned', epoch = a.epoch + 1, owner_node = ?2, owner_boot = ?3,
                 ready_at_ms = NULL, next_due_ms = NULL,
                 failed_activations = CASE WHEN a.claimed_revision = a.state_revision
                                           THEN a.failed_activations + 1 ELSE 0 END,
                 claimed_revision = a.state_revision
             FROM c
             WHERE a.actor_key = c.actor_key
             RETURNING a.actor_key, a.epoch, c.state, c.formats, c.requires";

        /// What a claim could find now (FIG-7044), at injected store time
        /// `?1` or else the server's: that time, whether some actor is ready
        /// or waiting with a passed due time, and the earliest due time
        /// still ahead. Every claimable actor is one of these, whatever node
        /// claims. One statement, holding no lock.
        readiness = "SELECT t.now,
                    EXISTS (SELECT 1 FROM actors
                        WHERE state = 'ready' AND ready_at_ms <= t.now)
                    OR EXISTS (SELECT 1 FROM actors
                        WHERE state = 'waiting' AND next_due_ms <= t.now),
                    (SELECT MIN(next_due_ms) FROM actors
                        WHERE state = 'waiting' AND next_due_ms > t.now)
             FROM (SELECT COALESCE(?1::BIGINT,
                 (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT) AS now) AS t";

        /// Session `?2`'s write authority when its actor `?1` exists: the
        /// actor row, then the history lock (seed `1`), in that order, and
        /// one row. The aggregate reads the whole locking subquery before
        /// the projection takes the advisory lock, so one statement keeps
        /// the order. No row, and no lock, when the actor does not exist.
        lock_session_writes = "SELECT held.locked, pg_advisory_xact_lock(hashtextextended(?2, 1::bigint))
             FROM (
                 SELECT count(*) AS locked FROM (
                     SELECT 1 FROM actors WHERE actor_key = ?1 FOR NO KEY UPDATE
                 ) AS actor
             ) AS held
             WHERE held.locked > 0";

        /// Lock the rows of actors `?1` in key order: a mailbox commit
        /// that wakes several actors takes their locks before any write,
        /// so two such commits never wait on each other in a cycle.
        lock_actors = "SELECT actor_key FROM actors
             WHERE actor_key = ANY(?1)
             ORDER BY actor_key
             FOR NO KEY UPDATE";

        /// Every registered boot, and whether some session holds its
        /// liveness lock. A free lock is taken for this statement's
        /// transaction only, so the probe holds nothing after it.
        liveness = "SELECT node_id, boot_id,
                    NOT pg_try_advisory_xact_lock(1818325864, hashtext(boot_id))
             FROM nodes
             ORDER BY node_id, boot_id";

        /// Take boot `?1`'s liveness lock for the rest of this transaction
        /// when no session holds it: true when it was free.
        take_liveness = "SELECT pg_try_advisory_xact_lock(1818325864, hashtext(?1))";

        /// Hold boot `?1`'s liveness lock for this session's life, waiting
        /// for an earlier session of the same boot to end.
        hold_liveness = "SELECT pg_advisory_lock(1818325864, hashtext(?1))";

        /// The owner's open of actor `?1`: its row, its pending mail oldest
        /// first, the database clock and, when `?2` names its session, that
        /// session's unfinished turn's accepted cancel request, in one read.
        /// It also reads what a head commit of that session plans on
        /// (FIG-5942): the published head's revision and leaf, the leaf's
        /// generation, frame and liveness, and every plugin writer range.
        /// External-frontier-only refresh, with body bytes bounded before transfer.
        open_retained = "WITH page AS MATERIALIZED (
                SELECT m.seq, octet_length(m.body) AS bytes,
                    sum(octet_length(m.body)) OVER (ORDER BY m.seq) AS total
                FROM actor_mail m JOIN actors a ON a.actor_key = m.actor_key
                WHERE a.actor_key = ?1 AND m.seq > a.acked_seq)
             SELECT a.epoch, a.state, a.state_revision, a.acked_seq, a.mail_seq,
                    p.seq, p.bytes, p.total, m.kind, m.body, m.appended_at_ms,
                    c.turn_id, c.request_id, c.origin, c.reason, c.disposition, c.mode,
                    (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT
             FROM actors a
             LEFT JOIN page p ON p.total - p.bytes <= ?3
             LEFT JOIN actor_mail m ON m.actor_key = a.actor_key AND m.seq = p.seq
                 AND p.total <= ?3 AND ?3 > 0
             LEFT JOIN session_runs r ON r.session_id = ?2
                 AND r.admission_json IS NOT NULL AND r.terminal_kind IS NULL
             LEFT JOIN turn_cancel_requests c ON c.session_id = r.session_id AND c.turn_id = r.run
             WHERE a.actor_key = ?1 ORDER BY p.seq";

        open = "SELECT a.epoch, a.state, a.state_revision, a.acked_seq, a.mail_seq,
                    m.seq, m.kind, m.body, m.appended_at_ms,
                    (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT,
                    c.turn_id, c.request_id, c.origin, c.reason, c.disposition, c.mode,
                    h.head_revision, v.leaf_node_id, leaf.generation, leaf.frame_node_id,
                    NOT leaf.tombstoned,
                    (SELECT jsonb_agg(jsonb_build_array(plugin_id, min_format, max_format))
                       FROM fleet_plugin_writers), v.config_ref
             FROM actors a
             LEFT JOIN actor_mail m ON m.actor_key = a.actor_key AND m.seq > a.acked_seq
             LEFT JOIN session_runs r ON r.session_id = ?2 AND r.admission_json IS NOT NULL
                 AND r.terminal_kind IS NULL
             LEFT JOIN turn_phases p ON p.session_id = r.session_id AND p.run = r.run
             LEFT JOIN turn_cancel_requests c ON c.session_id = p.session_id AND c.turn_id = p.run
             LEFT JOIN session_head h ON h.session_id = ?2
             LEFT JOIN session_revisions v ON v.session_id = h.session_id
                 AND v.head_revision = h.head_revision
             LEFT JOIN graph_nodes leaf ON leaf.node_id = v.leaf_node_id
             WHERE a.actor_key = ?1
             ORDER BY m.seq";

        /// A transaction's first statement after its fenced `BEGIN`: the
        /// epoch the writer fence admits (no row when the fleet record is
        /// absent) and the database clock.
        fence_and_clock = "SELECT (SELECT format_version FROM fleet_format WHERE singleton = TRUE),
                    (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT";

        /// [`Self::fence_and_clock`] and the transaction's id, by which a
        /// lost `COMMIT` is reconciled: a mailbox commit's first statement.
        fence_clock_and_xact = "SELECT (SELECT format_version FROM fleet_format WHERE singleton = TRUE),
                    (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT,
                    pg_current_xact_id()::text";

        /// An owner commit's first statement: [`Self::fence_clock_and_xact`]
        /// and actor `?1`'s ownership fence at epoch `?2`, its state revision
        /// bumped (no revision when the epoch is not current), and, when `?3`
        /// names its session, that session's unfinished turn's accepted
        /// cancel request as the statement's snapshot holds it: one row.
        owner_envelope = "WITH fenced AS (
                 UPDATE actors SET state_revision = state_revision + 1
                 WHERE actor_key = ?1 AND epoch = ?2 AND state = 'owned'
                     AND state_revision = ?4
                 RETURNING state_revision
             )
             SELECT (SELECT format_version FROM fleet_format WHERE singleton = TRUE),
                    (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT,
                    pg_current_xact_id()::text,
                    f.state_revision,
                    c.turn_id, c.request_id, c.origin, c.reason, c.disposition, c.mode
             FROM (SELECT 1) AS one
             LEFT JOIN fenced f ON TRUE
             LEFT JOIN session_runs r ON r.session_id = ?3 AND f.state_revision IS NOT NULL
                 AND r.admission_json IS NOT NULL AND r.terminal_kind IS NULL
             LEFT JOIN turn_phases p ON p.session_id = r.session_id AND p.run = r.run
             LEFT JOIN turn_cancel_requests c ON c.session_id = p.session_id
                 AND c.turn_id = p.run";

        /// Acknowledge actor `?1`'s mail through `?2` and delete it, in one
        /// statement.
        ack_through = "WITH acked AS (
                 UPDATE actors SET acked_seq = ?2 WHERE actor_key = ?1 AND acked_seq < ?2
             )
             DELETE FROM actor_mail WHERE actor_key = ?1 AND seq <= ?2";

        /// Send notification payload `?2[i]` on channel `?1[i]`, for each
        /// `i`, outside any writing transaction.
        notify = "SELECT pg_notify(t.channel, t.payload)
             FROM unnest(CAST(?1 AS TEXT[]), CAST(?2 AS TEXT[])) AS t(channel, payload)";
    }
}
