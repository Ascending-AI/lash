use crate::*;
use lash_core_execution::SessionEntry;

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
    .fetch_all(crate::observed_sql::executor(pool))
    .await
    .map_err(store_sqlx_error)?;
    let mut views = Vec::with_capacity(rows.len());
    for row in rows {
        let stored = crate::session_meta::stored_relation_from_row(&row)?;
        let deleted: bool = row.get("deleted");
        let closing: bool = row.get("closing");
        let entry = if deleted {
            let kind = match stored.relation_kind.as_str() {
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
            SessionEntry::Deleted {
                kind,
                parent: stored.parent_session_id.clone(),
            }
        } else {
            let relation = crate::session_meta::decode_catalog_relation(
                stored,
                row.get("observer_intent_rows_json"),
            )?;
            if closing {
                SessionEntry::Closing { relation }
            } else {
                SessionEntry::Live { relation }
            }
        };
        let view = SessionView {
            session_id: SessionId::parse(row.get::<String, _>("session_id"))?,
            created_at_ms: u64_from_sql("SessionView", "created_at_ms", row.get("created_at_ms"))?,
            last_commit_at_ms: row
                .get::<Option<i64>, _>("last_commit_at_ms")
                .map(|value| u64_from_sql("SessionView", "last_commit_at_ms", value))
                .transpose()?,
            head_revision: u64_from_sql("SessionView", "head_revision", row.get("head_revision"))?,
            entry,
        };
        if !filter.matches(&view) {
            continue;
        }
        views.push(view);
    }
    Ok(views)
}
