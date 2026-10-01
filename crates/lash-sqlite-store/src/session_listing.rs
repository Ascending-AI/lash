//! Session-catalog view listing over the durable-core catalog.

use lash_core_execution::{SessionEntry, SessionListFilter, SessionRelationKind, SessionView};
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
        let deleted = row.get::<_, i64>(20)? != 0;
        let closing = row.get::<_, i64>(21)? != 0;
        let entry = if deleted {
            let kind = match stored.relation_kind.as_str() {
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
            SessionEntry::Deleted {
                kind,
                parent: stored.parent_session_id.clone(),
            }
        } else {
            let relation =
                crate::session_meta::decode_catalog_relation(stored, &row.get::<_, String>(22)?)
                    .map_err(sqlite_conversion_error)?;
            if closing {
                SessionEntry::Closing { relation }
            } else {
                SessionEntry::Live { relation }
            }
        };
        Ok(SessionView {
            session_id: SessionId::from(row.get::<_, String>(0)?),
            created_at_ms: u64_from_sql("SessionView", "created_at_ms", row.get(17)?)?,
            last_commit_at_ms: row
                .get::<_, Option<i64>>(18)?
                .map(|value| u64_from_sql("SessionView", "last_commit_at_ms", value))
                .transpose()?,
            head_revision: u64_from_sql("SessionView", "head_revision", row.get(19)?)?,
            entry,
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
