use super::*;
use crate::session_sql::session_sql;
use lash_sansio::SessionId;
use lash_sansio::TurnId;

pub(crate) use lash_core_execution::store_backend_support::SessionMetaWrite;
use lash_core_execution::store_backend_support::{CausalColumns, SessionMetaCodec, StoredRelation};

const SESSION_META_CODEC: SessionMetaCodec = SessionMetaCodec::new("SQLite INTEGER");

pub(crate) fn stored_relation_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<StoredRelation> {
    Ok(StoredRelation {
        session_id: SessionId::from(row.get::<_, String>(0)?),
        relation_kind: row.get(1)?,
        parent_session_id: row.get::<_, Option<String>>(2)?.map(SessionId::from),
        cause: CausalColumns {
            kind: row.get(3)?,
            session_id: row.get::<_, Option<String>>(4)?.map(SessionId::from),
            turn_id: row
                .get::<_, Option<String>>(5)?
                .map(lash_core_execution::TurnId::from),
            effect_id: row.get(6)?,
            call_id: row.get(7)?,
            process_id: row
                .get::<_, Option<String>>(8)?
                .map(|value| crate::sql_process_id(8, value))
                .transpose()?,
            process_event_sequence: row.get(9)?,
            occurrence_id: row.get(10)?,
            subscription_id: row.get(11)?,
            subscription_incarnation: row.get(12)?,
            subscription_revision: row.get(13)?,
            node_id: row.get(14)?,
        },
        source_session_id: row.get::<_, Option<String>>(15)?.map(SessionId::from),
        source_node_id: row.get(16)?,
        pending_observer_intents: Vec::new(),
    })
}

pub(crate) fn decode_catalog_relation(
    stored: StoredRelation,
    observer_intent_rows_json: &str,
) -> Result<lash_core_execution::SessionRelation, StoreError> {
    let observer_intent_rows =
        serde_json::from_str(observer_intent_rows_json).map_err(|error| {
            SessionMetaCodec::corrupt(
                SESSION_META_CODEC,
                format!("invalid observer-intent process rows JSON: {error}"),
            )
        })?;
    Ok(SessionMetaCodec::decode_with_process_rows(
        SESSION_META_CODEC,
        stored,
        observer_intent_rows,
    )?
    .relation)
}

pub(crate) fn write_session_meta(
    conn: &Connection,
    meta: &SessionMeta,
    mode: SessionMetaWrite,
    created_at_ms: u64,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<bool, StoreError> {
    let stored = SessionMetaCodec::encode(SESSION_META_CODEC, meta)?;
    let sql = match mode {
        SessionMetaWrite::Insert => session_sql().meta_sqlite.insert.sql(),
        SessionMetaWrite::Replace => session_sql().meta_sqlite.upsert.sql(),
    };
    let changed = conn
        .execute(
            sql,
            params![
                stored.session_id.as_str(),
                stored.relation_kind,
                stored.parent_session_id.as_deref(),
                stored.cause.kind,
                stored.cause.session_id.as_deref(),
                stored.cause.turn_id.as_ref().map(TurnId::as_str),
                stored.cause.effect_id,
                stored.cause.call_id,
                stored.cause.process_id.as_deref(),
                stored.cause.process_event_sequence,
                stored.cause.occurrence_id,
                stored.cause.subscription_id,
                stored.cause.subscription_incarnation,
                stored.cause.subscription_revision,
                stored.cause.node_id,
                stored.source_session_id.as_deref(),
                stored.source_node_id,
                crate::clamp_epoch_ms(created_at_ms),
                fleet_format
                    .writer_version(lash_core_execution::store::CURRENT_SESSION_STATE_VERSION),
            ],
        )
        .map_err(sqlite_error)?;
    if changed == 0 {
        return Ok(false);
    }
    conn.execute(
        session_sql().observer_intents.delete_by_session.sql(),
        params![stored.session_id.as_str()],
    )
    .map_err(sqlite_error)?;
    for (process_index, intent) in stored.pending_observer_intents.iter().enumerate() {
        conn.execute(
            session_sql().observer_intents.insert.sql(),
            params![
                stored.session_id.as_str(),
                SessionMetaCodec::write_index(
                    SESSION_META_CODEC,
                    process_index,
                    "observer-intent process"
                )?,
                intent.process_id.as_str(),
            ],
        )
        .map_err(sqlite_error)?;
    }
    Ok(true)
}

/// Read the durable lineage recorded for `session_id`, if the row exists.
///
/// Admission uses this inside its own transaction, so it must not open a
/// nested one: it selects the four lineage columns and nothing else.
pub(crate) fn load_recorded_lineage(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<lash_core_execution::SessionLineage>, StoreError> {
    let row = conn
        .query_row(
            session_sql().meta.select_lineage.sql(),
            params![session_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()
        .map_err(sqlite_error)?;
    row.map(
        |(relation_kind, parent_session_id, source_session_id, source_node_id)| {
            SESSION_META_CODEC.decode_lineage(
                &relation_kind,
                parent_session_id.map(SessionId::from),
                source_session_id.map(SessionId::from),
                source_node_id,
            )
        },
    )
    .transpose()
}

pub(crate) fn load_session_meta(
    conn: &Connection,
    selected_session_id: Option<&SessionId>,
) -> Result<Option<SessionMeta>, StoreError> {
    let tx = conn.unchecked_transaction().map_err(sqlite_error)?;
    let session_id = if let Some(session_id) = selected_session_id {
        session_id.to_string()
    } else {
        let mut stmt = tx
            .prepare(session_sql().meta_sqlite.select_sole_session_id.sql())
            .map_err(sqlite_error)?;
        let session_ids = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sqlite_error)?;
        drop(stmt);
        if session_ids.len() != 1 {
            tx.commit().map_err(sqlite_error)?;
            return Ok(None);
        }
        #[expect(
            clippy::expect_used,
            reason = "the `len() != 1` guard above returned, so exactly one id remains"
        )]
        let session_id = session_ids.into_iter().next().expect("one session id");
        session_id
    };
    let mut stored = tx
        .query_row(
            session_sql().meta_sqlite.select_relation.sql(),
            params![session_id],
            stored_relation_from_row,
        )
        .optional()
        .map_err(sqlite_error)?;
    let Some(mut stored) = stored.take() else {
        tx.commit().map_err(sqlite_error)?;
        return Ok(None);
    };
    let mut stmt = tx
        .prepare(session_sql().observer_intents.select_for_session.sql())
        .map_err(sqlite_error)?;
    let observer_rows = stmt
        .query_map(params![stored.session_id.as_str()], |row| {
            Ok((row.get::<_, i64>(0)?, crate::row_process_id(row, 1)?))
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    drop(stmt);
    for (process_index, process_id) in observer_rows {
        if SessionMetaCodec::read_index(
            SESSION_META_CODEC,
            process_index,
            "observer-intent process_index",
        )? != stored.pending_observer_intents.len()
        {
            return Err(SessionMetaCodec::corrupt(
                SESSION_META_CODEC,
                "observer-intent process indexes are not contiguous",
            ));
        }
        stored
            .pending_observer_intents
            .push(lash_core_execution::store_backend_support::StoredObserverIntent { process_id });
    }
    let meta = SessionMetaCodec::decode(SESSION_META_CODEC, stored)?;
    tx.commit().map_err(sqlite_error)?;
    Ok(Some(meta))
}

/// Retain `checkpoint_ref` as the checkpoint session `session_id`'s latest
/// turn was admitted on, replacing the previous admission's (FIG-3682).
///
/// A session with no metadata row yet has committed nothing, so its admission
/// base names no checkpoint and there is nothing to retain.
pub(crate) fn retain_admission_base_conn(
    conn: &rusqlite::Connection,
    session_id: &SessionId,
    checkpoint_ref: Option<&lash_core_execution::store::BlobRef>,
) -> Result<(), StoreError> {
    conn.execute(
        session_sql().meta.retain_admission_base.sql(),
        rusqlite::params![
            session_id.as_str(),
            checkpoint_ref.map(|blob_ref| blob_ref.as_str())
        ],
    )
    .map_err(sqlite_error)?;
    Ok(())
}
