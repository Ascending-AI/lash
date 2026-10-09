//! Fleet discovery retains the feed's committed sequencing-before-read contract.
use super::*;
use lash_core_execution::{ProcessChangeBounds, ProcessRosterCursor, ProcessRosterRecords};

async fn bounds_tx(tx: &mut sqlx::PgConnection) -> Result<ProcessChangeBounds, PluginError> {
    let row = sqlx::query(process_sql().clock_postgres.select_bounds_for_share.sql())
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(plugin_sqlx_error)?;
    Ok(ProcessChangeBounds {
        current: ProcessChangeCursor::from_store_sequence(plugin_u64_from_sql(
            "ProcessChangeClock",
            "current_seq",
            row.get(0),
        )?),
        retained_after: ProcessChangeCursor::from_store_sequence(plugin_u64_from_sql(
            "ProcessChangeClock",
            "tombstone_compaction_horizon",
            row.get(1),
        )?),
    })
}

async fn sequence(registry: &PostgresProcessRegistry) -> Result<(), PluginError> {
    crate::change_feed::sequence_before_read(
        &registry.pool,
        &registry.fence,
        crate::change_feed::Feed::Processes,
    )
    .await
    .map_err(plugin_store_error)
}

pub(super) async fn bounds(
    registry: &PostgresProcessRegistry,
) -> Result<ProcessChangeBounds, PluginError> {
    sequence(registry).await?;
    let mut tx = crate::observed_sql::control("BEGIN", registry.pool.begin())
        .await
        .map_err(plugin_sqlx_error)?;
    let bounds = bounds_tx(&mut tx).await?;
    crate::observed_sql::control("COMMIT", tx.commit())
        .await
        .map_err(plugin_sqlx_error)?;
    Ok(bounds)
}

pub(super) async fn page(
    registry: &PostgresProcessRegistry,
    filter: &lash_core_execution::ProcessListFilter,
    limit: std::num::NonZeroUsize,
    cursor: Option<ProcessRosterCursor>,
) -> Result<ProcessRosterRecords, PluginError> {
    sequence(registry).await?;
    let limit = limit
        .get()
        .min(lash_core_execution::MAX_PROCESS_ROSTER_PAGE_SIZE);
    let store = format!("postgres:{}", registry.catalog_id);
    let mut tx = crate::observed_sql::control("BEGIN", registry.pool.begin())
        .await
        .map_err(plugin_sqlx_error)?;
    // This share lock keeps compaction and sequencing behind the page. Saves
    // that race it remain unsequenced and therefore belong after the scan fence.
    let bounds = bounds_tx(&mut tx).await?;
    if let Some(cursor) = &cursor {
        cursor.validate(&store, filter, bounds)?;
    }
    let through = match &cursor {
        Some(cursor) => Some(cursor.through().clone()),
        None => sqlx::query_scalar::<_, Option<String>>(
            process_sql().process.select_max_process_id.sql(),
        )
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(plugin_sqlx_error)?
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
            .map(serde_json::to_value)
            .transpose()
            .map_err(process_decode_error)?;
        let mut query = sqlx::query_scalar::<_, String>(crate::process_sql::roster_sql(filter))
            .bind(filter.status.labels())
            .bind(filter.originator.as_ref().map(|o| o.originator_id()))
            .bind(filter.identity_kind.as_deref())
            .bind(filter.identity_label.as_deref())
            .bind(definition)
            .bind(filter.created_at_start_ms.map(clamp_epoch_ms))
            .bind(filter.created_at_end_ms.map(clamp_epoch_ms))
            .bind(filter.retired_since_ms.map(clamp_epoch_ms));
        if let Some(scope) = &filter.until {
            query = query.bind(scope.storage_kind()).bind(scope.storage_id());
        }
        if let Some(before_ms) = filter.cancel_pending_before_ms {
            query = query.bind(clamp_epoch_ms(before_ms));
        }
        let (kind, frame) = match &filter.originator {
            Some(lash_core_execution::ProcessOriginatorFilter::Host { .. }) => (Some("host"), None),
            Some(lash_core_execution::ProcessOriginatorFilter::Session(scope)) => (
                Some("session"),
                scope.agent_frame_id.as_ref().map(|id| id.as_str()),
            ),
            None => (None, None),
        };
        let query = query
            .bind(kind)
            .bind(frame)
            .bind(cursor.as_ref().map(|cursor| cursor.after().as_str()))
            .bind(through.as_str())
            .bind((limit + 1) as i64);
        let rows = query
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(plugin_sqlx_error)?;
        for json in rows {
            #[cfg(test)]
            registry
                .decoded_roster_records
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            candidates.push(serde_json::from_str(&json).map_err(process_decode_error)?);
        }
    }
    crate::observed_sql::control("COMMIT", tx.commit())
        .await
        .map_err(plugin_sqlx_error)?;
    Ok(ProcessRosterRecords::from_candidates(
        store,
        filter,
        limit,
        cursor.as_ref(),
        through,
        bounds,
        candidates,
    ))
}

#[cfg(test)]
#[tokio::test]
async fn filtered_roster_page_decodes_only_matches_in_ten_thousand_rows() {
    let url = crate::testing::required_database_url();
    let database = crate::testing::IsolatedDatabase::create(&url).await;
    let backend = crate::testing::connect(database.url())
        .await
        .expect("postgres store");
    lash_core_execution::testing::process_execution_env_fixture(&backend.process_env_store()).await;
    let registry = backend.process_registry();

    use lash_core_execution::{
        Lifetime, ProcessListFilter, ProcessProvenance, ProcessRegistrar as _,
    };
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

    sqlx::query("DELETE FROM lash_processes")
        .execute(backend.pool())
        .await
        .expect("remove template");
    sqlx::query("INSERT INTO lash_processes (process_id, originator_id, identity_kind, identity_label, created_at_ms, updated_at_ms, lifetime, record_json) SELECT r->>'id', 'host', r#>>'{identity,kind}', r#>>'{identity,label}', (r->>'created_at_ms')::bigint, (r->>'updated_at_ms')::bigint, 'detached', r::text FROM jsonb_array_elements($1) r")
        .bind(serde_json::to_value(rows).expect("encode records")).execute(backend.pool()).await.expect("seed ten thousand rows");

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
