use crate::*;

pub(crate) async fn list_sessions(
    pool: &PgPool,
    filter: &SessionListFilter,
) -> Result<Vec<SessionSummary>, StoreError> {
    let rows = sqlx::query(
        "WITH catalog AS (
             SELECT meta.session_id, meta.relation_kind, meta.observer_intent_depth,
                    meta.parent_session_id, meta.caused_by_kind,
                    meta.caused_by_session_id, meta.caused_by_turn_id,
                    meta.caused_by_effect_id, meta.caused_by_call_id,
                    meta.caused_by_process_id, meta.caused_by_process_event_sequence,
                    meta.caused_by_occurrence_id, meta.caused_by_subscription_id,
                    meta.caused_by_subscription_incarnation,
                    meta.caused_by_subscription_revision, meta.caused_by_node_id,
                    meta.source_session_id, meta.source_node_id,
                    meta.observer_inheritance_kind,
                    COALESCE(meta.created_at_ms, 0) AS created_at_ms,
                    meta.last_commit_at_ms,
                    COALESCE(session.head_revision, 0) AS head_revision,
                    FALSE AS deleted
             FROM lash_session_meta AS meta
             LEFT JOIN lash_sessions AS session ON session.session_id = meta.session_id
             UNION ALL
             SELECT session_id, COALESCE(relation_kind, 'root'), 0::BIGINT,
                    parent_session_id, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                    NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                    COALESCE(created_at_ms, 0), last_commit_at_ms,
                    COALESCE(head_revision, 0), TRUE
             FROM lash_deleted_sessions
         )
         SELECT catalog.*,
                CASE WHEN deleted THEN '[]' ELSE COALESCE((
                    SELECT jsonb_agg(
                               jsonb_build_array(layer_index, process_index, process_id)
                               ORDER BY layer_index, process_index
                           )::TEXT
                    FROM lash_session_meta_observer_intent_processes
                    WHERE session_id = catalog.session_id
                ), '[]') END AS observer_intent_rows_json,
                CASE WHEN deleted THEN '[]' ELSE COALESCE((
                    SELECT jsonb_agg(
                               jsonb_build_array(process_index, process_id)
                               ORDER BY process_index
                           )::TEXT
                    FROM lash_session_meta_fork_pending_observer_processes
                    WHERE session_id = catalog.session_id
                ), '[]') END AS fork_pending_rows_json,
                CASE WHEN deleted THEN '[]' ELSE COALESCE((
                    SELECT jsonb_agg(
                               jsonb_build_array(process_index, process_id)
                               ORDER BY process_index
                           )::TEXT
                    FROM lash_session_meta_fork_inheritance_processes
                    WHERE session_id = catalog.session_id
                ), '[]') END AS fork_inheritance_rows_json
         FROM catalog
         ORDER BY created_at_ms ASC, session_id ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(store_sqlx_error)?;
    let mut summaries = Vec::with_capacity(rows.len());
    for row in rows {
        let stored = crate::session_meta::stored_relation_from_row(&row);
        let relation_label = stored.relation_kind.clone();
        let relation = match relation_label.as_str() {
            "root" => SessionRelationKind::Root,
            "child" => SessionRelationKind::Child,
            "fork" => SessionRelationKind::Fork,
            other => {
                return Err(StoreError::StoredDataCorrupt {
                    record_kind: "SessionSummary",
                    message: format!("unknown relation_kind `{other}`"),
                });
            }
        };
        let parent_session_id = stored.parent_session_id.clone();
        let deleted: bool = row.get("deleted");
        let durable_relation = if deleted {
            None
        } else {
            Some(crate::session_meta::decode_catalog_relation(
                stored,
                row.get("observer_intent_rows_json"),
                row.get("fork_pending_rows_json"),
                row.get("fork_inheritance_rows_json"),
            )?)
        };
        let summary = SessionSummary {
            session_id: row.get("session_id"),
            created_at_ms: u64_from_sql(
                "SessionSummary",
                "created_at_ms",
                row.get("created_at_ms"),
            )?,
            last_commit_at_ms: row
                .get::<Option<i64>, _>("last_commit_at_ms")
                .map(|value| u64_from_sql("SessionSummary", "last_commit_at_ms", value))
                .transpose()?,
            head_revision: u64_from_sql(
                "SessionSummary",
                "head_revision",
                row.get("head_revision"),
            )?,
            relation,
            durable_relation,
            parent_session_id,
            deleted,
        };
        if !filter
            .relation
            .is_none_or(|relation| relation == summary.relation)
            || !filter
                .deleted
                .is_none_or(|deleted| deleted == summary.deleted)
        {
            continue;
        }
        summaries.push(summary);
    }
    Ok(summaries)
}
