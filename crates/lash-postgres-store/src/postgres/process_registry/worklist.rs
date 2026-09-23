use super::*;

const CURSOR_BACKEND: &str = "postgres";

pub(super) async fn count_non_terminal_processes(
    registry: &PostgresProcessRegistry,
) -> Result<usize, PluginError> {
    let count =
        sqlx::query_scalar::<_, i64>(process_sql().process_postgres.count_live_worklist.sql())
            .fetch_one(&registry.pool)
            .await
            .map_err(plugin_sqlx_error)?;
    usize::try_from(count).map_err(|_| {
        PluginError::Session(format!(
            "PostgreSQL non-terminal process count {count} does not fit usize"
        ))
    })
}

pub(super) async fn collect_non_terminal_records(
    registry: &PostgresProcessRegistry,
) -> Result<Vec<ProcessRecord>, PluginError> {
    let rows = sqlx::query(process_sql().process.collect_non_terminal_records.sql())
        .fetch_all(&registry.pool)
        .await
        .map_err(plugin_sqlx_error)?;
    let mut records = Vec::with_capacity(rows.len());
    for row in rows {
        let json: String = row.get(0);
        records.push(serde_json::from_str(&json).map_err(process_decode_error)?);
    }
    Ok(records)
}

pub(super) async fn list_non_terminal_page(
    registry: &PostgresProcessRegistry,
    limit: std::num::NonZeroUsize,
    continuation: Option<lash_core_execution::ProcessWorklistCursor>,
) -> Result<lash_core_execution::ProcessWorklistPage, PluginError> {
    if let Some(cursor) = continuation.as_ref()
        && cursor.backend() != CURSOR_BACKEND
    {
        return Err(PluginError::ProcessWorklistCursorBackendMismatch {
            expected: CURSOR_BACKEND.to_string(),
            actual: cursor.backend().to_string(),
        });
    }
    let through_process_id = match continuation.as_ref() {
        Some(cursor) => cursor.through_process_id().to_string(),
        None => match sqlx::query_scalar::<_, Option<String>>(
            process_sql()
                .process_postgres
                .select_max_worklist_process_id
                .sql(),
        )
        .fetch_one(&registry.pool)
        .await
        .map_err(plugin_sqlx_error)?
        {
            Some(process_id) => process_id,
            None => {
                return Ok(lash_core_execution::ProcessWorklistPage {
                    records: Vec::new(),
                    continuation: None,
                });
            }
        },
    };
    let row_limit = i64::try_from(limit.get().saturating_add(1)).unwrap_or(i64::MAX);
    let rows = if let Some(cursor) = continuation.as_ref() {
        sqlx::query(process_sql().process_postgres.list_next_worklist_page.sql())
            .bind(&through_process_id)
            .bind(cursor.after_process_id())
            .bind(row_limit)
            .fetch_all(&registry.pool)
            .await
            .map_err(plugin_sqlx_error)?
    } else {
        sqlx::query(
            process_sql()
                .process_postgres
                .list_first_worklist_page
                .sql(),
        )
        .bind(&through_process_id)
        .bind(row_limit)
        .fetch_all(&registry.pool)
        .await
        .map_err(plugin_sqlx_error)?
    };
    let mut records: Vec<ProcessRecord> = Vec::new();
    for row in rows {
        let json: String = row.get(0);
        records.push(serde_json::from_str(&json).map_err(process_decode_error)?);
    }
    let has_more = records.len() > limit.get();
    records.truncate(limit.get());
    #[expect(
        clippy::expect_used,
        reason = "`has_more` is only true when `records` held more than `limit` rows, so the truncated page is non-empty"
    )]
    let continuation = has_more.then(|| {
        lash_core_execution::ProcessWorklistCursor::new(
            CURSOR_BACKEND,
            records.last().expect("non-empty bounded page").id.clone(),
            through_process_id,
        )
    });
    Ok(lash_core_execution::ProcessWorklistPage {
        records,
        continuation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn explain(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        sql: &str,
        binds: &[&str],
    ) -> String {
        let statement = format!("EXPLAIN (COSTS OFF) {sql}");
        let mut query = sqlx::query_scalar::<_, String>(&statement);
        for bind in binds {
            query = query.bind(*bind);
        }
        query = query.bind(65_i64);
        query
            .fetch_all(&mut **tx)
            .await
            .expect("explain PostgreSQL worklist query")
            .join(" | ")
    }

    #[tokio::test]
    async fn worklist_plans_put_both_cursor_bounds_in_the_partial_index_condition() {
        let Some(database_url) = crate::postgres_test_support::database_url() else {
            eprintln!("skipping worklist plan check: database URL is not set");
            return;
        };
        let _lock = crate::postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
        let storage = crate::PostgresStorage::connect(&database_url)
            .await
            .expect("connect PostgreSQL worklist plan test");
        let mut tx = storage
            .pool
            .begin()
            .await
            .expect("begin explain transaction");
        sqlx::query("SET LOCAL enable_seqscan = off")
            .execute(&mut *tx)
            .await
            .expect("prefer the worklist index in the empty test database");

        let worklist = &process_sql().process_postgres;
        let max_plan = sqlx::query_scalar::<_, String>(&format!(
            "EXPLAIN (COSTS OFF) {}",
            worklist.select_max_worklist_process_id.sql()
        ))
        .fetch_all(&mut *tx)
        .await
        .expect("explain PostgreSQL worklist maximum")
        .join(" | ");
        let count_plan = sqlx::query_scalar::<_, String>(&format!(
            "EXPLAIN (COSTS OFF) {}",
            worklist.count_live_worklist.sql()
        ))
        .fetch_all(&mut *tx)
        .await
        .expect("explain PostgreSQL non-terminal count")
        .join(" | ");
        let first_plan = explain(&mut tx, worklist.list_first_worklist_page.sql(), &["zz"]).await;
        let continuation_plan = explain(
            &mut tx,
            worklist.list_next_worklist_page.sql(),
            &["zz", "aa"],
        )
        .await;
        tx.rollback().await.expect("rollback explain transaction");

        for plan in [&count_plan, &max_plan, &first_plan, &continuation_plan] {
            assert!(
                plan.contains("idx_lash_processes_live_worklist"),
                "worklist query must use the partial index: {plan}"
            );
        }
        assert!(
            continuation_plan.contains("process_id <=")
                && continuation_plan.contains("process_id >"),
            "both cursor bounds must be index conditions: {continuation_plan}"
        );
    }
}
