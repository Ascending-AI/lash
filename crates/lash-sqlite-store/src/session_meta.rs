use super::*;

pub(crate) use lash_core::store_backend_support::SessionMetaWrite;
use lash_core::store_backend_support::{CausalColumns, SessionMetaCodec, StoredRelation};

const SESSION_META_CODEC: SessionMetaCodec = SessionMetaCodec::new("SQLite INTEGER");

pub(crate) fn stored_relation_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<StoredRelation> {
    Ok(StoredRelation {
        session_id: row.get(0)?,
        relation_kind: row.get(1)?,
        parent_session_id: row.get(2)?,
        cause: CausalColumns {
            kind: row.get(3)?,
            session_id: row.get(4)?,
            turn_id: row.get(5)?,
            effect_id: row.get(6)?,
            call_id: row.get(7)?,
            process_id: row.get(8)?,
            process_event_sequence: row.get(9)?,
            occurrence_id: row.get(10)?,
            subscription_id: row.get(11)?,
            subscription_incarnation: row.get(12)?,
            subscription_revision: row.get(13)?,
            node_id: row.get(14)?,
        },
        source_session_id: row.get(15)?,
        source_node_id: row.get(16)?,
        observer_inheritance_kind: row.get(17)?,
        pending_observer_intents: Vec::new(),
        fork_inheritance_processes: Vec::new(),
    })
}

pub(crate) fn decode_catalog_relation(
    stored: StoredRelation,
    observer_intent_rows_json: &str,
    fork_inheritance_rows_json: &str,
) -> Result<lash_core::SessionRelation, StoreError> {
    let observer_intent_rows =
        serde_json::from_str(observer_intent_rows_json).map_err(|error| {
            SessionMetaCodec::corrupt(
                SESSION_META_CODEC,
                format!("invalid observer-intent process rows JSON: {error}"),
            )
        })?;
    let fork_inheritance_rows =
        serde_json::from_str(fork_inheritance_rows_json).map_err(|error| {
            SessionMetaCodec::corrupt(
                SESSION_META_CODEC,
                format!("invalid fork inheritance process rows JSON: {error}"),
            )
        })?;
    Ok(SessionMetaCodec::decode_with_process_rows(
        SESSION_META_CODEC,
        stored,
        observer_intent_rows,
        fork_inheritance_rows,
    )?
    .relation)
}

const SELECT_COLUMNS: &str = "session_id, relation_kind, parent_session_id,
    caused_by_kind, caused_by_session_id, caused_by_turn_id,
    caused_by_effect_id, caused_by_call_id, caused_by_process_id,
    caused_by_process_event_sequence, caused_by_occurrence_id,
    caused_by_subscription_id, caused_by_subscription_incarnation,
    caused_by_subscription_revision, caused_by_node_id, source_session_id,
    source_node_id, observer_inheritance_kind";

pub(crate) fn write_session_meta(
    conn: &Connection,
    meta: &SessionMeta,
    mode: SessionMetaWrite,
    created_at_ms: u64,
) -> Result<bool, StoreError> {
    let stored = SessionMetaCodec::encode(SESSION_META_CODEC, meta)?;
    let sql = match mode {
        SessionMetaWrite::Insert => {
            "INSERT OR IGNORE INTO session_meta
             (session_id, session_state_version, relation_kind, parent_session_id,
              caused_by_kind, caused_by_session_id, caused_by_turn_id,
              caused_by_effect_id, caused_by_call_id, caused_by_process_id,
              caused_by_process_event_sequence, caused_by_occurrence_id,
              caused_by_subscription_id, caused_by_subscription_incarnation,
              caused_by_subscription_revision, caused_by_node_id, source_session_id,
              source_node_id, observer_inheritance_kind, created_at_ms, last_commit_at_ms)
             VALUES (?1, ?20, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     ?13, ?14, ?15, ?16, ?17, ?18, ?19, NULL)"
        }
        SessionMetaWrite::Replace => {
            "INSERT INTO session_meta
             (session_id, session_state_version, relation_kind, parent_session_id,
              caused_by_kind, caused_by_session_id, caused_by_turn_id,
              caused_by_effect_id, caused_by_call_id, caused_by_process_id,
              caused_by_process_event_sequence, caused_by_occurrence_id,
              caused_by_subscription_id, caused_by_subscription_incarnation,
              caused_by_subscription_revision, caused_by_node_id, source_session_id,
              source_node_id, observer_inheritance_kind, created_at_ms, last_commit_at_ms)
             VALUES (?1, ?20, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     ?13, ?14, ?15, ?16, ?17, ?18, ?19, NULL)
             ON CONFLICT(session_id) DO UPDATE SET
               relation_kind = excluded.relation_kind,
               parent_session_id = excluded.parent_session_id,
               caused_by_kind = excluded.caused_by_kind,
               caused_by_session_id = excluded.caused_by_session_id,
               caused_by_turn_id = excluded.caused_by_turn_id,
               caused_by_effect_id = excluded.caused_by_effect_id,
               caused_by_call_id = excluded.caused_by_call_id,
               caused_by_process_id = excluded.caused_by_process_id,
               caused_by_process_event_sequence = excluded.caused_by_process_event_sequence,
               caused_by_occurrence_id = excluded.caused_by_occurrence_id,
               caused_by_subscription_id = excluded.caused_by_subscription_id,
               caused_by_subscription_incarnation = excluded.caused_by_subscription_incarnation,
               caused_by_subscription_revision = excluded.caused_by_subscription_revision,
               caused_by_node_id = excluded.caused_by_node_id,
               source_session_id = excluded.source_session_id,
               source_node_id = excluded.source_node_id,
               observer_inheritance_kind = excluded.observer_inheritance_kind"
        }
    };
    let changed = conn
        .execute(
            sql,
            params![
                stored.session_id,
                stored.relation_kind,
                stored.parent_session_id,
                stored.cause.kind,
                stored.cause.session_id,
                stored.cause.turn_id,
                stored.cause.effect_id,
                stored.cause.call_id,
                stored.cause.process_id,
                stored.cause.process_event_sequence,
                stored.cause.occurrence_id,
                stored.cause.subscription_id,
                stored.cause.subscription_incarnation,
                stored.cause.subscription_revision,
                stored.cause.node_id,
                stored.source_session_id,
                stored.source_node_id,
                stored.observer_inheritance_kind,
                crate::clamp_epoch_ms(created_at_ms),
                lash_core::store::CURRENT_SESSION_STATE_VERSION,
            ],
        )
        .map_err(sqlite_error)?;
    if changed == 0 {
        return Ok(false);
    }
    for table in [
        "session_meta_pending_observer_intents",
        "session_meta_fork_inheritance_processes",
    ] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE session_id = ?1"),
            params![stored.session_id],
        )
        .map_err(sqlite_error)?;
    }
    for (process_index, intent) in stored.pending_observer_intents.iter().enumerate() {
        conn.execute(
            "INSERT INTO session_meta_pending_observer_intents
             (session_id, process_index, process_id, process_incarnation, attribution)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                stored.session_id,
                SessionMetaCodec::write_index(
                    SESSION_META_CODEC,
                    process_index,
                    "observer-intent process"
                )?,
                intent.process_id,
                intent.process_incarnation,
                intent.attribution,
            ],
        )
        .map_err(sqlite_error)?;
    }
    write_process_list(
        conn,
        "session_meta_fork_inheritance_processes",
        &stored.session_id,
        &stored.fork_inheritance_processes,
    )?;
    Ok(true)
}

pub(crate) fn load_session_meta(
    conn: &Connection,
    selected_session_id: Option<&str>,
) -> Result<Option<SessionMeta>, StoreError> {
    let tx = conn.unchecked_transaction().map_err(sqlite_error)?;
    let session_id = if let Some(session_id) = selected_session_id {
        session_id.to_string()
    } else {
        let mut stmt = tx
            .prepare("SELECT session_id FROM session_meta ORDER BY session_id ASC LIMIT 2")
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
        session_ids.into_iter().next().expect("one session id")
    };
    let mut stored = tx
        .query_row(
            &format!("SELECT {SELECT_COLUMNS} FROM session_meta WHERE session_id = ?1"),
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
        .prepare(
            "SELECT process_index, process_id, process_incarnation, attribution
             FROM session_meta_pending_observer_intents
             WHERE session_id = ?1 ORDER BY process_index",
        )
        .map_err(sqlite_error)?;
    let observer_rows = stmt
        .query_map(params![stored.session_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    drop(stmt);
    for (process_index, process_id, process_incarnation, attribution) in observer_rows {
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
        stored.pending_observer_intents.push(
            lash_core::store_backend_support::StoredObserverIntent {
                process_id,
                process_incarnation,
                attribution,
            },
        );
    }
    stored.fork_inheritance_processes = read_process_list(
        &tx,
        "session_meta_fork_inheritance_processes",
        &stored.session_id,
    )?;
    let meta = SessionMetaCodec::decode(SESSION_META_CODEC, stored)?;
    tx.commit().map_err(sqlite_error)?;
    Ok(Some(meta))
}

fn write_process_list(
    conn: &Connection,
    table: &str,
    session_id: &str,
    process_ids: &[String],
) -> Result<(), StoreError> {
    for (process_index, process_id) in process_ids.iter().enumerate() {
        conn.execute(
            &format!(
                "INSERT INTO {table} (session_id, process_index, process_id) VALUES (?1, ?2, ?3)"
            ),
            params![
                session_id,
                SessionMetaCodec::write_index(SESSION_META_CODEC, process_index, "process")?,
                process_id
            ],
        )
        .map_err(sqlite_error)?;
    }
    Ok(())
}

fn read_process_list(
    conn: &Connection,
    table: &str,
    session_id: &str,
) -> Result<Vec<String>, StoreError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT process_index, process_id FROM {table}
             WHERE session_id = ?1 ORDER BY process_index"
        ))
        .map_err(sqlite_error)?;
    let rows = stmt
        .query_map(params![session_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    let mut process_ids = Vec::with_capacity(rows.len());
    for (process_index, process_id) in rows {
        if SessionMetaCodec::read_index(SESSION_META_CODEC, process_index, "process_index")?
            != process_ids.len()
        {
            return Err(SessionMetaCodec::corrupt(
                SESSION_META_CODEC,
                "process indexes are not contiguous",
            ));
        }
        process_ids.push(process_id);
    }
    Ok(process_ids)
}
