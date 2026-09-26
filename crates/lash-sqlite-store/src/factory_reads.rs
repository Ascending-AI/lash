//! The catalog-wide reads a SQLite session-store factory answers without a
//! bound session: the turn park feed (FIG-3659), a logical root's terminal
//! evidence and the open control intents (FIG-3600 S7).

use super::*;

/// A turn park feed row: sequence, session and turn ids, park id, kind and
/// cause columns, serialized reason and the parked-at instant.
type ParkFeedRow = (
    i64,
    String,
    String,
    i64,
    String,
    Option<String>,
    Option<String>,
    i64,
);

impl SqliteSessionStoreFactory {
    /// [`SessionStoreFactory::turn_park_feed`] over the durable core.
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
        if !self.core.target().exists() {
            return Ok(page);
        }
        let conn = SqliteConnection::open_readonly(self.core.target())
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
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
                let mut statement = conn.prepare(
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
                    ))
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(sqlite_error)?;
        for (seq, session_id, turn_id, park_id, kind, cause, reason_json, at_ms) in rows {
            let kind = lash_core_execution::store::ParkEventKind::decode_columns(
                &kind,
                cause.as_deref(),
                reason_json.as_deref(),
            )?;
            page.events.push(lash_core_execution::store::ParkFeedEvent {
                seq: u64::try_from(seq).unwrap_or_default(),
                at_ms: u64::try_from(at_ms).unwrap_or_default(),
                target: lash_core_execution::store::TurnParkTarget {
                    session_id: SessionId::from(session_id),
                    turn_id: lash_sansio::TurnId::from(turn_id),
                },
                park_id: lash_core_execution::store::ParkId::from_feed_sequence(
                    u64::try_from(park_id).unwrap_or_default(),
                ),
                kind,
            });
            page.next = lash_core_execution::store::ParkFeedCursor::from_store_sequence(
                u64::try_from(seq).unwrap_or_default(),
            );
        }
        Ok(page)
    }

    /// [`SessionStoreFactory::root_terminal`] over the durable core: the
    /// root's own evidence, else the session's `close_session` tombstone,
    /// which answers every root of a deleted session.
    pub(crate) async fn read_root_terminal(
        &self,
        session_id: &SessionId,
        root: &lash_sansio::TurnId,
    ) -> Result<Option<lash_core_execution::store::RootTerminal>, StoreError> {
        if !self.core.target().exists() {
            return Ok(None);
        }
        let conn = SqliteConnection::open_readonly(self.core.target())
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let session_id = session_id.clone();
        let root = root.clone();
        conn.read(move |conn| {
            Ok((|| {
                if let Some(terminal) =
                    crate::session_roots::root_terminal_conn(conn, &session_id, &root)?
                {
                    return Ok(Some(terminal));
                }
                Ok(
                    crate::session_roots::close_session_intent_conn(conn, &session_id)?
                        .and_then(|intent| intent.session_deleted_terminal(&root)),
                )
            })())
        })
        .await
        .map_err(sqlite_error)?
    }

    /// [`SessionStoreFactory::list_open_control_intents`] over the durable
    /// core.
    pub(crate) async fn read_open_control_intents(
        &self,
        after: Option<lash_core_execution::store::ControlIntentId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<lash_core_execution::store::ControlIntent>, StoreError> {
        if !self.core.target().exists() {
            return Ok(Vec::new());
        }
        let conn = SqliteConnection::open_readonly(self.core.target())
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        conn.read(move |conn| {
            Ok(crate::session_roots::open_control_intents_conn(
                conn,
                after,
                limit.get(),
            ))
        })
        .await
        .map_err(sqlite_error)?
    }
}
