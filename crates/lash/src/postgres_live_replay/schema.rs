//! The store's tables, its incarnation, and the SQL it runs.
//!
//! Three tables live in the configured schema:
//!
//! - `live_replay_incarnation`, logged: the one row naming the history the
//!   unlogged tables hold, and the position watermark of forgotten sessions.
//! - `live_replay_head`, unlogged: one row per session, the sequencer. Its
//!   `tail_position` is the last position assigned; `floor_position` is the
//!   session's generation boundary (no cursor below it continues, and
//!   invalidation raises it); `first_retained` is the lowest position still
//!   held; the counters drive per-session retention.
//! - `live_replay_log`, unlogged: the events, keyed by (session, position),
//!   unique by activity identity within the window. Its row `('', 0)` is the
//!   sentinel naming the incarnation: crash recovery and failover truncate
//!   unlogged tables, so a missing or foreign sentinel means the history is
//!   gone and the incarnation rotates.

use lash_core::{LiveReplayStoreError, SessionCursor, SessionRevision};
use lash_sansio::SessionId;
use sqlx::{PgPool, Postgres, Row as _, Transaction};

use super::codec::Doorbell;

/// The incarnation the store's cursors name, and the position a session
/// with no head starts from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Incarnation {
    pub(super) id: String,
    pub(super) watermark: u64,
}

impl Incarnation {
    pub(super) fn cursor(
        &self,
        session_id: &SessionId,
        revision: SessionRevision,
        position: u64,
    ) -> SessionCursor {
        SessionCursor::new(&self.id, session_id, revision, position)
    }
}

/// Every statement the store issues, qualified with its schema.
#[derive(Debug)]
pub(super) struct Statements {
    pub(super) channel: String,
    pub(super) incarnation: String,
    pub(super) lock_heads: String,
    pub(super) create_heads: String,
    pub(super) delivered: String,
    pub(super) expire_sessions: String,
    pub(super) delete_below: String,
    pub(super) trim_bytes: String,
    pub(super) insert_events: String,
    pub(super) append_and_update: String,
    pub(super) update_heads: String,
    pub(super) notify: String,
    pub(super) read: String,
    pub(super) load_heads: String,
    pub(super) load_runs: String,
    pub(super) expired_heads: String,
    pub(super) forget_heads: String,
    pub(super) raise_watermark: String,
    pub(super) invalidate_head: String,
    pub(super) create_invalidated_head: String,
}

/// Microseconds of a duration, for `$n * interval '1 microsecond'`.
pub(super) fn micros(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
}

impl Statements {
    pub(super) fn new(schema: &str) -> Self {
        let inc = format!("\"{schema}\".live_replay_incarnation");
        let head = format!("\"{schema}\".live_replay_head");
        let log = format!("\"{schema}\".live_replay_log");
        let head_columns = "session_id, tail_position, floor_position, first_retained, \
                            retained_events, retained_bytes";
        let valid = format!(
            "(SELECT s.payload = convert_to(i.incarnation_id, 'UTF8') FROM {log} s \
              WHERE s.session_id = '' AND s.position = 0) IS TRUE"
        );
        Self {
            channel: schema.to_string(),
            incarnation: format!(
                "SELECT i.incarnation_id, i.watermark, {valid} AS valid FROM {inc} i"
            ),
            // The first retained row's age decides whether this publication
            // expires the session's oldest events first.
            lock_heads: format!(
                "SELECT h.{cols}, \
                        (SELECT l.published_at < statement_timestamp() - $2 * interval '1 microsecond' \
                           FROM {log} l WHERE l.session_id = h.session_id \
                            AND l.position = h.first_retained) IS TRUE AS expiring, \
                        i.incarnation_id, i.watermark, {valid} AS valid \
                 FROM {head} h CROSS JOIN {inc} i WHERE h.session_id = ANY($1) \
                 ORDER BY h.session_id FOR UPDATE OF h",
                cols = head_columns.replace(", ", ", h."),
            ),
            create_heads: format!(
                "INSERT INTO {head} ({head_columns}, touched_at) \
                 SELECT sid, i.watermark, i.watermark, i.watermark + 1, 0, 0, clock_timestamp() \
                 FROM unnest($1::text[]) AS sid CROSS JOIN {inc} i \
                 ON CONFLICT (session_id) DO NOTHING \
                 RETURNING {head_columns}, false AS expiring, \
                   (SELECT incarnation_id FROM {inc}) AS incarnation_id, \
                   (SELECT watermark FROM {inc}) AS watermark, \
                   (SELECT {valid} FROM {inc} i) AS valid"
            ),
            // A key's delivered spans are disjoint, so its span runs from
            // the first ordinal of its lowest span to the last of its
            // highest: two index probes per key.
            delivered: format!(
                "SELECT k.s AS session_id, k.key AS activity_key, NULL::text AS activity_id, \
                        (SELECT l.activity_first FROM {log} l \
                          WHERE l.session_id = k.s AND l.activity_key = k.key \
                          ORDER BY l.activity_first LIMIT 1) AS first, \
                        (SELECT l.activity_last FROM {log} l \
                          WHERE l.session_id = k.s AND l.activity_key = k.key \
                          ORDER BY l.activity_first DESC LIMIT 1) AS last \
                 FROM unnest($1::text[], $2::text[]) AS k(s, key) \
                 UNION ALL \
                 SELECT l.session_id, NULL, l.activity_id, NULL, NULL \
                 FROM {log} l JOIN unnest($3::text[], $4::text[]) AS k(s, id) \
                   ON l.session_id = k.s AND l.activity_id = k.id AND l.activity_key IS NULL"
            ),
            expire_sessions: format!(
                "DELETE FROM {log} WHERE session_id = ANY($1) AND position > 0 \
                   AND published_at < statement_timestamp() - $2 * interval '1 microsecond' \
                 RETURNING session_id, position, bytes"
            ),
            delete_below: format!(
                "DELETE FROM {log} l USING unnest($1::text[], $2::bigint[]) AS c(s, below) \
                 WHERE l.session_id = c.s AND l.position < c.below \
                 RETURNING l.session_id, l.position, l.bytes"
            ),
            trim_bytes: format!(
                "DELETE FROM {log} l USING ( \
                   SELECT session_id, position FROM ( \
                     SELECT session_id, position, \
                            sum(bytes) OVER (PARTITION BY session_id ORDER BY position DESC) AS suffix \
                     FROM {log} WHERE session_id = ANY($1)) w \
                   WHERE suffix > $2) d \
                 WHERE l.session_id = d.session_id AND l.position = d.position \
                 RETURNING l.session_id, l.position, l.bytes"
            ),
            insert_events: format!(
                "INSERT INTO {log} (session_id, position, revision, turn_id, activity_id, \
                                    activity_key, activity_first, activity_last, payload, bytes, \
                                    published_at) \
                 SELECT s, p, r, t, a, k, f, z, d, b, clock_timestamp() \
                 FROM unnest($1::text[], $2::bigint[], $3::bigint[], $4::text[], $5::text[], \
                             $6::text[], $7::bigint[], $8::bigint[], $9::bytea[], $10::bigint[]) \
                      AS e(s, p, r, t, a, k, f, z, d, b)"
            ),
            // The common tick: no row leaves the window, so the append, the
            // heads and the doorbell are one statement.
            append_and_update: format!(
                "WITH appended AS (INSERT INTO {log} (session_id, position, revision, turn_id, activity_id, \
                                    activity_key, activity_first, activity_last, payload, bytes, \
                                    published_at) \
                 SELECT s, p, r, t, a, k, f, z, d, b, clock_timestamp() \
                 FROM unnest($1::text[], $2::bigint[], $3::bigint[], $4::text[], $5::text[], \
                             $6::text[], $7::bigint[], $8::bigint[], $9::bytea[], $10::bigint[]) \
                      AS e(s, p, r, t, a, k, f, z, d, b)), \
                      updated AS (UPDATE {head} h SET tail_position = u.tail, floor_position = u.floor, \
                        first_retained = u.first, retained_events = u.events, \
                        retained_bytes = u.bytes, touched_at = clock_timestamp() \
                 FROM unnest($11::text[], $12::bigint[], $13::bigint[], $14::bigint[], \
                             $15::bigint[], $16::bigint[]) AS u(s, tail, floor, first, events, bytes) \
                 WHERE h.session_id = u.s) \
                 SELECT pg_notify($17, payload) FROM unnest($18::text[]) AS payload"
            ),
            update_heads: format!(
                "WITH updated AS (UPDATE {head} h SET tail_position = u.tail, floor_position = u.floor, \
                        first_retained = u.first, retained_events = u.events, \
                        retained_bytes = u.bytes, touched_at = clock_timestamp() \
                 FROM unnest($1::text[], $2::bigint[], $3::bigint[], $4::bigint[], \
                             $5::bigint[], $6::bigint[]) AS u(s, tail, floor, first, events, bytes) \
                 WHERE h.session_id = u.s) \
                 SELECT pg_notify($7, payload) FROM unnest($8::text[]) AS payload"
            ),
            notify: "SELECT pg_notify($1, payload) FROM unnest($2::text[]) AS payload".to_string(),
            // One statement, so the head, the window's age cut and the rows
            // are one snapshot.
            read: format!(
                "SELECT i.incarnation_id, i.watermark, {valid} AS valid, \
                        h.tail_position, h.floor_position, \
                        COALESCE((SELECT l.position FROM {log} l \
                                   WHERE l.session_id = h.session_id \
                                     AND l.position >= h.first_retained \
                                     AND l.published_at >= statement_timestamp() - $3 * interval '1 microsecond' \
                                   ORDER BY l.position LIMIT 1), \
                                 h.tail_position + 1) AS first_live, \
                        e.position, e.revision, e.turn_id, e.payload \
                 FROM {inc} i \
                 LEFT JOIN {head} h ON h.session_id = $1 \
                 LEFT JOIN LATERAL (SELECT l.position, l.revision, l.turn_id, l.payload \
                                    FROM {log} l WHERE l.session_id = $1 AND l.position > $2 \
                                    ORDER BY l.position LIMIT $4) e ON true \
                 ORDER BY e.position"
            ),
            load_heads: format!(
                "SELECT session_id, tail_position, floor_position, first_retained FROM {head}"
            ),
            // Runs of one revision: gaps and islands over each session's
            // positions.
            load_runs: format!(
                "SELECT session_id, min(position) AS first, max(position) AS last, revision \
                 FROM (SELECT session_id, position, revision, \
                              position - row_number() OVER (PARTITION BY session_id, revision \
                                                            ORDER BY position) AS island \
                       FROM {log} WHERE session_id <> '') runs \
                 GROUP BY session_id, revision, island"
            ),
            expired_heads: format!(
                "SELECT h.session_id FROM {head} h JOIN {log} l \
                   ON l.session_id = h.session_id AND l.position = h.first_retained \
                 WHERE l.published_at < statement_timestamp() - $1 * interval '1 microsecond' \
                 ORDER BY h.session_id LIMIT $2"
            ),
            forget_heads: format!(
                "DELETE FROM {head} WHERE session_id IN ( \
                   SELECT session_id FROM {head} \
                   WHERE first_retained > tail_position \
                     AND touched_at < statement_timestamp() - $1 * interval '1 microsecond' \
                   ORDER BY session_id LIMIT $2 FOR UPDATE SKIP LOCKED) \
                 RETURNING session_id, tail_position"
            ),
            raise_watermark: format!(
                "UPDATE {inc} SET watermark = greatest(watermark, $1) RETURNING watermark"
            ),
            invalidate_head: format!(
                "UPDATE {head} SET tail_position = tail_position + 1, \
                        floor_position = tail_position + 1, first_retained = tail_position + 2, \
                        retained_events = 0, retained_bytes = 0, touched_at = clock_timestamp() \
                 WHERE session_id = $1 RETURNING tail_position"
            ),
            create_invalidated_head: format!(
                "INSERT INTO {head} ({head_columns}, touched_at) \
                 SELECT $1, i.watermark + 1, i.watermark + 1, i.watermark + 2, 0, 0, clock_timestamp() \
                 FROM {inc} i ON CONFLICT (session_id) DO NOTHING RETURNING tail_position"
            ),
        }
    }
}

fn store_error(context: &str, error: impl std::fmt::Display) -> LiveReplayStoreError {
    LiveReplayStoreError::Store(format!("postgres live replay {context}: {error}"))
}

pub(super) fn db_error(context: &'static str) -> impl Fn(sqlx::Error) -> LiveReplayStoreError {
    move |error| store_error(context, error)
}

/// Create the schema and tables when absent. Replicas starting together
/// take turns through a transaction-scoped advisory lock.
pub(super) async fn install(pool: &PgPool, schema: &str) -> Result<(), LiveReplayStoreError> {
    let mut tx = pool.begin().await.map_err(db_error("install"))?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("lash_live_replay:{schema}"))
        .execute(&mut *tx)
        .await
        .map_err(db_error("install lock"))?;
    let ddl = [
        format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""),
        format!(
            "CREATE TABLE IF NOT EXISTS \"{schema}\".live_replay_incarnation ( \
               singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton), \
               incarnation_id text NOT NULL, \
               watermark bigint NOT NULL, \
               rotations bigint NOT NULL, \
               rotated_at timestamptz NOT NULL)"
        ),
        format!(
            "CREATE UNLOGGED TABLE IF NOT EXISTS \"{schema}\".live_replay_head ( \
               session_id text PRIMARY KEY, \
               tail_position bigint NOT NULL, \
               floor_position bigint NOT NULL, \
               first_retained bigint NOT NULL, \
               retained_events bigint NOT NULL, \
               retained_bytes bigint NOT NULL, \
               touched_at timestamptz NOT NULL)"
        ),
        format!(
            "CREATE UNLOGGED TABLE IF NOT EXISTS \"{schema}\".live_replay_log ( \
               session_id text NOT NULL, \
               position bigint NOT NULL, \
               revision bigint NOT NULL, \
               turn_id text, \
               activity_id text, \
               activity_key text, \
               activity_first bigint, \
               activity_last bigint, \
               payload bytea NOT NULL, \
               bytes bigint NOT NULL, \
               published_at timestamptz NOT NULL, \
               PRIMARY KEY (session_id, position))"
        ),
        format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS live_replay_log_activity \
             ON \"{schema}\".live_replay_log (session_id, activity_id) \
             WHERE activity_key IS NULL AND activity_id IS NOT NULL"
        ),
        format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS live_replay_log_span \
             ON \"{schema}\".live_replay_log (session_id, activity_key, activity_first) \
             WHERE activity_key IS NOT NULL"
        ),
    ];
    for statement in ddl {
        sqlx::query(&statement)
            .execute(&mut *tx)
            .await
            .map_err(db_error("install"))?;
    }
    tx.commit().await.map_err(db_error("install commit"))
}

/// The incarnation the unlogged tables hold, rotating it first when their
/// sentinel is missing or names another: the history behind the old one is
/// gone, so every cursor naming it must gap (P5).
pub(super) async fn ensure_incarnation(
    pool: &PgPool,
    sql: &Statements,
) -> Result<Incarnation, LiveReplayStoreError> {
    if let Some(incarnation) = current_incarnation(pool, sql).await? {
        return Ok(incarnation);
    }
    let mut tx = pool.begin().await.map_err(db_error("rotate"))?;
    let rotated = rotate(&mut tx, sql).await?;
    tx.commit().await.map_err(db_error("rotate commit"))?;
    Ok(rotated)
}

/// The current incarnation when its sentinel holds; `None` when it must
/// rotate.
async fn current_incarnation(
    pool: &PgPool,
    sql: &Statements,
) -> Result<Option<Incarnation>, LiveReplayStoreError> {
    let row = sqlx::query(&sql.incarnation)
        .fetch_optional(pool)
        .await
        .map_err(db_error("read incarnation"))?;
    Ok(row.and_then(|row| {
        row.get::<bool, _>("valid").then(|| Incarnation {
            id: row.get("incarnation_id"),
            watermark: position(row.get("watermark")),
        })
    }))
}

/// Rotate under the incarnation row's lock, unless a racing replica just
/// did: it truncates the unlogged tables, mints a new incarnation with a
/// zero watermark, plants its sentinel and tells every replica.
async fn rotate(
    tx: &mut Transaction<'_, Postgres>,
    sql: &Statements,
) -> Result<Incarnation, LiveReplayStoreError> {
    let channel = &sql.channel;
    let inc = format!("\"{channel}\".live_replay_incarnation");
    let head = format!("\"{channel}\".live_replay_head");
    let log = format!("\"{channel}\".live_replay_log");
    sqlx::query(&format!("SELECT 1 FROM {inc} FOR UPDATE"))
        .execute(&mut **tx)
        .await
        .map_err(db_error("lock incarnation"))?;
    if let Some(row) = sqlx::query(&sql.incarnation)
        .fetch_optional(&mut **tx)
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
    sqlx::query(&format!("TRUNCATE {head}, {log}"))
        .execute(&mut **tx)
        .await
        .map_err(db_error("truncate"))?;
    sqlx::query(&format!(
        "INSERT INTO {inc} (singleton, incarnation_id, watermark, rotations, rotated_at) \
         VALUES (true, $1, 0, 0, now()) \
         ON CONFLICT (singleton) DO UPDATE SET incarnation_id = EXCLUDED.incarnation_id, \
           watermark = 0, rotations = {inc}.rotations + 1, rotated_at = now()"
    ))
    .bind(&id)
    .execute(&mut **tx)
    .await
    .map_err(db_error("mint incarnation"))?;
    sqlx::query(&format!(
        "INSERT INTO {log} (session_id, position, revision, payload, bytes, published_at) \
         VALUES ('', 0, 0, convert_to($1, 'UTF8'), 0, now())"
    ))
    .bind(&id)
    .execute(&mut **tx)
    .await
    .map_err(db_error("plant sentinel"))?;
    notify(
        tx,
        sql,
        &[Doorbell::Rotated {
            incarnation: id.clone(),
        }],
    )
    .await?;
    tracing::warn!(
        schema = %channel,
        incarnation = %id,
        "the live replay history was lost or never existed; its incarnation rotated"
    );
    Ok(Incarnation { id, watermark: 0 })
}

/// Send `doorbells` on the store's channel when `tx` commits, packed into
/// payloads under PostgreSQL's 8000-byte limit.
pub(super) async fn notify(
    tx: &mut Transaction<'_, Postgres>,
    sql: &Statements,
    doorbells: &[Doorbell],
) -> Result<(), LiveReplayStoreError> {
    if doorbells.is_empty() {
        return Ok(());
    }
    let payloads = super::codec::pack_doorbells(doorbells)?;
    sqlx::query(&sql.notify)
        .bind(&sql.channel)
        .bind(&payloads)
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

/// End `session_id`'s generation: raise its floor past its tail (creating
/// the head above the watermark when it has none), delete its events and
/// tell every replica, whose subscriptions to it then close (P11).
pub(super) async fn invalidate(
    pool: &PgPool,
    sql: &Statements,
    session_id: &SessionId,
) -> Result<Vec<Doorbell>, LiveReplayStoreError> {
    let session = session_id.to_string();
    let mut tx = pool.begin().await.map_err(db_error("invalidate"))?;
    let mut floor = None;
    for _ in 0..4 {
        for statement in [&sql.invalidate_head, &sql.create_invalidated_head] {
            if let Some(row) = sqlx::query(statement)
                .bind(&session)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_error("invalidate"))?
            {
                floor = Some(position(row.get("tail_position")));
                break;
            }
        }
        if floor.is_some() {
            break;
        }
    }
    let floor = floor.ok_or_else(|| store_error("invalidate", "the session head kept racing"))?;
    sqlx::query(&sql.delete_below)
        .bind(vec![session.clone()])
        .bind(vec![column(floor)])
        .execute(&mut *tx)
        .await
        .map_err(db_error("invalidate"))?;
    let doorbells = vec![Doorbell::Invalidated { session, floor }];
    notify(&mut tx, sql, &doorbells).await?;
    tx.commit().await.map_err(db_error("invalidate commit"))?;
    Ok(doorbells)
}
