use crate::*;

pub(crate) use lash_core::store_backend_support::SessionMetaWrite;
use lash_core::store_backend_support::{CausalColumns, SessionMetaCodec, StoredRelation};

const SESSION_META_CODEC: SessionMetaCodec = SessionMetaCodec::new("PostgreSQL BIGINT");

pub(crate) fn stored_relation_from_row(row: &PgRow) -> StoredRelation {
    StoredRelation {
        session_id: row.get("session_id"),
        relation_kind: row.get("relation_kind"),
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
        pending_observer_intents: Vec::new(),
        fork_inheritance_processes: Vec::new(),
    }
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
             (session_id, session_state_version, relation_kind, parent_session_id,
              caused_by_kind, caused_by_session_id, caused_by_turn_id,
              caused_by_effect_id, caused_by_call_id, caused_by_process_id,
              caused_by_process_event_sequence, caused_by_occurrence_id,
              caused_by_subscription_id, caused_by_subscription_incarnation,
              caused_by_subscription_revision, caused_by_node_id, source_session_id,
              source_node_id, observer_inheritance_kind, created_at_ms, last_commit_at_ms)
             VALUES ($1, $20, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                     $13, $14, $15, $16, $17, $18, $19, NULL)
             ON CONFLICT (session_id) DO NOTHING"
        }
        SessionMetaWrite::Replace => {
            "INSERT INTO lash_session_meta
             (session_id, session_state_version, relation_kind, parent_session_id,
              caused_by_kind, caused_by_session_id, caused_by_turn_id,
              caused_by_effect_id, caused_by_call_id, caused_by_process_id,
              caused_by_process_event_sequence, caused_by_occurrence_id,
              caused_by_subscription_id, caused_by_subscription_incarnation,
              caused_by_subscription_revision, caused_by_node_id, source_session_id,
              source_node_id, observer_inheritance_kind, created_at_ms, last_commit_at_ms)
             VALUES ($1, $20, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                     $13, $14, $15, $16, $17, $18, $19, NULL)
             ON CONFLICT (session_id) DO UPDATE SET
               relation_kind = EXCLUDED.relation_kind,
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
        "lash_session_meta_pending_observer_intents",
        "lash_session_meta_fork_inheritance_processes",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE session_id = $1"))
            .bind(&stored.session_id)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    for (process_index, intent) in stored.pending_observer_intents.iter().enumerate() {
        sqlx::query(
            "INSERT INTO lash_session_meta_pending_observer_intents
             (session_id, process_index, process_id, process_incarnation, attribution)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&stored.session_id)
        .bind(SessionMetaCodec::write_index(
            SESSION_META_CODEC,
            process_index,
            "observer-intent process",
        )?)
        .bind(&intent.process_id)
        .bind(intent.process_incarnation)
        .bind(&intent.attribution)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    }
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
    let mut connection = acquire_runtime_connection(pool).await?;
    let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
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
    let observer_rows = sqlx::query_as::<_, (i64, String, Option<i64>, String)>(
        "SELECT process_index, process_id, process_incarnation, attribution
         FROM lash_session_meta_pending_observer_intents
         WHERE session_id = $1 ORDER BY process_index",
    )
    .bind(&stored.session_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(store_sqlx_error)?;
    for (process_index, process_id, process_incarnation, attribution) in observer_rows {
        let process_index = SessionMetaCodec::read_index(
            SESSION_META_CODEC,
            process_index,
            "observer-intent process_index",
        )?;
        if process_index != stored.pending_observer_intents.len() {
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
