use crate::*;

pub(crate) async fn list_sessions(
    pool: &PgPool,
    filter: &SessionListFilter,
) -> Result<Vec<SessionSummary>, StoreError> {
    let rows = sqlx::query(
        "SELECT meta.session_id, COALESCE(meta.created_at_ms, 0) AS created_at_ms,
                meta.last_commit_at_ms, COALESCE(session.head_revision, 0),
                meta.relation_kind, meta.parent_session_id, FALSE AS deleted
         FROM lash_session_meta AS meta
         LEFT JOIN lash_sessions AS session ON session.session_id = meta.session_id
         UNION ALL
         SELECT session_id, COALESCE(created_at_ms, 0) AS created_at_ms,
                last_commit_at_ms, COALESCE(head_revision, 0),
                COALESCE(relation_kind, 'root'), parent_session_id, TRUE AS deleted
         FROM lash_deleted_sessions
         ORDER BY created_at_ms ASC, session_id ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(store_sqlx_error)?;
    let mut summaries = Vec::with_capacity(rows.len());
    for row in rows {
        let relation_label: String = row.get(4);
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
        let deleted = row.get(6);
        let mut summary = SessionSummary {
            session_id: row.get(0),
            created_at_ms: u64_from_sql("SessionSummary", "created_at_ms", row.get(1))?,
            last_commit_at_ms: row
                .get::<Option<i64>, _>(2)
                .map(|value| u64_from_sql("SessionSummary", "last_commit_at_ms", value))
                .transpose()?,
            head_revision: u64_from_sql("SessionSummary", "head_revision", row.get(3))?,
            relation,
            full_relation: None,
            parent_session_id: row.get(5),
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
        if !deleted {
            summary.full_relation =
                crate::session_meta::load_session_meta(pool, Some(&summary.session_id))
                    .await?
                    .map(|meta| meta.relation);
        }
        summaries.push(summary);
    }
    Ok(summaries)
}
