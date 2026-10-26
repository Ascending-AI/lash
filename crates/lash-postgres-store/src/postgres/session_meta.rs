use crate::session_sql::session_sql;
use crate::*;
use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::ProcessId;
use lash_vocabulary::identity::TurnId;

use lash_core_store::store_backend_support::CausalColumns;
use lash_core_store::store_backend_support::SessionMetaCodec;
use lash_core_store::store_backend_support::StoredRelation;

const SESSION_META_CODEC: SessionMetaCodec = SessionMetaCodec::new("PostgreSQL BIGINT");

pub(crate) fn stored_relation_from_row(row: &PgRow) -> Result<StoredRelation, StoreError> {
    Ok(StoredRelation {
        session_id: SessionId::parse(row.get::<String, _>("session_id"))?,
        relation_kind: row.get("relation_kind"),
        parent_session_id: row
            .get::<Option<String>, _>("parent_session_id")
            .map(SessionId::parse)
            .transpose()?,
        cause: CausalColumns {
            kind: row.get("caused_by_kind"),
            session_id: row
                .get::<Option<String>, _>("caused_by_session_id")
                .map(SessionId::parse)
                .transpose()?,
            turn_id: row
                .get::<Option<String>, _>("caused_by_turn_id")
                .map(lash_vocabulary::identity::TurnId::parse)
                .transpose()?,
            effect_id: row.get("caused_by_effect_id"),
            call_id: row.get("caused_by_call_id"),
            process_id: row
                .get::<Option<String>, _>("caused_by_process_id")
                .map(|value| {
                    ProcessId::parse(&value).map_err(|error| StoreError::StoredDataCorrupt {
                        record_kind: "SessionMeta caused_by_process_id",
                        message: error.to_string(),
                    })
                })
                .transpose()?,
            process_event_sequence: row.get("caused_by_process_event_sequence"),
            node_id: row.get("caused_by_node_id"),
        },
        source_session_id: row
            .get::<Option<String>, _>("source_session_id")
            .map(SessionId::parse)
            .transpose()?,
        source_node_id: row.get("source_node_id"),
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

/// Read the durable lineage recorded for `session_id`, if the row exists.
///
/// Admission calls this inside its own transaction and needs only the lineage
/// columns, so it avoids the observer-intent joins a full
/// metadata load performs.
pub(crate) async fn load_recorded_lineage_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
) -> Result<Option<lash_core_store::session_identity::SessionLineage>, StoreError> {
    let row = crate::observed_sql::query(session_sql().meta.select_lineage.sql())
        .bind(session_id.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    SESSION_META_CODEC
        .decode_lineage(
            &row.get::<String, _>("relation_kind"),
            row.get::<Option<String>, _>("parent_session_id")
                .map(SessionId::parse)
                .transpose()?,
            row.get::<Option<String>, _>("source_session_id")
                .map(SessionId::parse)
                .transpose()?,
            row.get::<Option<String>, _>("source_node_id"),
        )
        .map(Some)
}

pub(crate) async fn write_session_meta_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    meta: &SessionMeta,
    retention: lash_core_store::store::Retention,
    created_at_ms: u64,
    fleet_format: lash_core_store::store::FleetFormat,
) -> Result<bool, StoreError> {
    let stored = SessionMetaCodec::encode(SESSION_META_CODEC, meta)?;
    let sql = session_sql().meta_postgres.insert.sql();
    let result = crate::observed_sql::query(sql)
        .bind(stored.session_id.as_str())
        .bind(&stored.relation_kind)
        .bind(stored.parent_session_id.as_deref())
        .bind(&stored.cause.kind)
        .bind(stored.cause.session_id.as_deref())
        .bind(stored.cause.turn_id.as_ref().map(TurnId::as_str))
        .bind(&stored.cause.effect_id)
        .bind(&stored.cause.call_id)
        .bind(stored.cause.process_id.as_deref())
        .bind(stored.cause.process_event_sequence)
        .bind(&stored.cause.node_id)
        .bind(stored.source_session_id.as_deref())
        .bind(&stored.source_node_id)
        .bind(i64::try_from(created_at_ms).unwrap_or(i64::MAX))
        .bind(
            fleet_format.writer_version(lash_core_store::surface_format!(
                lash_core_store::store::CURRENT_SESSION_STATE_VERSION
            )) as i32,
        )
        .bind(meta.owning_process_id.as_ref().map(ProcessId::as_str))
        .bind(retention.kind())
        .bind(retention.last_turns().map(i64::from))
        .execute(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?;
    if result.rows_affected() == 0 {
        return Ok(false);
    }

    settle_observer_intents_tx(tx, &meta.session_id, &meta.pending_observer_intents).await?;
    Ok(true)
}

pub(crate) async fn settle_observer_intents_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    remaining: &[lash_core_store::session_identity::SessionObserverIntent],
) -> Result<(), StoreError> {
    crate::observed_sql::query(session_sql().observer_intents.delete_by_session.sql())
        .bind(session_id.as_str())
        .execute(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?;
    for (process_index, intent) in remaining.iter().enumerate() {
        crate::observed_sql::query(session_sql().observer_intents.insert.sql())
            .bind(session_id.as_str())
            .bind(SessionMetaCodec::write_index(
                SESSION_META_CODEC,
                process_index,
                "observer-intent process",
            )?)
            .bind(intent.process_id.as_str())
            .execute(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(store_sqlx_error)?;
    }
    Ok(())
}

pub(crate) async fn load_session_meta(
    pool: &PgPool,
    selected_session_id: Option<&SessionId>,
    observer: &crate::StoreObserver,
) -> Result<Option<SessionMeta>, StoreError> {
    load_session_meta_snapshot(pool, selected_session_id, observer, false).await
}

/// The commit preflight's tombstone and metadata belong to the same statement.
/// It grants no write authority; the commit still rechecks under its lock.
pub(crate) async fn load_session_meta_guarded(
    pool: &PgPool,
    session_id: &SessionId,
    observer: &crate::StoreObserver,
) -> Result<Option<SessionMeta>, StoreError> {
    load_session_meta_snapshot(pool, Some(session_id), observer, true).await
}

async fn load_session_meta_snapshot(
    pool: &PgPool,
    selected_session_id: Option<&SessionId>,
    observer: &crate::StoreObserver,
    guarded: bool,
) -> Result<Option<SessionMeta>, StoreError> {
    let mut connection = acquire_runtime_connection(pool, observer).await?;
    let query = match selected_session_id {
        Some(session) => {
            crate::observed_sql::query(session_sql().meta_postgres.select_snapshot.sql())
                .bind(session.as_str())
                .bind(guarded)
        }
        None => crate::observed_sql::query(session_sql().meta_postgres.select_sole_snapshot.sql()),
    };
    let mut rows = query
        .fetch_all(crate::observed_sql::executor(connection.as_mut()))
        .await
        .map_err(store_sqlx_error)?;
    if rows.len() != 1 {
        return Ok(None);
    }
    let Some(row) = rows.pop() else {
        return Ok(None);
    };
    if guarded && row.get::<bool, _>("deleted") {
        return Err(StoreError::SessionDeleted {
            session_id: selected_session_id.cloned().ok_or_else(|| {
                SESSION_META_CODEC.corrupt("guarded metadata requires a session id")
            })?,
        });
    }
    if row.get::<Option<String>, _>("session_id").is_none() {
        return Ok(None);
    }
    let stored = stored_relation_from_row(&row)?;
    let intents: String = row.get("observer_intent_rows_json");
    let intents = serde_json::from_str(&intents).map_err(|error| {
        SESSION_META_CODEC.corrupt(format!(
            "invalid observer-intent process rows JSON: {error}"
        ))
    })?;
    let mut meta = SESSION_META_CODEC.decode_with_process_rows(stored, intents)?;
    meta.owning_process_id = row
        .get::<Option<String>, _>("owning_process_id")
        .map(|value| {
            ProcessId::parse(&value)
                .map_err(|error| SESSION_META_CODEC.corrupt(format!("owning process: {error}")))
        })
        .transpose()?;
    Ok(Some(meta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core_ids::operational_metrics::sql;
    use lash_core_store::store::{SessionHistoryStore as _, WindowSelector};

    /// SESSION-META-SNAPSHOT: facts and deletion use one backend call, with
    /// original observer indexes still owning corruption refusal.
    #[tokio::test]
    async fn metadata_is_one_ordered_read_including_the_deletion_guard() {
        let Some(url) = crate::postgres_test_support::database_url() else {
            return;
        };
        let database = crate::testing::IsolatedDatabase::create(&url).await;
        let storage = crate::testing::connect(database.url())
            .await
            .expect("storage");
        crate::observed_sql::query("INSERT INTO lash_session_meta (session_id, relation_kind, parent_session_id, owning_process_id, retention_kind) VALUES ('child', 'child', 'parent', $1, 'until_gc')")
            .bind(ProcessId::fixture("owner").to_string()).execute(storage.pool()).await.expect("metadata");
        crate::observed_sql::query("INSERT INTO lash_session_meta_pending_observer_intents (session_id, process_index, process_id) VALUES ('child', 0, $1), ('child', 1, $2)")
            .bind(ProcessId::fixture("observer-a").to_string()).bind(ProcessId::fixture("observer-b").to_string())
            .execute(storage.pool()).await.expect("intents");
        let store = storage.store();
        let session = SessionId::from("child");
        let (ordinary, ordinary_sql) = sql::collect(
            "metadata.ordinary",
            "postgres",
            store.load_session_meta(&session),
        )
        .await;
        let (guarded, guarded_sql) = sql::collect(
            "metadata.guarded",
            "postgres",
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
                .map(|intent| intent.process_id.clone())
                .collect::<Vec<_>>(),
            [
                ProcessId::fixture("observer-a"),
                ProcessId::fixture("observer-b")
            ]
        );
        assert!(
            matches!(meta.relation, lash_core_store::session_identity::SessionRelation::Child { parent_session_id, .. } if parent_session_id.as_str() == "parent")
        );
        assert_eq!(ordinary_sql.statements, 1);
        assert_eq!(guarded_sql.statements, 1);
        for remaining in [1_i64, 0] {
            crate::observed_sql::query(
                "DELETE FROM lash_session_meta_pending_observer_intents WHERE process_index >= $1",
            )
            .bind(remaining)
            .execute(storage.pool())
            .await
            .expect("settle intents");
            let (meta, receipt) = sql::collect(
                format!("metadata.intents{remaining}"),
                "postgres",
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
        crate::observed_sql::query("INSERT INTO lash_session_meta_pending_observer_intents (session_id, process_index, process_id) VALUES ('child', 2, $1)")
            .bind(ProcessId::fixture("gap").to_string()).execute(storage.pool()).await.expect("gap");
        assert!(matches!(
            store.load_session_meta(&session).await,
            Err(StoreError::StoredDataCorrupt { .. })
        ));
        crate::observed_sql::query("INSERT INTO lash_deleted_sessions (session_id, created_at_ms, head_revision, relation_kind) VALUES ('child', 0, 0, 'root')")
            .execute(storage.pool()).await.expect("tombstone beside corrupt metadata");
        assert!(matches!(
            store.load_session_meta_for_commit(&session).await,
            Err(StoreError::SessionDeleted { .. })
        ));
    }

    /// SESSION-GENERATION-BEFORE-WINDOW: missing current/terminal heads never
    /// hide a present NULL, negative, unsupported or newer physical marker.
    #[tokio::test]
    async fn a_window_without_a_head_refuses_an_unsupported_generation() {
        let Some(url) = crate::postgres_test_support::database_url() else {
            return;
        };
        let database = crate::testing::IsolatedDatabase::create(&url).await;
        let storage = crate::testing::connect(database.url())
            .await
            .expect("storage");
        crate::observed_sql::query("INSERT INTO lash_session_meta (session_id, relation_kind, session_state_version, retention_kind) VALUES ('no-head', 'root', 0, 'until_gc')")
            .execute(storage.pool()).await.expect("metadata");
        let store = storage.store();
        for marker in [
            Some(0_i32),
            None,
            Some(lash_core_store::store::CURRENT_SESSION_STATE_VERSION as i32 + 1),
            Some(-1),
        ] {
            crate::observed_sql::query("UPDATE lash_session_meta SET session_state_version = $1 WHERE session_id = 'no-head'")
                .bind(marker).execute(storage.pool()).await.expect("change marker");
            for selector in [
                WindowSelector::Current,
                WindowSelector::Terminal(TurnId::from("missing-run")),
            ] {
                let error = store
                    .load_session_window(&SessionId::from("no-head"), selector)
                    .await
                    .expect_err("refuse before missing head");
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
        crate::observed_sql::query(
            "UPDATE lash_session_meta SET session_state_version = $1 WHERE session_id = 'no-head'",
        )
        .bind(lash_core_store::store::CURRENT_SESSION_STATE_VERSION as i32)
        .execute(storage.pool())
        .await
        .expect("supported marker");
        let session = SessionId::from("no-head");
        let (window, window_sql) = lash_core_ids::operational_metrics::sql::collect(
            "window.admission",
            "postgres",
            store.load_session_window(&session, WindowSelector::Current),
        )
        .await;
        let (head, head_sql) = lash_core_ids::operational_metrics::sql::collect(
            "head.admission",
            "postgres",
            lash_core_store::store::SessionCommitStore::load_session_head_meta(&store, &session),
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
}
