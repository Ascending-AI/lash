//! Session-catalog view listing over the durable-core catalog.

use lash_core_store::session_catalog::SessionEntry;
use lash_core_store::session_catalog::SessionListFilter;
use lash_core_store::session_catalog::SessionRelationKind;
use lash_core_store::session_catalog::SessionView;
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
    let relation = filter.relation.map(|kind| match kind {
        SessionRelationKind::Root => "root",
        SessionRelationKind::Child => "child",
        SessionRelationKind::Fork => "fork",
    });
    let cause =
        lash_core_store::store_backend_support::CausalColumns::encode(filter.caused_by.as_ref());
    let rows = stmt.query_map(
        rusqlite::params![
            relation,
            filter.deleted,
            cause.kind,
            cause.session_id.as_deref(),
            cause
                .turn_id
                .as_ref()
                .map(lash_vocabulary::identity::TurnId::as_str),
            cause.effect_id,
            cause.call_id,
            cause.process_id.as_deref(),
            cause.process_event_sequence,
            cause.node_id,
        ],
        |row| {
            let stored = crate::session_meta::stored_relation_from_row(row)?;
            let deleted = row.get::<_, i64>(16)? != 0;
            let closing = row.get::<_, i64>(17)? != 0;
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
                let relation = crate::session_meta::decode_catalog_relation(
                    stored,
                    &row.get::<_, String>(18)?,
                )
                .map_err(sqlite_conversion_error)?;
                if closing {
                    SessionEntry::Closing { relation }
                } else {
                    SessionEntry::Live { relation }
                }
            };
            Ok(SessionView {
                session_id: crate::codec::sql_identity(row.get::<_, String>(0)?)?,
                created_at_ms: u64_from_sql("SessionView", "created_at_ms", row.get(13)?)?,
                last_commit_at_ms: row
                    .get::<_, Option<i64>>(14)?
                    .map(|value| u64_from_sql("SessionView", "last_commit_at_ms", value))
                    .transpose()?,
                head_revision: u64_from_sql("SessionView", "head_revision", row.get(15)?)?,
                entry,
            })
        },
    )?;
    rows.collect()
}
