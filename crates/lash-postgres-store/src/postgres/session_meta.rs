use crate::*;

pub(crate) use lash_core::store_backend_support::SessionMetaWrite;
use lash_core::store_backend_support::{CausalColumns, SessionMetaCodec, StoredRelation};

const SESSION_META_CODEC: SessionMetaCodec = SessionMetaCodec::new("PostgreSQL BIGINT");

fn stored_relation_from_row(row: &PgRow) -> StoredRelation {
    StoredRelation {
        session_id: row.get("session_id"),
        relation_kind: row.get("relation_kind"),
        observer_intent_depth: row.get("observer_intent_depth"),
        parent_session_id: row.get("parent_session_id"),
        cause: CausalColumns {
            kind: row.get("caused_by_kind"),
            session_id: row.get("caused_by_session_id"),
            turn_id: row.get("caused_by_turn_id"),
            effect_id: row.get("caused_by_effect_id"),
            call_id: row.get("caused_by_call_id"),
            process_id: row.get("caused_by_process_id"),
            process_event_sequence: row.get("caused_by_process_event_sequence"),
            occurrence_id: row.get("caused_by_occurrence_id"),
            subscription_id: row.get("caused_by_subscription_id"),
            subscription_incarnation: row.get("caused_by_subscription_incarnation"),
            subscription_revision: row.get("caused_by_subscription_revision"),
            node_id: row.get("caused_by_node_id"),
        },
        source_session_id: row.get("source_session_id"),
        source_node_id: row.get("source_node_id"),
        observer_inheritance_kind: row.get("observer_inheritance_kind"),
        observer_intent_processes: Vec::new(),
        fork_pending_processes: Vec::new(),
        fork_inheritance_processes: Vec::new(),
    }
}

const SELECT_COLUMNS: &str = "session_id, relation_kind, observer_intent_depth,
    parent_session_id, caused_by_kind, caused_by_session_id, caused_by_turn_id,
    caused_by_effect_id, caused_by_call_id, caused_by_process_id,
    caused_by_process_event_sequence, caused_by_occurrence_id,
    caused_by_subscription_id, caused_by_subscription_incarnation,
    caused_by_subscription_revision, caused_by_node_id, source_session_id,
    source_node_id, observer_inheritance_kind";

pub(crate) async fn write_session_meta_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    meta: &SessionMeta,
    mode: SessionMetaWrite,
    created_at_ms: u64,
) -> Result<bool, StoreError> {
    let stored = SessionMetaCodec::encode(SESSION_META_CODEC, meta)?;
    let sql = match mode {
        SessionMetaWrite::Insert => {
            "INSERT INTO lash_session_meta
             (session_id, session_state_version, relation_kind, observer_intent_depth, parent_session_id,
              caused_by_kind, caused_by_session_id, caused_by_turn_id,
              caused_by_effect_id, caused_by_call_id, caused_by_process_id,
              caused_by_process_event_sequence, caused_by_occurrence_id,
              caused_by_subscription_id, caused_by_subscription_incarnation,
              caused_by_subscription_revision, caused_by_node_id, source_session_id,
              source_node_id, observer_inheritance_kind, created_at_ms, last_commit_at_ms)
             VALUES ($1, $21, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                     $14, $15, $16, $17, $18, $19, $20, NULL)
             ON CONFLICT (session_id) DO NOTHING"
        }
        SessionMetaWrite::Replace => {
            "INSERT INTO lash_session_meta
             (session_id, session_state_version, relation_kind, observer_intent_depth, parent_session_id,
              caused_by_kind, caused_by_session_id, caused_by_turn_id,
              caused_by_effect_id, caused_by_call_id, caused_by_process_id,
              caused_by_process_event_sequence, caused_by_occurrence_id,
              caused_by_subscription_id, caused_by_subscription_incarnation,
              caused_by_subscription_revision, caused_by_node_id, source_session_id,
              source_node_id, observer_inheritance_kind, created_at_ms, last_commit_at_ms)
             VALUES ($1, $21, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                     $14, $15, $16, $17, $18, $19, $20, NULL)
             ON CONFLICT (session_id) DO UPDATE SET
               relation_kind = EXCLUDED.relation_kind,
               observer_intent_depth = EXCLUDED.observer_intent_depth,
               parent_session_id = EXCLUDED.parent_session_id,
               caused_by_kind = EXCLUDED.caused_by_kind,
               caused_by_session_id = EXCLUDED.caused_by_session_id,
               caused_by_turn_id = EXCLUDED.caused_by_turn_id,
               caused_by_effect_id = EXCLUDED.caused_by_effect_id,
               caused_by_call_id = EXCLUDED.caused_by_call_id,
               caused_by_process_id = EXCLUDED.caused_by_process_id,
               caused_by_process_event_sequence = EXCLUDED.caused_by_process_event_sequence,
               caused_by_occurrence_id = EXCLUDED.caused_by_occurrence_id,
               caused_by_subscription_id = EXCLUDED.caused_by_subscription_id,
               caused_by_subscription_incarnation = EXCLUDED.caused_by_subscription_incarnation,
               caused_by_subscription_revision = EXCLUDED.caused_by_subscription_revision,
               caused_by_node_id = EXCLUDED.caused_by_node_id,
               source_session_id = EXCLUDED.source_session_id,
               source_node_id = EXCLUDED.source_node_id,
               observer_inheritance_kind = EXCLUDED.observer_inheritance_kind"
        }
    };
    let result = sqlx::query(sql)
        .bind(&stored.session_id)
        .bind(&stored.relation_kind)
        .bind(stored.observer_intent_depth)
        .bind(&stored.parent_session_id)
        .bind(&stored.cause.kind)
        .bind(&stored.cause.session_id)
        .bind(&stored.cause.turn_id)
        .bind(&stored.cause.effect_id)
        .bind(&stored.cause.call_id)
        .bind(&stored.cause.process_id)
        .bind(stored.cause.process_event_sequence)
        .bind(&stored.cause.occurrence_id)
        .bind(&stored.cause.subscription_id)
        .bind(&stored.cause.subscription_incarnation)
        .bind(stored.cause.subscription_revision)
        .bind(&stored.cause.node_id)
        .bind(&stored.source_session_id)
        .bind(&stored.source_node_id)
        .bind(&stored.observer_inheritance_kind)
        .bind(i64::try_from(created_at_ms).unwrap_or(i64::MAX))
        .bind(lash_core::store::CURRENT_SESSION_STATE_VERSION as i32)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    if result.rows_affected() == 0 {
        return Ok(false);
    }

    for table in [
        "lash_session_meta_observer_intent_processes",
        "lash_session_meta_fork_pending_observer_processes",
        "lash_session_meta_fork_inheritance_processes",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE session_id = $1"))
            .bind(&stored.session_id)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    for (layer_index, process_ids) in stored.observer_intent_processes.iter().enumerate() {
        for (process_index, process_id) in process_ids.iter().enumerate() {
            sqlx::query(
                "INSERT INTO lash_session_meta_observer_intent_processes
                 (session_id, layer_index, process_index, process_id)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(&stored.session_id)
            .bind(SessionMetaCodec::write_index(
                SESSION_META_CODEC,
                layer_index,
                "observer-intent layer",
            )?)
            .bind(SessionMetaCodec::write_index(
                SESSION_META_CODEC,
                process_index,
                "observer-intent process",
            )?)
            .bind(process_id)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        }
    }
    write_process_list(
        tx,
        "lash_session_meta_fork_pending_observer_processes",
        &stored.session_id,
        &stored.fork_pending_processes,
    )
    .await?;
    write_process_list(
        tx,
        "lash_session_meta_fork_inheritance_processes",
        &stored.session_id,
        &stored.fork_inheritance_processes,
    )
    .await?;
    Ok(true)
}

pub(crate) async fn load_session_meta(
    pool: &PgPool,
    selected_session_id: Option<&str>,
) -> Result<Option<SessionMeta>, StoreError> {
    let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
    let row = if let Some(session_id) = selected_session_id {
        sqlx::query(&format!(
            "SELECT {SELECT_COLUMNS} FROM lash_session_meta WHERE session_id = $1 FOR SHARE"
        ))
        .bind(session_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
    } else {
        let mut rows = sqlx::query(&format!(
            "SELECT {SELECT_COLUMNS} FROM lash_session_meta
             ORDER BY session_id ASC LIMIT 2 FOR SHARE"
        ))
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        if rows.len() != 1 {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(None);
        }
        rows.pop()
    };
    let Some(row) = row else {
        tx.commit().await.map_err(store_sqlx_error)?;
        return Ok(None);
    };
    let mut stored = stored_relation_from_row(&row);
    let depth = SessionMetaCodec::read_index(
        SESSION_META_CODEC,
        stored.observer_intent_depth,
        "observer_intent_depth",
    )?;
    stored.observer_intent_processes = vec![Vec::new(); depth];
    let observer_rows = sqlx::query_as::<_, (i64, i64, String)>(
        "SELECT layer_index, process_index, process_id
         FROM lash_session_meta_observer_intent_processes
         WHERE session_id = $1 ORDER BY layer_index, process_index",
    )
    .bind(&stored.session_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(store_sqlx_error)?;
    for (layer_index, process_index, process_id) in observer_rows {
        let layer_index = SessionMetaCodec::read_index(
            SESSION_META_CODEC,
            layer_index,
            "observer-intent layer_index",
        )?;
        let layer = stored
            .observer_intent_processes
            .get_mut(layer_index)
            .ok_or_else(|| {
                SessionMetaCodec::corrupt(
                    SESSION_META_CODEC,
                    "observer-intent process names a missing layer",
                )
            })?;
        let process_index = SessionMetaCodec::read_index(
            SESSION_META_CODEC,
            process_index,
            "observer-intent process_index",
        )?;
        if process_index != layer.len() {
            return Err(SessionMetaCodec::corrupt(
                SESSION_META_CODEC,
                "observer-intent process indexes are not contiguous",
            ));
        }
        layer.push(process_id);
    }
    stored.fork_pending_processes = read_process_list(
        &mut tx,
        "lash_session_meta_fork_pending_observer_processes",
        &stored.session_id,
    )
    .await?;
    stored.fork_inheritance_processes = read_process_list(
        &mut tx,
        "lash_session_meta_fork_inheritance_processes",
        &stored.session_id,
    )
    .await?;
    let meta = SessionMetaCodec::decode(SESSION_META_CODEC, stored)?;
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(Some(meta))
}

async fn write_process_list(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    session_id: &str,
    process_ids: &[String],
) -> Result<(), StoreError> {
    for (process_index, process_id) in process_ids.iter().enumerate() {
        sqlx::query(&format!(
            "INSERT INTO {table} (session_id, process_index, process_id) VALUES ($1, $2, $3)"
        ))
        .bind(session_id)
        .bind(SessionMetaCodec::write_index(
            SESSION_META_CODEC,
            process_index,
            "process",
        )?)
        .bind(process_id)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    }
    Ok(())
}

async fn read_process_list(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    session_id: &str,
) -> Result<Vec<String>, StoreError> {
    let rows = sqlx::query_as::<_, (i64, String)>(&format!(
        "SELECT process_index, process_id FROM {table}
         WHERE session_id = $1 ORDER BY process_index"
    ))
    .bind(session_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
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
