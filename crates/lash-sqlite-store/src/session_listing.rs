//! Session-catalog view listing over the durable-core catalog.

use lash_core_execution::{SessionListFilter, SessionRelationKind, SessionView};
use lash_sansio::SessionId;
use rusqlite::Connection;

use crate::{sqlite_conversion_error, stored_data_corrupt, u64_from_sql};

pub(crate) fn list_session_views(
    conn: &Connection,
    filter: &SessionListFilter,
) -> rusqlite::Result<Vec<SessionView>> {
    let mut stmt = conn.prepare_cached(
        crate::session_sql::session_sql()
            .meta_sqlite
            .select_catalog
            .sql(),
    )?;
    let rows = stmt.query_map([], |row| {
        let stored = crate::session_meta::stored_relation_from_row(row)?;
        let relation = match stored.relation_kind.as_str() {
            "root" => SessionRelationKind::Root,
            "child" => SessionRelationKind::Child,
            "fork" => SessionRelationKind::Fork,
            other => {
                return Err(sqlite_conversion_error(stored_data_corrupt(
                    "SessionView",
                    format!("unknown relation_kind `{other}`"),
                )));
            }
        };
        let parent_session_id = stored.parent_session_id.clone();
        let deleted = row.get::<_, i64>(20)? != 0;
        let durable_relation = if deleted {
            None
        } else {
            Some(
                crate::session_meta::decode_catalog_relation(stored, &row.get::<_, String>(21)?)
                    .map_err(sqlite_conversion_error)?,
            )
        };
        Ok(SessionView {
            session_id: SessionId::from(row.get::<_, String>(0)?),
            created_at_ms: u64_from_sql("SessionView", "created_at_ms", row.get(17)?)?,
            last_commit_at_ms: row
                .get::<_, Option<i64>>(18)?
                .map(|value| u64_from_sql("SessionView", "last_commit_at_ms", value))
                .transpose()?,
            head_revision: u64_from_sql("SessionView", "head_revision", row.get(19)?)?,
            relation,
            durable_relation,
            parent_session_id,
            deleted,
        })
    })?;
    let mut views = Vec::new();
    for row in rows {
        let view = row?;
        if filter.matches(&view) {
            views.push(view);
        }
    }
    Ok(views)
}
