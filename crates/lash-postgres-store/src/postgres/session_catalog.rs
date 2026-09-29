use crate::*;

pub(crate) async fn list_sessions(
    pool: &PgPool,
    filter: &SessionListFilter,
) -> Result<Vec<SessionView>, StoreError> {
    let rows = sqlx::query(
        crate::session_sql::session_sql()
            .meta_postgres
            .select_catalog
            .sql(),
    )
    .fetch_all(pool)
    .await
    .map_err(store_sqlx_error)?;
    let mut views = Vec::with_capacity(rows.len());
    for row in rows {
        let stored = crate::session_meta::stored_relation_from_row(&row)?;
        let relation_label = stored.relation_kind.clone();
        let relation = match relation_label.as_str() {
            "root" => SessionRelationKind::Root,
            "child" => SessionRelationKind::Child,
            "fork" => SessionRelationKind::Fork,
            other => {
                return Err(StoreError::StoredDataCorrupt {
                    record_kind: "SessionView",
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
            )?)
        };
        let view = SessionView {
            session_id: SessionId::from(row.get::<String, _>("session_id")),
            created_at_ms: u64_from_sql("SessionView", "created_at_ms", row.get("created_at_ms"))?,
            last_commit_at_ms: row
                .get::<Option<i64>, _>("last_commit_at_ms")
                .map(|value| u64_from_sql("SessionView", "last_commit_at_ms", value))
                .transpose()?,
            head_revision: u64_from_sql("SessionView", "head_revision", row.get("head_revision"))?,
            relation,
            durable_relation,
            parent_session_id,
            deleted,
        };
        if !filter.matches(&view) {
            continue;
        }
        views.push(view);
    }
    Ok(views)
}
