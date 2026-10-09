//! The store's tables, its incarnation, and the SQL it runs.
//!
//! Five tables live in the configured schema, created by the published
//! `postgres-process-replay-schema.sql`:
//!
//! - `process_replay_incarnation`, logged: the one row naming the history
//!   the unlogged tables hold.
//! - `process_replay_sentinel`, unlogged: the one row proving that history
//!   is still there. Crash recovery and failover truncate unlogged tables,
//!   so a missing or foreign sentinel means the history is gone and the
//!   incarnation rotates. It also holds what is only true of that history:
//!   the position watermark of evicted and forgotten processes, and the
//!   aggregate budget (resident processes, reserved bytes) the heads hold.
//! - `process_replay_head`, unlogged: one row per process, the sequencer.
//!   Its `tail_position` is the last position assigned; `floor_position` is
//!   the process's generation boundary (no cursor below it continues, and
//!   invalidation raises it); `first_retained` is the lowest position still
//!   held; the counters drive per-process retention, and `reserved_bytes`
//!   is the process's share of the aggregate byte budget.
//! - `process_replay_log`, unlogged: the events, keyed by (process,
//!   position).
//! - `process_replay_dedupe`, unlogged: the observation identity of each
//!   retained event and the position holding it.
//!
//! Every writer locks heads in process order and the sentinel after them,
//! and takes other processes' heads only without waiting, so writers never
//! deadlock on the store's own order.

use lash_core::{ProcessId, ProcessObservationCursor, ProcessReplayStoreError, ProcessSequence};
use sqlx::{PgPool, Postgres, Row as _, Transaction};

use super::codec::Doorbell;
use super::schema_shape::{self, PostgresProcessReplaySchemaReport, SCHEMA_DDL};

/// The incarnation the store's cursors name, and the position a process
/// with no head starts from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Incarnation {
    pub(super) id: String,
    pub(super) watermark: u64,
}

impl Incarnation {
    pub(super) fn cursor(
        &self,
        process_id: &ProcessId,
        sequence: ProcessSequence,
        position: u64,
    ) -> ProcessObservationCursor {
        ProcessObservationCursor::new(&self.id, process_id, sequence, position)
    }
}

/// Every statement the store issues, qualified with its schema.
#[derive(Debug)]
pub(super) struct Statements {
    pub(super) schema: String,
    pub(super) channel: String,
    pub(super) incarnation: String,
    pub(super) lock_heads: String,
    pub(super) lock_sentinel: String,
    pub(super) write_sentinel: String,
    pub(super) create_heads: String,
    pub(super) evict_heads: String,
    pub(super) purge_log: String,
    pub(super) purge_dedupe: String,
    pub(super) delivered: String,
    pub(super) expire_processes: String,
    pub(super) delete_below: String,
    pub(super) trim_bytes: String,
    pub(super) trim_dedupe: String,
    pub(super) insert_events: String,
    pub(super) append_and_update: String,
    pub(super) update_heads: String,
    pub(super) notify: String,
    pub(super) read: String,
    pub(super) positions: String,
    pub(super) expired_heads: String,
    pub(super) touch_heads: String,
    pub(super) forget_heads: String,
    pub(super) shrink_reservations: String,
    pub(super) invalidate_head: String,
}

/// Microseconds of a duration, for `$n * interval '1 microsecond'`.
pub(super) fn micros(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
}

impl Statements {
    pub(super) fn new(schema: &str) -> Self {
        let inc = format!("\"{schema}\".process_replay_incarnation");
        let sen = format!("\"{schema}\".process_replay_sentinel");
        let head = format!("\"{schema}\".process_replay_head");
        let log = format!("\"{schema}\".process_replay_log");
        let ded = format!("\"{schema}\".process_replay_dedupe");
        let head_columns = "process_id, tail_position, floor_position, first_retained, \
                            retained_events, retained_bytes, reserved_bytes";
        let age = |n: u8| format!("statement_timestamp() - ${n} * interval '1 microsecond'");
        // The first position of a head's window that is still within the
        // window's age, by database time.
        let first_live = |n: u8| {
            format!(
                "COALESCE((SELECT l.position FROM {log} l \
                            WHERE l.process_id = h.process_id \
                              AND l.position >= h.first_retained \
                              AND l.published_at >= {age} \
                            ORDER BY l.position LIMIT 1), \
                          h.tail_position + 1)",
                age = age(n)
            )
        };
        let events = "(process_id, position, sequence, payload, bytes, published_at) \
                      SELECT p, n, q, d, b, clock_timestamp() \
                      FROM unnest($1::text[], $2::bigint[], $3::bigint[], $4::bytea[], $5::bigint[]) \
                           AS e(p, n, q, d, b)";
        // A redelivered identity never reaches an insert, so a conflict is
        // a row whose event already left the window.
        let identities = format!(
            "INSERT INTO {ded} (process_id, identity, position) \
             SELECT p, k, n FROM unnest($1::text[], $6::text[], $2::bigint[]) AS e(p, k, n) \
             ON CONFLICT (process_id, identity) DO UPDATE SET position = EXCLUDED.position"
        );
        let heads = |first: u8| {
            format!(
                "UPDATE {head} h SET tail_position = u.tail, floor_position = u.floor, \
                        first_retained = u.first, retained_events = u.events, \
                        retained_bytes = u.bytes, reserved_bytes = u.reserved, \
                        touched_at = clock_timestamp() \
                 FROM unnest(${}::text[], ${}::bigint[], ${}::bigint[], ${}::bigint[], \
                             ${}::bigint[], ${}::bigint[], ${}::bigint[]) \
                      AS u(p, tail, floor, first, events, bytes, reserved) \
                 WHERE h.process_id = u.p",
                first,
                first + 1,
                first + 2,
                first + 3,
                first + 4,
                first + 5,
                first + 6
            )
        };
        Self {
            schema: schema.to_string(),
            channel: format!("{schema}_process_replay"),
            incarnation: format!(
                "SELECT i.incarnation_id, COALESCE(s.watermark, 0) AS watermark, \
                        (s.incarnation_id = i.incarnation_id) IS TRUE AS valid \
                 FROM {inc} i LEFT JOIN {sen} s ON true"
            ),
            // The first retained row's age decides whether this transaction
            // expires the process's oldest events first.
            lock_heads: format!(
                "SELECT h.{cols}, \
                        (SELECT l.published_at < {age} \
                           FROM {log} l WHERE l.process_id = h.process_id \
                            AND l.position = h.first_retained) IS TRUE AS expiring, \
                        (SELECT i.incarnation_id FROM {inc} i) AS incarnation_id, \
                        (SELECT s.incarnation_id FROM {sen} s) AS sentinel_id, \
                        (SELECT s.watermark FROM {sen} s) AS watermark \
                 FROM {head} h WHERE h.process_id = ANY($1) \
                 ORDER BY h.process_id FOR UPDATE OF h",
                cols = head_columns.replace(", ", ", h."),
                age = age(2),
            ),
            lock_sentinel: format!(
                "SELECT s.incarnation_id AS sentinel_id, \
                        (SELECT i.incarnation_id FROM {inc} i) AS incarnation_id, \
                        s.watermark, s.resident_processes, s.reserved_bytes \
                 FROM {sen} s FOR UPDATE"
            ),
            write_sentinel: format!(
                "UPDATE {sen} SET watermark = $1, resident_processes = $2, reserved_bytes = $3"
            ),
            create_heads: format!(
                "INSERT INTO {head} ({head_columns}, touched_at) \
                 SELECT p, $2, $2, $2 + 1, 0, 0, 0, clock_timestamp() \
                 FROM unnest($1::text[]) AS p \
                 ON CONFLICT (process_id) DO NOTHING RETURNING process_id"
            ),
            // The idlest heads outside `$1`: `$2` of them, and as many more
            // as it takes to free `$3` reserved bytes. A head another
            // writer holds is skipped, never waited for.
            evict_heads: format!(
                "DELETE FROM {head} WHERE process_id IN ( \
                   SELECT h.process_id FROM {head} h WHERE h.process_id IN ( \
                     SELECT v.process_id FROM ( \
                       SELECT c.process_id, row_number() OVER w AS n, \
                              COALESCE(sum(c.reserved_bytes) OVER ( \
                                w ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING), 0) AS freed \
                       FROM {head} c WHERE c.process_id <> ALL($1) \
                       WINDOW w AS (ORDER BY c.touched_at, c.process_id)) v \
                     WHERE v.n <= $2 OR v.freed < $3) \
                   FOR UPDATE SKIP LOCKED) \
                 RETURNING process_id, tail_position, reserved_bytes"
            ),
            purge_log: format!("DELETE FROM {log} WHERE process_id = ANY($1)"),
            purge_dedupe: format!("DELETE FROM {ded} WHERE process_id = ANY($1)"),
            delivered: format!(
                "SELECT d.process_id, d.identity, l.payload \
                 FROM unnest($1::text[], $2::text[]) AS k(p, identity) \
                 JOIN {ded} d ON d.process_id = k.p AND d.identity = k.identity \
                 JOIN {log} l ON l.process_id = d.process_id AND l.position = d.position"
            ),
            expire_processes: format!(
                "DELETE FROM {log} WHERE process_id = ANY($1) AND published_at < {age} \
                 RETURNING process_id, position, bytes",
                age = age(2),
            ),
            delete_below: format!(
                "DELETE FROM {log} l USING unnest($1::text[], $2::bigint[]) AS c(p, below) \
                 WHERE l.process_id = c.p AND l.position < c.below \
                 RETURNING l.process_id, l.position, l.bytes"
            ),
            // The oldest events of each listed process past its byte limit.
            trim_bytes: format!(
                "DELETE FROM {log} l USING ( \
                   SELECT w.process_id, w.position FROM ( \
                     SELECT g.process_id, g.position, c.cap, \
                            sum(g.bytes) OVER (PARTITION BY g.process_id \
                                               ORDER BY g.position DESC) AS suffix \
                     FROM {log} g JOIN unnest($1::text[], $2::bigint[]) AS c(p, cap) \
                       ON g.process_id = c.p) w \
                   WHERE w.suffix > w.cap) d \
                 WHERE l.process_id = d.process_id AND l.position = d.position \
                 RETURNING l.process_id, l.position, l.bytes"
            ),
            trim_dedupe: format!(
                "DELETE FROM {ded} d USING unnest($1::text[], $2::bigint[]) AS c(p, below) \
                 WHERE d.process_id = c.p AND d.position < c.below"
            ),
            insert_events: format!("WITH identified AS ({identities}) INSERT INTO {log} {events}"),
            // The common tick: no row leaves a window, so the append, the
            // identities, the heads and the doorbell are one statement.
            append_and_update: format!(
                "WITH appended AS (INSERT INTO {log} {events}), \
                      identified AS ({identities}), \
                      updated AS ({heads}) \
                 SELECT pg_notify($14, payload) FROM unnest($15::text[]) AS payload",
                heads = heads(7),
            ),
            update_heads: format!(
                "WITH updated AS ({heads}) \
                 SELECT pg_notify($8, payload) FROM unnest($9::text[]) AS payload",
                heads = heads(1),
            ),
            notify: "SELECT pg_notify($1, payload) FROM unnest($2::text[]) AS payload".to_string(),
            // One statement, so the head, the window's age cut and the rows
            // are one snapshot.
            read: format!(
                "SELECT i.incarnation_id, COALESCE(s.watermark, 0) AS watermark, \
                        (s.incarnation_id = i.incarnation_id) IS TRUE AS valid, \
                        h.tail_position, h.floor_position, {first_live} AS first_live, \
                        e.position, e.sequence, e.payload \
                 FROM {inc} i \
                 LEFT JOIN {sen} s ON true \
                 LEFT JOIN {head} h ON h.process_id = $1 \
                 LEFT JOIN LATERAL (SELECT l.position, l.sequence, l.payload \
                                    FROM {log} l WHERE l.process_id = $1 AND l.position > $2 \
                                    ORDER BY l.position LIMIT $4) e ON true \
                 ORDER BY e.position",
                first_live = first_live(3),
            ),
            // The head and, within the window's age, its first event and
            // its first event stamped past `$2`.
            positions: format!(
                "SELECT i.incarnation_id, COALESCE(s.watermark, 0) AS watermark, \
                        (s.incarnation_id = i.incarnation_id) IS TRUE AS valid, \
                        h.tail_position, {first_live} AS first_live, \
                        (SELECT l.position FROM {log} l \
                          WHERE l.process_id = h.process_id \
                            AND l.position >= h.first_retained \
                            AND l.published_at >= {age} AND l.sequence > $2 \
                          ORDER BY l.position LIMIT 1) AS first_newer \
                 FROM {inc} i \
                 LEFT JOIN {sen} s ON true \
                 LEFT JOIN {head} h ON h.process_id = $1",
                first_live = first_live(3),
                age = age(3),
            ),
            expired_heads: format!(
                "SELECT h.process_id FROM {head} h JOIN {log} l \
                   ON l.process_id = h.process_id AND l.position = h.first_retained \
                 WHERE l.published_at < {age} \
                 ORDER BY h.process_id LIMIT $2",
                age = age(1),
            ),
            // A head another writer holds is skipped, never waited for:
            // that writer touches it or takes it away.
            touch_heads: format!(
                "UPDATE {head} SET touched_at = clock_timestamp() WHERE process_id IN ( \
                   SELECT process_id FROM {head} WHERE process_id = ANY($1) \
                   ORDER BY process_id FOR UPDATE SKIP LOCKED)"
            ),
            forget_heads: format!(
                "DELETE FROM {head} WHERE process_id IN ( \
                   SELECT process_id FROM {head} \
                   WHERE first_retained > tail_position AND touched_at < {age} \
                   ORDER BY process_id LIMIT $2 FOR UPDATE SKIP LOCKED) \
                 RETURNING process_id, tail_position, reserved_bytes",
                age = age(1),
            ),
            // Hand back the whole steps (`$1` bytes each) a head reserves
            // beyond what it retains, `$2` heads at a time.
            shrink_reservations: format!(
                "WITH slack AS ( \
                   SELECT process_id, reserved_bytes AS held, \
                          ((retained_bytes + $1 - 1) / $1) * $1 AS needed \
                   FROM {head} WHERE reserved_bytes >= retained_bytes + $1 \
                   ORDER BY process_id LIMIT $2 FOR UPDATE SKIP LOCKED), \
                 shrunk AS ( \
                   UPDATE {head} h SET reserved_bytes = slack.needed FROM slack \
                   WHERE h.process_id = slack.process_id) \
                 SELECT COALESCE(sum(held - needed), 0)::bigint AS released, \
                        count(*)::bigint AS heads FROM slack"
            ),
            invalidate_head: format!(
                "UPDATE {head} SET tail_position = tail_position + 1, \
                        floor_position = tail_position + 1, first_retained = tail_position + 2, \
                        retained_events = 0, retained_bytes = 0, touched_at = clock_timestamp() \
                 WHERE process_id = $1 RETURNING tail_position"
            ),
        }
    }
}

pub(super) fn store_error(context: &str, error: impl std::fmt::Display) -> ProcessReplayStoreError {
    ProcessReplayStoreError::Store(format!("postgres process replay {context}: {error}"))
}

pub(super) fn db_error(context: &'static str) -> impl Fn(sqlx::Error) -> ProcessReplayStoreError {
    move |error| store_error(context, error)
}

/// The advisory lock key of the store's tables in `schema`: installs take it
/// exclusively and verifications shared, each for its transaction.
fn lock_key(schema: &str) -> String {
    format!("lash_process_replay:{schema}")
}

/// Create the schema and its tables when absent, by executing the published
/// DDL verbatim under `schema`, then verify them: a table an older build left
/// behind is not repaired, and its drift refuses. Replicas starting together
/// take turns through a transaction-scoped advisory lock.
pub(super) async fn install(
    pool: &PgPool,
    schema: &str,
) -> Result<PostgresProcessReplaySchemaReport, ProcessReplayStoreError> {
    let mut tx = pool.begin().await.map_err(db_error("install"))?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(lock_key(schema))
        .execute(&mut *tx)
        .await
        .map_err(db_error("install lock"))?;
    for statement in [
        format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""),
        format!("SET LOCAL search_path TO \"{schema}\""),
    ] {
        sqlx::query(&statement)
            .execute(&mut *tx)
            .await
            .map_err(db_error("install"))?;
    }
    sqlx::raw_sql(SCHEMA_DDL)
        .execute(&mut *tx)
        .await
        .map_err(db_error("install"))?;
    let report = schema_shape::verify(&mut tx, schema).await?;
    tx.commit().await.map_err(db_error("install commit"))?;
    Ok(report)
}

/// Check the tables in `schema` against the published artifact, running no
/// DDL. An install in progress finishes first.
pub(super) async fn verify(
    pool: &PgPool,
    schema: &str,
) -> Result<PostgresProcessReplaySchemaReport, ProcessReplayStoreError> {
    let mut tx = pool.begin().await.map_err(db_error("verify schema"))?;
    sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtext($1))")
        .bind(lock_key(schema))
        .execute(&mut *tx)
        .await
        .map_err(db_error("verify schema lock"))?;
    let report = schema_shape::verify(&mut tx, schema).await?;
    tx.commit()
        .await
        .map_err(db_error("verify schema commit"))?;
    Ok(report)
}

/// The incarnation the unlogged tables hold, rotating it first when their
/// sentinel is missing or names another: the history behind the old one is
/// gone, so every cursor naming it must gap.
pub(super) async fn ensure_incarnation(
    pool: &PgPool,
    sql: &Statements,
) -> Result<Incarnation, ProcessReplayStoreError> {
    let row = sqlx::query(&sql.incarnation)
        .fetch_optional(pool)
        .await
        .map_err(db_error("read incarnation"))?;
    if let Some(row) = row
        && row.get::<bool, _>("valid")
    {
        return Ok(Incarnation {
            id: row.get("incarnation_id"),
            watermark: position(row.get("watermark")),
        });
    }
    rotate(pool, sql, false).await
}

/// Rotate under the incarnation table's lock: truncate the unlogged tables,
/// mint a new incarnation, plant its sentinel and tell every replica. Unless
/// `force`d, a rotation a racing replica just finished stands.
pub(super) async fn rotate(
    pool: &PgPool,
    sql: &Statements,
    force: bool,
) -> Result<Incarnation, ProcessReplayStoreError> {
    let schema = &sql.schema;
    let table = |name: &str| format!("\"{schema}\".process_replay_{name}");
    let inc = table("incarnation");
    let mut tx = pool.begin().await.map_err(db_error("rotate"))?;
    // The row may not exist yet, so the table's lock serialises rotations.
    sqlx::query(&format!("LOCK TABLE {inc} IN SHARE ROW EXCLUSIVE MODE"))
        .execute(&mut *tx)
        .await
        .map_err(db_error("lock incarnation"))?;
    if !force
        && let Some(row) = sqlx::query(&sql.incarnation)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_error("read incarnation"))?
        && row.get::<bool, _>("valid")
    {
        return Ok(Incarnation {
            id: row.get("incarnation_id"),
            watermark: position(row.get("watermark")),
        });
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    sqlx::query(&format!(
        "TRUNCATE {}, {}, {}, {}",
        table("sentinel"),
        table("head"),
        table("log"),
        table("dedupe")
    ))
    .execute(&mut *tx)
    .await
    .map_err(db_error("truncate"))?;
    sqlx::query(&format!(
        "INSERT INTO {inc} (singleton, incarnation_id, rotations, rotated_at) \
         VALUES (true, $1, 0, now()) \
         ON CONFLICT (singleton) DO UPDATE SET incarnation_id = EXCLUDED.incarnation_id, \
           rotations = {inc}.rotations + 1, rotated_at = now()"
    ))
    .bind(&id)
    .execute(&mut *tx)
    .await
    .map_err(db_error("mint incarnation"))?;
    sqlx::query(&format!(
        "INSERT INTO {} (singleton, incarnation_id, watermark, resident_processes, reserved_bytes) \
         VALUES (true, $1, 0, 0, 0)",
        table("sentinel")
    ))
    .bind(&id)
    .execute(&mut *tx)
    .await
    .map_err(db_error("plant sentinel"))?;
    notify(&mut tx, sql, &Doorbell::all()).await?;
    tx.commit().await.map_err(db_error("rotate commit"))?;
    tracing::warn!(
        %schema,
        incarnation = %id,
        forced = force,
        "the process replay history was lost, invalidated or never existed; its incarnation rotated"
    );
    Ok(Incarnation { id, watermark: 0 })
}

/// Ring `doorbell` on the store's channel when `tx` commits.
pub(super) async fn notify(
    tx: &mut Transaction<'_, Postgres>,
    sql: &Statements,
    doorbell: &Doorbell,
) -> Result<(), ProcessReplayStoreError> {
    if doorbell.is_empty() {
        return Ok(());
    }
    sqlx::query(&sql.notify)
        .bind(&sql.channel)
        .bind(doorbell.pack()?)
        .execute(&mut **tx)
        .await
        .map_err(db_error("notify"))?;
    Ok(())
}

/// A stored position or count; the columns never hold a negative value.
pub(super) fn position(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// A position or count as its column type.
pub(super) fn column(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// The sentinel as a writer holds it under its row lock: the watermark and
/// the aggregate budget, written back when the writer changed them.
#[derive(Debug)]
pub(super) struct Sentinel {
    pub(super) incarnation: Incarnation,
    pub(super) resident: u64,
    pub(super) reserved: u64,
    dirty: bool,
}

impl Sentinel {
    /// Lock the sentinel; `None` when it is missing or names another
    /// incarnation, so the incarnation must rotate.
    pub(super) async fn lock(
        tx: &mut Transaction<'_, Postgres>,
        sql: &Statements,
    ) -> Result<Option<Self>, sqlx::Error> {
        let row = sqlx::query(&sql.lock_sentinel)
            .fetch_optional(&mut **tx)
            .await?;
        Ok(row.and_then(|row| {
            let id: Option<String> = row.get("incarnation_id");
            (id.as_deref() == Some(row.get::<&str, _>("sentinel_id"))).then(|| Self {
                incarnation: Incarnation {
                    id: id.unwrap_or_default(),
                    watermark: position(row.get("watermark")),
                },
                resident: position(row.get("resident_processes")),
                reserved: position(row.get("reserved_bytes")),
                dirty: false,
            })
        }))
    }

    /// Raise the watermark to at least `above`: a process without a head
    /// starts past every position named so far.
    pub(super) fn raise(&mut self, above: u64) {
        if above > self.incarnation.watermark {
            self.incarnation.watermark = above;
            self.dirty = true;
        }
    }

    pub(super) fn admit(&mut self, processes: u64) {
        self.resident += processes;
        self.dirty |= processes > 0;
    }

    pub(super) fn reserve(&mut self, bytes: u64) {
        self.reserved += bytes;
        self.dirty |= bytes > 0;
    }

    /// Account heads that left: their count, their reservations, and the
    /// watermark past their tails.
    pub(super) fn released(&mut self, rows: &[sqlx::postgres::PgRow]) {
        for row in rows {
            self.resident = self.resident.saturating_sub(1);
            self.reserved = self
                .reserved
                .saturating_sub(position(row.get("reserved_bytes")));
            self.raise(position(row.get("tail_position")) + 1);
            self.dirty = true;
        }
    }

    pub(super) fn release_bytes(&mut self, bytes: u64) {
        self.reserved = self.reserved.saturating_sub(bytes);
        self.dirty |= bytes > 0;
    }

    pub(super) async fn write(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        sql: &Statements,
    ) -> Result<(), sqlx::Error> {
        if !self.dirty {
            return Ok(());
        }
        sqlx::query(&sql.write_sentinel)
            .bind(column(self.incarnation.watermark))
            .bind(column(self.resident))
            .bind(column(self.reserved))
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
}

/// Delete the events and identities of heads that left the head table.
pub(super) async fn purge(
    tx: &mut Transaction<'_, Postgres>,
    sql: &Statements,
    processes: &[String],
) -> Result<(), sqlx::Error> {
    if processes.is_empty() {
        return Ok(());
    }
    for statement in [&sql.purge_log, &sql.purge_dedupe] {
        sqlx::query(statement)
            .bind(processes)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// End `process_id`'s generation: raise its floor past its tail, delete its
/// events and tell every replica, whose subscriptions to it then close. A
/// process without a head gets one whose floor is past the watermark its
/// cursors name.
pub(super) async fn invalidate(
    shared: &super::Shared,
    process_id: &ProcessId,
) -> Result<Doorbell, ProcessReplayStoreError> {
    let (pool, sql) = (&shared.pool, &shared.sql);
    let process = process_id.to_string();
    let processes = vec![process.clone()];
    for _ in 0..4 {
        let mut tx = pool.begin().await.map_err(db_error("invalidate"))?;
        let invalidated = sqlx::query(&sql.invalidate_head)
            .bind(&process)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_error("invalidate"))?;
        if let Some(row) = invalidated {
            let floor = position(row.get("tail_position"));
            sqlx::query(&sql.delete_below)
                .bind(&processes)
                .bind(vec![column(floor)])
                .execute(&mut *tx)
                .await
                .map_err(db_error("invalidate"))?;
            sqlx::query(&sql.purge_dedupe)
                .bind(&processes)
                .execute(&mut *tx)
                .await
                .map_err(db_error("invalidate"))?;
        } else {
            let Some(mut sentinel) = Sentinel::lock(&mut tx, sql)
                .await
                .map_err(db_error("invalidate"))?
            else {
                // The history is gone already: rotating gaps every cursor.
                drop(tx);
                rotate(pool, sql, false).await?;
                return Ok(Doorbell::all());
            };
            let above = sentinel.incarnation.watermark + 1;
            if sentinel.resident < shared.config.max_processes as u64 {
                let created = sqlx::query(&sql.create_heads)
                    .bind(&processes)
                    .bind(column(above))
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(db_error("invalidate"))?;
                if created.is_empty() {
                    // A writer created the head first: invalidate that one.
                    continue;
                }
                sentinel.admit(1);
            } else {
                // No room for a head: every cursor naming a process
                // without one gaps instead.
                sentinel.raise(above);
            }
            sentinel
                .write(&mut tx, sql)
                .await
                .map_err(db_error("invalidate"))?;
        }
        let doorbell = Doorbell::process(process);
        notify(&mut tx, sql, &doorbell).await?;
        tx.commit().await.map_err(db_error("invalidate commit"))?;
        return Ok(doorbell);
    }
    Err(store_error("invalidate", "the process head kept racing"))
}

/// Where a cursor for a process stands.
pub(super) enum CursorAt {
    /// After everything the window holds stamped at or before a sequence.
    Current(ProcessSequence),
    /// Before everything the window still holds.
    Earliest,
}

/// A cursor of `process_id` at `sequence`, from one snapshot of its head.
pub(super) async fn cursor(
    shared: &super::Shared,
    process_id: &ProcessId,
    sequence: ProcessSequence,
    at: CursorAt,
) -> Result<ProcessObservationCursor, ProcessReplayStoreError> {
    let stamped = match at {
        CursorAt::Current(sequence) => sequence.as_u64(),
        CursorAt::Earliest => 0,
    };
    let row = sqlx::query(&shared.sql.positions)
        .bind(process_id.as_str())
        .bind(column(stamped))
        .bind(micros(shared.config.max_age))
        .fetch_optional(&shared.pool)
        .await
        .map_err(db_error("read head"))?;
    let Some(row) = row.filter(|row| row.get::<bool, _>("valid")) else {
        let incarnation = ensure_incarnation(&shared.pool, &shared.sql).await?;
        return Ok(incarnation.cursor(process_id, sequence, incarnation.watermark));
    };
    let incarnation = Incarnation {
        id: row.get("incarnation_id"),
        watermark: position(row.get("watermark")),
    };
    let Some(tail) = row.get::<Option<i64>, _>("tail_position").map(position) else {
        return Ok(incarnation.cursor(process_id, sequence, incarnation.watermark));
    };
    let before = |name: &str| {
        row.get::<Option<i64>, _>(name)
            .map(|first| position(first).saturating_sub(1))
    };
    let live_position = match at {
        CursorAt::Current(_) => before("first_newer").unwrap_or(tail),
        CursorAt::Earliest => before("first_live").unwrap_or(tail),
    };
    Ok(incarnation.cursor(process_id, sequence, live_position))
}
