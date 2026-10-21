use super::*;
use crate::session_sql::session_sql;
use lash_vocabulary::identity::ProcessId;
use lash_vocabulary::identity::SessionId;
use lash_vocabulary::identity::TurnId;

use lash_core_store::store_backend_support::CausalColumns;
use lash_core_store::store_backend_support::SessionMetaCodec;
use lash_core_store::store_backend_support::StoredRelation;

const SESSION_META_CODEC: SessionMetaCodec = SessionMetaCodec::new("SQLite INTEGER");

pub(crate) fn stored_relation_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<StoredRelation> {
    Ok(StoredRelation {
        session_id: crate::codec::sql_identity(row.get::<_, String>(0)?)?,
        relation_kind: row.get(1)?,
        parent_session_id: row
            .get::<_, Option<String>>(2)?
            .map(crate::codec::sql_identity)
            .transpose()?,
        cause: CausalColumns {
            kind: row.get(3)?,
            session_id: row
                .get::<_, Option<String>>(4)?
                .map(crate::codec::sql_identity)
                .transpose()?,
            turn_id: row
                .get::<_, Option<String>>(5)?
                .map(crate::codec::sql_identity)
                .transpose()?,
            effect_id: row.get(6)?,
            call_id: row.get(7)?,
            process_id: row
                .get::<_, Option<String>>(8)?
                .map(|value| crate::sql_process_id(8, value))
                .transpose()?,
            process_event_sequence: row.get(9)?,
            node_id: row.get(10)?,
        },
        source_session_id: row
            .get::<_, Option<String>>(11)?
            .map(crate::codec::sql_identity)
            .transpose()?,
        source_node_id: row.get(12)?,
        pending_observer_intents: Vec::new(),
    })
}

pub(crate) fn decode_catalog_relation(
    stored: StoredRelation,
    observer_intent_rows_json: &str,
) -> Result<lash_core_store::session_identity::SessionRelation, StoreError> {
    let observer_intent_rows =
        serde_json::from_str(observer_intent_rows_json).map_err(|error| {
            SessionMetaCodec::corrupt(
                SESSION_META_CODEC,
                format!("invalid observer-intent process rows JSON: {error}"),
            )
        })?;
    Ok(SessionMetaCodec::decode_with_process_rows(
        SESSION_META_CODEC,
        stored,
        observer_intent_rows,
    )?
    .relation)
}

pub(crate) fn write_session_meta(
    conn: &Connection,
    meta: &SessionMeta,
    retention: lash_core_store::store::Retention,
    created_at_ms: u64,
    fleet_format: lash_core_store::store::FleetFormat,
) -> Result<bool, StoreError> {
    let stored = SessionMetaCodec::encode(SESSION_META_CODEC, meta)?;
    let sql = session_sql().meta_sqlite.insert.sql();
    let changed = conn
        .execute(
            sql,
            params![
                stored.session_id.as_str(),
                stored.relation_kind,
                stored.parent_session_id.as_deref(),
                stored.cause.kind,
                stored.cause.session_id.as_deref(),
                stored.cause.turn_id.as_ref().map(TurnId::as_str),
                stored.cause.effect_id,
                stored.cause.call_id,
                stored.cause.process_id.as_deref(),
                stored.cause.process_event_sequence,
                stored.cause.node_id,
                stored.source_session_id.as_deref(),
                stored.source_node_id,
                crate::clamp_epoch_ms(created_at_ms),
                fleet_format.writer_version(lash_core_store::surface_format!(
                    lash_core_store::store::CURRENT_SESSION_STATE_VERSION
                )),
                meta.owning_process_id.as_ref().map(ProcessId::as_str),
                retention.kind(),
                retention.last_turns().map(i64::from),
            ],
        )
        .map_err(sqlite_error)?;
    if changed == 0 {
        return Ok(false);
    }
    settle_observer_intents_conn(conn, &meta.session_id, &meta.pending_observer_intents)?;
    Ok(true)
}

/// Replace the observer selector inside the caller's guarded transaction.
pub(crate) fn settle_observer_intents_conn(
    conn: &Connection,
    session_id: &SessionId,
    remaining: &[lash_core_store::session_identity::SessionObserverIntent],
) -> Result<(), StoreError> {
    crate::conn::cached_execute(
        conn,
        session_sql().observer_intents.delete_by_session.sql(),
        params![session_id.as_str()],
    )
    .map_err(sqlite_error)?;
    for (process_index, intent) in remaining.iter().enumerate() {
        crate::conn::cached_execute(
            conn,
            session_sql().observer_intents.insert.sql(),
            params![
                session_id.as_str(),
                SessionMetaCodec::write_index(
                    SESSION_META_CODEC,
                    process_index,
                    "observer-intent process"
                )?,
                intent.process_id.as_str(),
            ],
        )
        .map_err(sqlite_error)?;
    }
    Ok(())
}

/// Read only the immutable lineage for admission's rebind guard.
pub(crate) fn load_recorded_lineage(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<lash_core_store::session_identity::SessionLineage>, StoreError> {
    let row = conn
        .prepare_cached(session_sql().meta.select_lineage.sql())
        .and_then(|mut statement| {
            statement.query_row(params![session_id.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
        })
        .optional()
        .map_err(sqlite_error)?;
    row.map(
        |(relation_kind, parent_session_id, source_session_id, source_node_id)| {
            SESSION_META_CODEC.decode_lineage(
                &relation_kind,
                parent_session_id.map(SessionId::parse).transpose()?,
                source_session_id.map(SessionId::parse).transpose()?,
                source_node_id,
            )
        },
    )
    .transpose()
}

pub(crate) fn load_session_meta(
    conn: &Connection,
    selected_session_id: Option<&SessionId>,
) -> Result<Option<SessionMeta>, StoreError> {
    load_session_meta_snapshot(conn, selected_session_id, false)
}

pub(crate) fn load_session_meta_guarded(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<SessionMeta>, StoreError> {
    load_session_meta_snapshot(conn, Some(session_id), true)
}

fn load_session_meta_snapshot(
    conn: &Connection,
    selected_session_id: Option<&SessionId>,
    guarded: bool,
) -> Result<Option<SessionMeta>, StoreError> {
    let sql = match selected_session_id {
        Some(_) => &session_sql().meta_sqlite.select_snapshot,
        None => &session_sql().meta_sqlite.select_sole_snapshot,
    };
    let mut statement = conn.prepare_cached(sql.sql()).map_err(sqlite_error)?;
    // Classify cardinality and tombstones before decoding any relation or ID.
    let mut rows = match selected_session_id {
        Some(session) => statement.query(params![session.as_str(), guarded]),
        None => statement.query([]),
    }
    .map_err(sqlite_error)?;
    let Some(row) = rows.next().map_err(sqlite_error)? else {
        return Ok(None);
    };
    if guarded && row.get::<_, bool>(15).map_err(sqlite_error)? {
        return Err(StoreError::SessionDeleted {
            session_id: selected_session_id.cloned().ok_or_else(|| {
                SESSION_META_CODEC.corrupt("guarded metadata requires a session id")
            })?,
        });
    }
    if row
        .get::<_, Option<String>>(0)
        .map_err(sqlite_error)?
        .is_none()
    {
        return Ok(None);
    }
    // Defer decoding until we know a sole-session request is unambiguous.
    let raw = (
        stored_relation_from_row(row),
        row.get::<_, String>(14),
        row.get::<_, Option<String>>(13),
    );
    if selected_session_id.is_none() && rows.next().map_err(sqlite_error)?.is_some() {
        return Ok(None);
    }
    let (stored, intents, owner) = raw;
    let stored = stored.map_err(sqlite_error)?;
    let intents = serde_json::from_str(&intents.map_err(sqlite_error)?).map_err(|error| {
        SESSION_META_CODEC.corrupt(format!(
            "invalid observer-intent process rows JSON: {error}"
        ))
    })?;
    let mut meta = SESSION_META_CODEC.decode_with_process_rows(stored, intents)?;
    meta.owning_process_id = owner
        .map_err(sqlite_error)?
        .map(|value| {
            ProcessId::parse(&value).map_err(|error| SESSION_META_CODEC.corrupt(error.to_string()))
        })
        .transpose()?;
    Ok(Some(meta))
}

/// Retain `checkpoint_ref` as the checkpoint session `session_id`'s latest
/// turn was admitted on, replacing the previous admission's (FIG-3682).
///
/// A session with no metadata row yet has committed nothing, so its admission
/// base names no checkpoint and there is nothing to retain. The base it
/// replaces is peeled (FIG-5949).
pub(crate) fn retain_admission_base_conn(
    conn: &rusqlite::Connection,
    session_id: &SessionId,
    checkpoint_ref: Option<&lash_core_store::store::BlobRef>,
) -> Result<(), StoreError> {
    let replaced = conn
        .prepare_cached(session_sql().meta.select_admission_base.sql())
        .and_then(|mut statement| {
            statement.query_row(rusqlite::params![session_id.as_str()], |row| {
                crate::codec::optional_hash_column(row, 0)
            })
        })
        .optional()
        .map_err(sqlite_error)?
        .flatten()
        .filter(|old| Some(old.as_str()) != checkpoint_ref.map(|blob_ref| blob_ref.as_str()));
    crate::conn::cached_execute(
        conn,
        session_sql().meta.retain_admission_base.sql(),
        rusqlite::params![
            session_id.as_str(),
            checkpoint_ref.map(|blob_ref| crate::codec::SqlHash(blob_ref.as_str()))
        ],
    )
    .map_err(sqlite_error)?;
    crate::durable::peel(conn, replaced.into_iter().collect()).map_err(sqlite_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core_store::store::{SessionHistoryStore as _, WindowSelector};

    /// SESSION-GENERATION-BEFORE-WINDOW: even a missing head cannot hide refusal.
    #[tokio::test]
    async fn a_window_without_a_head_refuses_an_unsupported_generation() {
        let store = crate::test_support::sqlite_memory_store()
            .await
            .expect("store");
        store.conn.call(|conn| {
            conn.execute("INSERT INTO session_meta (session_id, relation_kind, session_state_version, retention_kind) VALUES ('no-head', 'root', 0, 'until_gc')", [])?;
            Ok(())
        }).await.expect("legacy marker without head");
        for marker in [
            Some(0_i64),
            None,
            Some(i64::from(lash_core_store::store::CURRENT_SESSION_STATE_VERSION) + 1),
            Some(-1),
        ] {
            store.conn.call(move |conn| {
                conn.execute("UPDATE session_meta SET session_state_version = ?1 WHERE session_id = 'no-head'", [marker])?;
                Ok(())
            }).await.expect("change generation between invocations");
            for selector in [
                WindowSelector::Current,
                WindowSelector::Terminal(TurnId::from("missing-run")),
            ] {
                let error = store
                    .load_session_window(&SessionId::from("no-head"), selector)
                    .await
                    .expect_err("generation refusal precedes missing head");
                match marker {
                    None | Some(0) => assert!(
                        matches!(
                            error,
                            StoreError::SessionStateVersionUnsupported { found: 0, .. }
                        ),
                        "{error:?}"
                    ),
                    Some(-1) => assert!(
                        matches!(
                            error,
                            StoreError::StoredDataCorrupt {
                                record_kind: "SessionStateVersion",
                                ..
                            }
                        ),
                        "{error:?}"
                    ),
                    Some(_) => assert!(
                        matches!(
                            error,
                            StoreError::SessionStateVersionNewerThanRuntime { .. }
                        ),
                        "{error:?}"
                    ),
                }
            }
        }

        store.conn.call(|conn| {
            conn.execute("UPDATE session_meta SET session_state_version = ?1 WHERE session_id = 'no-head'", [lash_core_store::store::CURRENT_SESSION_STATE_VERSION])?;
            Ok(())
        }).await.expect("supported marker");
        let session = SessionId::from("no-head");
        let (window, window_sql) = lash_core_ids::operational_metrics::sql::collect(
            "window.admission",
            "sqlite",
            store.load_session_window(&session, WindowSelector::Current),
        )
        .await;
        let (head, head_sql) = lash_core_ids::operational_metrics::sql::collect(
            "head.admission",
            "sqlite",
            lash_core_store::store::SessionCommitStore::load_session_head_meta(
                store.as_ref(),
                &session,
            ),
        )
        .await;
        assert!(window.expect("supported window").is_none());
        assert!(head.expect("supported head").is_none());
        for receipt in [window_sql, head_sql] {
            assert!(
                !receipt
                    .shapes
                    .keys()
                    .any(|shape| shape.starts_with("select session_state_version from ")),
                "{receipt:?}"
            );
            eprintln!(
                "WINDOW_SQL {}",
                serde_json::to_string(&receipt).expect("receipt")
            );
        }
    }

    /// SESSION-META-SNAPSHOT: lineage, owner and ordered intents share one read.
    #[tokio::test]
    async fn metadata_is_one_ordered_read_including_the_deletion_guard() {
        use lash_core_ids::operational_metrics::sql;
        let store = crate::test_support::sqlite_memory_store()
            .await
            .expect("store");
        store.conn.call(|conn| {
            conn.execute("INSERT INTO session_meta (session_id, relation_kind, parent_session_id, owning_process_id, retention_kind) VALUES ('child', 'child', 'parent', ?1, 'until_gc')", [ProcessId::fixture("owner").as_str()])?;
            conn.execute("INSERT INTO session_meta_pending_observer_intents (session_id, process_index, process_id) VALUES ('child', 0, ?1), ('child', 1, ?2)", params![ProcessId::fixture("observer-a").as_str(), ProcessId::fixture("observer-b").as_str()])?;
            Ok(())
        }).await.expect("metadata");
        let session = SessionId::from("child");
        let (ordinary, ordinary_sql) = sql::collect(
            "metadata.ordinary",
            "sqlite",
            store.load_session_meta(&session),
        )
        .await;
        let (guarded, guarded_sql) = sql::collect(
            "metadata.guarded",
            "sqlite",
            store.load_session_meta_for_commit(&session),
        )
        .await;
        eprintln!(
            "METADATA_SQL {}",
            serde_json::to_string(&ordinary_sql).expect("receipt")
        );
        eprintln!(
            "METADATA_SQL {}",
            serde_json::to_string(&guarded_sql).expect("receipt")
        );
        let meta = ordinary.expect("ordinary").expect("child");
        assert_eq!(guarded.expect("guarded"), Some(meta.clone()));
        assert_eq!(meta.owning_process_id, Some(ProcessId::fixture("owner")));
        assert_eq!(
            meta.pending_observer_intents
                .iter()
                .map(|intent| intent.process_id.as_str())
                .collect::<Vec<_>>(),
            [
                ProcessId::fixture("observer-a").as_str(),
                ProcessId::fixture("observer-b").as_str()
            ]
        );
        assert!(
            matches!(meta.relation, lash_core_store::session_identity::SessionRelation::Child { parent_session_id, .. } if parent_session_id.as_str() == "parent")
        );
        assert_eq!(ordinary_sql.statements, 1, "ordinary metadata read");
        assert_eq!(guarded_sql.statements, 1, "guarded metadata read");
        for remaining in [1, 0] {
            store.conn.call(move |conn| {
                conn.execute("DELETE FROM session_meta_pending_observer_intents WHERE process_index >= ?1", [remaining])?;
                Ok(())
            }).await.expect("settle intents");
            let (meta, receipt) = sql::collect(
                format!("metadata.intents{remaining}"),
                "sqlite",
                store.load_session_meta(&session),
            )
            .await;
            assert_eq!(
                meta.expect("load")
                    .expect("metadata")
                    .pending_observer_intents
                    .len(),
                remaining as usize
            );
            assert_eq!(receipt.statements, 1);
            eprintln!(
                "METADATA_SQL {}",
                serde_json::to_string(&receipt).expect("receipt")
            );
        }
        store.conn.call(|conn| {
            conn.execute("INSERT INTO session_meta_pending_observer_intents (session_id, process_index, process_id) VALUES ('child', 2, ?1)", [ProcessId::fixture("gap").as_str()])?;
            Ok(())
        }).await.expect("gap");
        assert!(matches!(
            store.load_session_meta(&session).await,
            Err(StoreError::StoredDataCorrupt { .. })
        ));
        store.conn.call(|conn| {
            conn.execute("INSERT INTO deleted_sessions (session_id, created_at_ms, head_revision, relation_kind) VALUES ('child', 0, 0, 'root')", [])?;
            // SQLite can retain a BLOB in a TEXT column after corruption.
            // JSON aggregation must not preempt permanent deletion evidence.
            conn.execute("UPDATE session_meta_pending_observer_intents SET process_id = x'ff' WHERE session_id = 'child'", [])?;
            Ok(())
        }).await.expect("tombstone beside corrupt metadata");
        assert!(matches!(
            store.load_session_meta_for_commit(&session).await,
            Err(StoreError::SessionDeleted { .. })
        ));
        store.conn.call(|conn| {
            conn.execute("INSERT INTO session_meta (session_id, relation_kind, retention_kind) VALUES ('other', 'root', 'until_gc')", [])?;
            assert!(load_session_meta(conn, None).expect("multiple headers precede corrupt observer data").is_none());
            conn.execute("DELETE FROM session_meta WHERE session_id = 'other'", [])?;
            conn.execute("DELETE FROM session_meta_pending_observer_intents", [])?;
            assert!(load_session_meta(conn, None).expect("sole").is_some());
            conn.execute("DELETE FROM session_meta", [])?;
            assert!(load_session_meta(conn, None).expect("empty").is_none());
            Ok(())
        }).await.expect("sole-session cardinality");
    }
}
