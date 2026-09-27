use super::*;

const CURSOR_BACKEND: &str = "postgres";

pub(super) async fn count_non_terminal_processes(
    registry: &PostgresProcessRegistry,
) -> Result<usize, PluginError> {
    let count = sqlx::query_scalar::<_, i64>(
        process_sql()
            .process_postgres
            .count_non_terminal_processes
            .sql(),
    )
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

pub(super) async fn list_non_terminal_processes_page(
    registry: &PostgresProcessRegistry,
    limit: std::num::NonZeroUsize,
    continuation: Option<lash_core_execution::ProcessRegistryCursor>,
) -> Result<lash_core_execution::NonTerminalProcessPage, PluginError> {
    let page_size = limit
        .get()
        .min(lash_core_execution::MAX_NON_TERMINAL_PROCESS_PAGE_SIZE);
    if let Some(cursor) = continuation.as_ref()
        && cursor.backend() != CURSOR_BACKEND
    {
        return Err(PluginError::ProcessRegistryCursorBackendMismatch {
            expected: CURSOR_BACKEND.to_string(),
            actual: cursor.backend().to_string(),
        });
    }
    let through_process_id = match continuation.as_ref() {
        Some(cursor) => cursor.through_process_id().clone(),
        None => match sqlx::query_scalar::<_, Option<String>>(
            process_sql()
                .process_postgres
                .select_max_non_terminal_process_id
                .sql(),
        )
        .fetch_one(&registry.pool)
        .await
        .map_err(plugin_sqlx_error)?
        {
            Some(process_id) => crate::stored_process_id(&process_id)?,
            None => {
                return Ok(lash_core_execution::NonTerminalProcessPage {
                    records: Vec::new(),
                    continuation: None,
                });
            }
        },
    };
    let row_limit = i64::try_from(page_size + 1).unwrap_or(i64::MAX);
    let rows = if let Some(cursor) = continuation.as_ref() {
        sqlx::query(
            process_sql()
                .process_postgres
                .list_next_non_terminal_process_page
                .sql(),
        )
        .bind(through_process_id.as_str())
        .bind(cursor.after_process_id().as_str())
        .bind(row_limit)
        .fetch_all(&registry.pool)
        .await
        .map_err(plugin_sqlx_error)?
    } else {
        sqlx::query(
            process_sql()
                .process_postgres
                .list_first_non_terminal_process_page
                .sql(),
        )
        .bind(through_process_id.as_str())
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
    let has_more = records.len() > page_size;
    records.truncate(page_size);
    #[expect(
        clippy::expect_used,
        reason = "`has_more` is only true when `records` held more than `limit` rows, so the truncated page is non-empty"
    )]
    let continuation = has_more.then(|| {
        lash_core_execution::ProcessRegistryCursor::new(
            CURSOR_BACKEND,
            records.last().expect("non-empty bounded page").id.clone(),
            through_process_id,
        )
    });
    Ok(lash_core_execution::NonTerminalProcessPage {
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
            .expect("explain PostgreSQL non-terminal query")
            .join(" | ")
    }

    /// The witness plans against a database of its own, created from the DDL
    /// artifact for this test alone: no rows, no `ANALYZE` history, no
    /// autovacuum statistics. On a shared database the planner costs these
    /// statements from whatever rows and statistics earlier tests left behind,
    /// and PR CI once chose a bitmap scan of `idx_lash_processes_lifetime_scope`
    /// plus a sort for the first page (FIG-3687). A fresh database fixes every
    /// planner input except the statement text and the schema, which are what
    /// this asserts on. `enable_seqscan = off` is still needed, because an
    /// empty table would otherwise be scanned whatever the indexes say.
    #[tokio::test]
    async fn non_terminal_page_plans_put_both_cursor_bounds_in_the_partial_index_condition() {
        let Some(database_url) = crate::postgres_test_support::database_url() else {
            eprintln!("skipping non-terminal page plan check: database URL is not set");
            return;
        };
        let database = crate::testing::IsolatedDatabase::create(&database_url).await;
        let storage = crate::PostgresStorage::connect(database.url())
            .await
            .expect("connect PostgreSQL non-terminal page plan database");
        let mut tx = storage
            .pool
            .begin()
            .await
            .expect("begin explain transaction");
        sqlx::query("SET LOCAL enable_seqscan = off")
            .execute(&mut *tx)
            .await
            .expect("prefer the non-terminal index in the empty test database");

        let process_queries = &process_sql().process_postgres;
        let max_plan = sqlx::query_scalar::<_, String>(&format!(
            "EXPLAIN (COSTS OFF) {}",
            process_queries.select_max_non_terminal_process_id.sql()
        ))
        .fetch_all(&mut *tx)
        .await
        .expect("explain PostgreSQL non-terminal maximum")
        .join(" | ");
        let count_plan = sqlx::query_scalar::<_, String>(&format!(
            "EXPLAIN (COSTS OFF) {}",
            process_queries.count_non_terminal_processes.sql()
        ))
        .fetch_all(&mut *tx)
        .await
        .expect("explain PostgreSQL non-terminal count")
        .join(" | ");
        let first_plan = explain(
            &mut tx,
            process_queries.list_first_non_terminal_process_page.sql(),
            &["zz"],
        )
        .await;
        let continuation_plan = explain(
            &mut tx,
            process_queries.list_next_non_terminal_process_page.sql(),
            &["zz", "aa"],
        )
        .await;
        tx.rollback().await.expect("rollback explain transaction");

        for plan in [&count_plan, &max_plan, &first_plan, &continuation_plan] {
            assert!(
                plan.contains("idx_lash_processes_non_terminal"),
                "non-terminal page query must use the partial index: {plan}"
            );
        }
        assert!(
            continuation_plan.contains("process_id <=")
                && continuation_plan.contains("process_id >"),
            "both cursor bounds must be index conditions: {continuation_plan}"
        );
    }
}
