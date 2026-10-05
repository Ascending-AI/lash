//! Catalog-wide turn park feed reads (FIG-3659).

use super::*;

/// A turn park feed row: sequence, session and turn ids, park id, kind and
/// cause columns, serialized reason, the parked-at instant and the
/// recorded park build generation.
type ParkFeedRow = (
    i64,
    String,
    String,
    i64,
    String,
    Option<String>,
    Option<String>,
    i64,
    Option<String>,
    Option<i64>,
);

impl SqliteStore {
    /// The durable-core turn park feed.
    pub(crate) async fn read_turn_park_feed(
        &self,
        after: lash_core_execution::store::ParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<
        lash_core_execution::store::ParkFeedPage<lash_core_execution::store::TurnParkTarget>,
        StoreError,
    > {
        let mut page = lash_core_execution::store::ParkFeedPage {
            events: Vec::new(),
            next: after,
        };
        if !self.location.target().exists() {
            return Ok(page);
        }
        let conn = self.read_connection();
        let after_seq = i64::try_from(after.store_sequence()).unwrap_or(i64::MAX);
        let limit = i64::try_from(limit.get()).unwrap_or(i64::MAX);
        let rows: Vec<ParkFeedRow> = conn
            .read(move |conn| {
                let clock = &crate::turn_ingress::turn_ingress_sql().turn_park_clock;
                let horizon: i64 = conn
                    .query_row(clock.select_compaction_horizon.sql(), [], |row| row.get(0))
                    .optional()?
                    .unwrap_or(0);
                if after_seq < horizon {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        StoreError::ParkFeedCursorCompacted {
                            horizon:
                                lash_core_execution::store::ParkFeedCursor::from_store_sequence(
                                    u64::try_from(horizon).unwrap_or_default(),
                                ),
                        },
                    )));
                }
                let mut statement = conn.prepare_cached(
                    crate::turn_ingress::turn_ingress_sql()
                        .turn_park_events
                        .select_events_after
                        .sql(),
                )?;
                let rows = statement.query_map(params![after_seq, limit], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, Option<i64>>(9)?,
                    ))
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(sqlite_error)?;
        for (
            seq,
            session_id,
            turn_id,
            park_id,
            kind,
            cause,
            reason_json,
            at_ms,
            build_generation,
            redrive_intent,
        ) in rows
        {
            let kind = lash_core_execution::store::ParkEventKind::decode_columns(
                &kind,
                cause.as_deref(),
                reason_json.as_deref(),
                redrive_intent,
            )?;
            let build_generation = build_generation
                .map(|stored| {
                    lash_core_execution::engine::BuildGeneration::parse(&stored).map_err(|error| {
                        StoreError::StoredDataCorrupt {
                            record_kind: "TurnParkEvent",
                            message: format!(
                                "stored turn park event carries park_build_generation \
                                 `{stored}`: {error}"
                            ),
                        }
                    })
                })
                .transpose()?;
            page.events.push(lash_core_execution::store::ParkFeedEvent {
                seq: u64::try_from(seq).unwrap_or_default(),
                at_ms: u64::try_from(at_ms).unwrap_or_default(),
                target: lash_core_execution::store::TurnParkTarget {
                    session_id: SessionId::parse(session_id)?,
                    turn_id: lash_sansio::TurnId::parse(turn_id)?,
                },
                park_id: lash_core_execution::store::ParkId::from_feed_sequence(
                    u64::try_from(park_id).unwrap_or_default(),
                ),
                kind,
                build_generation,
            });
            page.next = lash_core_execution::store::ParkFeedCursor::from_store_sequence(
                u64::try_from(seq).unwrap_or_default(),
            );
        }
        Ok(page)
    }
}

pub(crate) fn next_turn_change_sequence(conn: &Connection) -> Result<i64, StoreError> {
    conn.query_row(
        crate::session_sql::session_sql()
            .turn_commits
            .next_change_seq
            .sql(),
        [],
        |row| row.get(0),
    )
    .optional()
    .map_err(sqlite_error)?
    .ok_or(StoreError::MonotonicCounterOverflow {
        counter: "turn_change_sequence",
        current: i64::MAX as u64,
    })
}

pub(crate) fn record_session_terminal(
    conn: &Connection,
    session_id: &SessionId,
    fault_json: Option<&str>,
    at_ms: i64,
) -> Result<(), StoreError> {
    let sequence = next_turn_change_sequence(conn)?;
    conn.execute(
        crate::session_sql::session_sql()
            .turn_commits
            .insert_session_terminal
            .sql(),
        params![sequence, session_id.as_str(), fault_json, at_ms],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

impl SqliteStore {
    pub(crate) async fn read_turn_changes(
        &self,
        after: lash_core_execution::store::TurnChangeCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<lash_core_execution::store::TurnChangePage, StoreError> {
        let fleet = self.conn.fleet();
        self.read_connection()
            .read(move |conn| {
                // The clock and rows share one read snapshot, including compaction.
                Ok((|| {
                    let sql = &crate::session_sql::session_sql().turn_commits;
                    let (current, horizon): (i64, i64) = conn
                        .query_row(sql.change_clock.sql(), [], |row| {
                            Ok((row.get(0)?, row.get(1)?))
                        })
                        .map_err(sqlite_error)?;
                    let current =
                        u64::try_from(current).map_err(|_| StoreError::StoredDataCorrupt {
                            record_kind: "TurnChangeClock",
                            message: "negative current sequence".to_owned(),
                        })?;
                    let horizon =
                        u64::try_from(horizon).map_err(|_| StoreError::StoredDataCorrupt {
                            record_kind: "TurnChangeClock",
                            message: "negative retention horizon".to_owned(),
                        })?;
                    after.check(current, horizon)?;
                    let mut stmt = conn
                        .prepare_cached(sql.changes_after.sql())
                        .map_err(sqlite_error)?;
                    let rows = stmt
                        .query_map(
                            params![
                                after.store_sequence() as i64,
                                i64::try_from(limit.get()).unwrap_or(i64::MAX)
                            ],
                            |row| {
                                Ok((
                                    row.get::<_, i64>(0)?,
                                    row.get::<_, String>(1)?,
                                    row.get::<_, Option<String>>(2)?,
                                    row.get::<_, Option<String>>(3)?,
                                    row.get::<_, Option<String>>(4)?,
                                    row.get::<_, i64>(5)?,
                                ))
                            },
                        )
                        .map_err(sqlite_error)?;
                    let mut changes = Vec::new();
                    for row in rows {
                        let (seq, session, operation, payload, code, at_ms) =
                            row.map_err(sqlite_error)?;
                        changes.push(lash_core_execution::store::TurnChange::from_stored(
                            seq, session, operation, payload, code, at_ms, fleet,
                        )?);
                    }
                    let next = changes.last().map_or(
                        lash_core_execution::store::TurnChangeCursor::from_store_sequence(current),
                        |change| change.cursor,
                    );
                    Ok(lash_core_execution::store::TurnChangePage {
                        changes,
                        next,
                        retained_after:
                            lash_core_execution::store::TurnChangeCursor::from_store_sequence(
                                horizon,
                            ),
                    })
                })())
            })
            .await
            .map_err(sqlite_error)?
    }
}
