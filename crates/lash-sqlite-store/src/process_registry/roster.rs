//! Fleet scans capture their change fence before reading the first keyset.
use super::*;
use lash_core_execution::{ProcessChangeBounds, ProcessRosterCursor, ProcessRosterRecords};

pub(super) fn bounds_conn(
    conn: &Connection,
) -> Result<ProcessChangeBounds, lash_core_execution::PluginError> {
    let current = conn
        .query_row(process_sql().clock_sqlite.select_current.sql(), [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(process_sqlite_error)?;
    let horizon = conn
        .query_row(
            process_sql().clock_sqlite.select_compaction_horizon.sql(),
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(process_sqlite_error)?;
    Ok(ProcessChangeBounds {
        current: ProcessChangeCursor::from_store_sequence(plugin_u64_from_sql(
            "ProcessChangeClock",
            "current_seq",
            current,
        )?),
        retained_after: ProcessChangeCursor::from_store_sequence(plugin_u64_from_sql(
            "ProcessChangeClock",
            "tombstone_compaction_horizon",
            horizon,
        )?),
    })
}

pub(super) async fn bounds(
    registry: &SqliteProcessRegistry,
) -> Result<ProcessChangeBounds, lash_core_execution::PluginError> {
    registry
        .conn
        .call(|conn| {
            Ok((|| {
                let tx = conn.unchecked_transaction().map_err(process_sqlite_error)?;
                let bounds = bounds_conn(&tx)?;
                tx.commit().map_err(process_sqlite_error)?;
                Ok(bounds)
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn page(
    registry: &SqliteProcessRegistry,
    filter: &ProcessListFilter,
    limit: std::num::NonZeroUsize,
    cursor: Option<ProcessRosterCursor>,
) -> Result<ProcessRosterRecords, lash_core_execution::PluginError> {
    let store = format!("sqlite:{}", registry.location.target().canonical_name());
    let filter = filter.clone();
    let limit = limit
        .get()
        .min(lash_core_execution::MAX_PROCESS_ROSTER_PAGE_SIZE);
    #[cfg(test)]
    let decoded = Arc::clone(&registry.decoded_roster_records);
    registry
        .conn
        .call(move |conn| {
            Ok((|| {
                let tx = conn.unchecked_transaction().map_err(process_sqlite_error)?;
                let bounds = bounds_conn(&tx)?;
                if let Some(cursor) = &cursor {
                    cursor.validate(&store, &filter, bounds)?;
                }
                let through = match &cursor {
                    Some(cursor) => Some(cursor.through().clone()),
                    None => tx
                        .query_row(
                            process_sql().process.select_max_process_id.sql(),
                            [],
                            |row| row.get::<_, Option<String>>(0),
                        )
                        .map_err(process_sqlite_error)?
                        .map(|id| crate::stored_process_id(&id))
                        .transpose()?,
                };
                let mut candidates = Vec::new();
                if let Some(through) = &through
                    && filter
                        .created_at_start_ms
                        .is_none_or(|value| value <= i64::MAX as u64)
                {
                    let definition = filter
                        .definition_id
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()
                        .map_err(process_decode_error)?;
                    let status = filter
                        .status
                        .labels()
                        .map(|labels| serde_json::to_string(&labels))
                        .transpose()
                        .map_err(process_decode_error)?;
                    let (sql, values) = sql::roster_query(
                        &filter,
                        status,
                        definition,
                        cursor.as_ref().map(|cursor| cursor.after().as_str()),
                        through.as_str(),
                        limit + 1,
                    );
                    let mut stmt = tx.prepare_cached(sql).map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map(rusqlite::params_from_iter(values), |row| {
                            row.get::<_, String>(0)
                        })
                        .map_err(process_sqlite_error)?;
                    for row in rows {
                        #[cfg(test)]
                        decoded.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        candidates.push(
                            serde_json::from_str(&row.map_err(process_sqlite_error)?)
                                .map_err(process_decode_error)?,
                        );
                    }
                }
                tx.commit().map_err(process_sqlite_error)?;
                Ok(ProcessRosterRecords::from_candidates(
                    store,
                    &filter,
                    limit,
                    cursor.as_ref(),
                    through,
                    bounds,
                    candidates,
                ))
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}

#[cfg(test)]
#[tokio::test]
async fn filtered_roster_page_decodes_only_matches_in_ten_thousand_rows() {
    let backend = crate::SqliteStoreSet::memory().await.expect("memory store");
    lash_core_execution::testing::process_execution_env_fixture(
        backend.process_env_store().as_ref(),
    )
    .await;
    let registry = backend.process_registry();

    use lash_core_execution::{Lifetime, ProcessProvenance, ProcessRegistrar as _};
    let base = registry
        .register_process(lash_core_execution::testing::held_engine_registration(
            serde_json::Value::Null,
            ProcessProvenance::host(),
            Lifetime::Detached,
        ))
        .await
        .expect("register template");
    let mut rows = Vec::new();
    let mut expected = Vec::new();
    for index in 0..10_000 {
        let mut record = base.clone();
        record.id = ProcessId::fixture(&format!("roster-{index:05}"));
        let matches = matches!(index, 1000 | 5000 | 9000);
        record.identity.label = Some(if matches { "selected" } else { "other" }.to_owned());
        if matches {
            expected.push(record.id.clone());
        }
        rows.push(record);
    }
    expected.sort();

    registry.conn.call(move |conn| {
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM processes", [])?;
        for record in rows {
            tx.execute("INSERT INTO processes (process_id, originator_id, identity_kind, identity_label, created_at_ms, updated_at_ms, change_seq, lifetime, record_json) VALUES (?1, 'host', ?2, ?3, ?4, ?5, 1, 'detached', ?6)", params![record.id.as_str(), record.identity.kind.as_str(), record.identity.label, record.created_at_ms as i64, record.updated_at_ms as i64, serde_json::to_string(&record).expect("encode record")])?;
        }
        tx.commit()?;
        Ok(())
    }).await.expect("seed ten thousand rows");

    let page = registry
        .list_processes_page(
            &ProcessListFilter {
                status: lash_core_execution::ProcessStatusFilter::Any,
                identity_label: Some("selected".to_owned()),
                ..ProcessListFilter::default()
            },
            std::num::NonZeroUsize::new(10).expect("page limit"),
            None,
        )
        .await
        .expect("filtered page");
    assert_eq!(
        registry
            .decoded_roster_records
            .load(std::sync::atomic::Ordering::Relaxed),
        3,
        "a filtered page must decode only its three matching records"
    );
    assert_eq!(
        page.records
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        expected
    );
    assert!(
        page.continuation.is_none(),
        "all three matches fit in one page"
    );
}
