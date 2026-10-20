//! Connection adapters: snapshot boundaries, clock sampling and store-origin tags.
use super::*;

impl SqliteDurableStore {
    pub(super) fn instant(&self) -> Result<DurableInstant, DurableError> {
        integer(self.clock.timestamp_ms())
            .map(DurableInstant)
            .map_err(store_failure)
    }

    pub(super) async fn write<T, F>(&self, label: CommitLabel, body: F) -> Result<T, DurableError>
    where
        T: Send + 'static,
        F: FnOnce(&FencedTx<'_>, DurableInstant) -> Flow<T> + Send + 'static,
    {
        let now = self.instant()?;
        tracing::trace!(label = label.as_str(), "durable sqlite commit");
        self.observer
            .observe_sql(label.as_str(), "sqlite", async {
                self.conn
                    .write_flow(move |tx| {
                        crate::observed_sql::with_origin(std::any::type_name::<F>(), || {
                            body(tx, now)
                        })
                    })
                    .await
                    .map_err(store_failure)?
            })
            .await
    }

    pub(super) async fn read<T, F>(&self, body: F) -> Result<T, DurableError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> rusqlite::Result<Result<T, DurableError>> + Send + 'static,
    {
        self.readers
            .next()
            .read(move |tx| {
                crate::observed_sql::with_origin(std::any::type_name::<F>(), || body(tx))
            })
            .await
            .map_err(store_failure)?
    }

    /// What a claim could find now ([`DurableStore::readiness`]): one
    /// statement on a reader, so no claim write opens to learn it.
    pub(super) async fn readiness_now(&self) -> Result<lash_durable::Readiness, DurableError> {
        let now = self.instant()?;
        self.query(move |connection| {
            let (due, next_due) = connection
                .prepare_cached(SQL.sqlite.readiness.sql())?
                .query_row([now.0], |row| {
                    Ok((row.get::<_, bool>(0)?, row.get::<_, Option<i64>>(1)?))
                })?;
            Ok(Ok(lash_durable::Readiness {
                now,
                due,
                next_due: next_due.map(DurableInstant),
            }))
        })
        .await
    }

    /// A single statement owns its SQLite snapshot. Compound reads keep
    /// using `read` so their rows and graph bodies share one transaction.
    pub(super) async fn query<T, F>(&self, body: F) -> Result<T, DurableError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> rusqlite::Result<Result<T, DurableError>> + Send + 'static,
    {
        self.readers
            .next()
            .call(move |conn| {
                crate::observed_sql::with_origin(std::any::type_name::<F>(), || body(conn))
            })
            .await
            .map_err(store_failure)?
    }
}

lash_store_sql::statements! {
    /// The engine statements only SQLite issues.
    pub(crate) struct SqliteDurableStatements @ "durable_sqlite" {
        /// Whether boot `?2` of node `?1` holds a lease: its draining flag,
        /// or no row.
        node_live = "SELECT draining FROM nodes WHERE node_id = ?1 AND boot_id = ?2";

        /// Every registered boot.
        boots = "SELECT node_id, boot_id FROM nodes ORDER BY node_id, boot_id";

        /// Up to `?3` actors claimable at `?1` in a format set of JSON array
        /// `?2` whose claim requirements are served by JSON array `?5`,
        /// including retired-owner tokens, and whose package set node `?7`
        /// holds (CLAIM-HOLDS); process
        /// actors in another set or with another requirement with mail of
        /// kind `?4` (a cancel) pending; and session actors in a set of `?2`
        /// with another requirement whose open turn holds an `immediate`
        /// cancel request, with mail of kind `?6` (a close) pending, or
        /// whose close has not reached its tombstone (FIG-6072); oldest
        /// first, with the state that made each claimable, its format set,
        /// its requirements and whether node `?7` holds its package set.
        claimable = "SELECT a.actor_key, a.state, a.formats, a.requires,
                    (a.packages IS NULL OR (
                        EXISTS (SELECT 1 FROM package_sets AS ps WHERE ps.digest = a.packages)
                        AND NOT EXISTS (SELECT 1 FROM package_set_members AS pm
                            WHERE pm.set_digest = a.packages
                              AND NOT EXISTS (SELECT 1 FROM node_artifacts AS nh
                                  WHERE nh.node_id = ?7 AND nh.artifact = pm.artifact))))
                        AS holds
             FROM actors a
             WHERE ((a.state = 'ready' AND a.ready_at_ms <= ?1)
                 OR (a.state = 'waiting' AND a.next_due_ms <= ?1))
               AND ((holds AND a.formats IN (SELECT value FROM json_each(?2))
                     AND (a.requires IS NULL OR NOT EXISTS (
                         SELECT 1 FROM json_each(a.requires) r
                         WHERE NOT EXISTS (
                             SELECT 1 FROM json_each(?5) s
                             WHERE s.value = r.value OR (
                                 substr(r.value, 1, 9) = 'callback:'
                                 AND substr(s.value, -1) = '/'
                                 AND substr(r.value, 1, length(s.value)) = s.value
                                 AND instr(substr(r.value, length(s.value) + 1), '/') = 0)))))
                 OR (a.kind = 'process' AND EXISTS (
                     SELECT 1 FROM actor_mail m
                     WHERE m.actor_key = a.actor_key AND m.seq > a.acked_seq
                       AND m.kind = ?4))
                 OR (a.kind = 'session' AND a.formats IN (SELECT value FROM json_each(?2))
                     AND (EXISTS (
                             SELECT 1 FROM turn_cancel_requests c
                             WHERE c.session_id = substr(a.actor_key, 3)
                               AND c.mode = 'immediate')
                         OR EXISTS (
                             SELECT 1 FROM actor_mail m
                             WHERE m.actor_key = a.actor_key AND m.seq > a.acked_seq
                               AND m.kind = ?6)
                         OR EXISTS (
                             SELECT 1 FROM session_close s
                             WHERE s.session_id = substr(a.actor_key, 3)
                               AND (s.done_step IS NULL OR s.done_step <> 'tombstone')))))
             ORDER BY COALESCE(a.ready_at_ms, a.next_due_ms), a.actor_key
             LIMIT ?3";

        /// What a claim at `?1` could find (FIG-7044): whether some actor is
        /// ready or waiting with a passed due time, and the earliest due time
        /// still ahead. Every claimable actor is one of these, whatever node
        /// claims; both partial indexes answer it.
        readiness = "SELECT EXISTS (SELECT 1 FROM actors
                        WHERE state = 'ready' AND ready_at_ms <= ?1)
                    OR EXISTS (SELECT 1 FROM actors
                        WHERE state = 'waiting' AND next_due_ms <= ?1),
                    (SELECT MIN(next_due_ms) FROM actors
                        WHERE state = 'waiting' AND next_due_ms > ?1)";

        /// Session `?1`'s unfinished turn with its phase row, in the turn
        /// read's column order, and the cancel request the turn accepted, in
        /// the cancel read's (FIG-6617): one statement, as the open's turn.
        turn = "SELECT r.run, r.admission_json, p.phase, p.phase_arg, p.iteration,
                    p.checkpoint_ref, p.model_request_ref, p.model_deadline_ms,
                    p.turn_deadline_ms, p.written_epoch, p.model_calls,
                    p.model_stream_from, p.working_execution_ref, p.answer_ref, p.tool_calls,
                    c.request_id, c.origin, c.reason, c.disposition, c.mode
             FROM session_runs AS r
             JOIN turn_phases AS p ON p.session_id = r.session_id AND p.run = r.run
             LEFT JOIN turn_cancel_requests AS c
                 ON c.session_id = r.session_id AND c.turn_id = r.run
             WHERE r.session_id = ?1 AND r.admission_json IS NOT NULL
               AND r.terminal_kind IS NULL";

        /// Actor `?1`'s epoch, state, revision and mail marks with each mail
        /// past its acknowledgement, in order (one row with NULL mail columns
        /// when none); and, when `?2` names its session, on every row that
        /// session's unfinished turn with its phase row, in the turn read's
        /// column order, and the cancel request the turn accepted, in the
        /// cancel read's (FIG-6399). One statement, so one snapshot answers
        /// the open; the turn is looked up once.
        open = "WITH t AS MATERIALIZED (
                 SELECT r.run, r.admission_json, p.phase, p.phase_arg, p.iteration,
                        p.checkpoint_ref, p.model_request_ref, p.model_deadline_ms,
                        p.turn_deadline_ms, p.written_epoch, p.model_calls,
                        p.model_stream_from, p.working_execution_ref, p.answer_ref, p.tool_calls,
                        c.request_id, c.origin, c.reason, c.disposition, c.mode
                 FROM session_runs AS r
                 JOIN turn_phases AS p ON p.session_id = r.session_id AND p.run = r.run
                 LEFT JOIN turn_cancel_requests AS c
                     ON c.session_id = r.session_id AND c.turn_id = r.run
                 WHERE r.session_id = ?2 AND r.admission_json IS NOT NULL
                   AND r.terminal_kind IS NULL)
             SELECT a.epoch, a.state, a.state_revision, a.acked_seq, a.mail_seq,
                    m.seq, m.kind, m.body, m.appended_at_ms, t.*
             FROM actors a
             LEFT JOIN actor_mail m ON m.actor_key = a.actor_key AND m.seq > a.acked_seq
             LEFT JOIN t ON TRUE
             WHERE a.actor_key = ?1
             ORDER BY m.seq";

        /// Release without rewriting the due date: a claimed actor's is
        /// NULL, and a release without a timer leaves it so. The SQL still
        /// classifies pending mail while holding the writer lock.
        release_without_due = "UPDATE actors
             SET state = CASE WHEN mail_seq > acked_seq OR CAST(?2 AS TEXT) = 'ready'
                              THEN 'ready' ELSE CAST(?2 AS TEXT) END,
                 ready_at_ms = CASE WHEN mail_seq > acked_seq OR CAST(?2 AS TEXT) = 'ready'
                                    THEN CAST(?4 AS BIGINT) ELSE NULL END,
                 epoch = epoch + 1, owner_node = NULL, owner_boot = NULL
             WHERE actor_key = ?1
             RETURNING state";

        /// Give the bounded JSON array of actor keys `?1` to boot `?3` of
        /// node `?2`, returning each identity with its bumped epoch. A
        /// claim that finds no commit advanced the actor since the previous
        /// claim counts one more failed activation; any such commit resets
        /// the count.
        claim = "UPDATE actors
             SET state = 'owned', epoch = epoch + 1, owner_node = ?2, owner_boot = ?3,
                 ready_at_ms = NULL, next_due_ms = NULL,
                 failed_activations = CASE WHEN claimed_revision = state_revision
                                           THEN failed_activations + 1 ELSE 0 END,
                 claimed_revision = state_revision
             WHERE actor_key IN (SELECT value FROM json_each(?1))
             RETURNING actor_key, epoch";
    }
}
