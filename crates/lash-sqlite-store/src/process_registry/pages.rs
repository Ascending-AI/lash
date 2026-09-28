use super::*;

const CURSOR_BACKEND: &str = "sqlite";

pub(super) async fn count_non_terminal_processes(
    registry: &SqliteProcessRegistry,
) -> Result<usize, lash_core_execution::PluginError> {
    registry
        .conn
        .call(|conn| {
            Ok((|| {
                let count: i64 = conn
                    .query_row(
                        process_sql()
                            .process_sqlite
                            .count_non_terminal_processes
                            .sql(),
                        [],
                        |row| row.get(0),
                    )
                    .map_err(process_sqlite_error)?;
                usize::try_from(count).map_err(|_| {
                    lash_core_execution::PluginError::Session(format!(
                        "SQLite non-terminal process count {count} does not fit usize"
                    ))
                })
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn collect_non_terminal_records(
    registry: &SqliteProcessRegistry,
) -> Result<Vec<ProcessRecord>, lash_core_execution::PluginError> {
    registry
        .conn
        .call(|conn| {
            Ok((|| {
                let mut stmt = conn
                    .prepare(process_sql().process.collect_non_terminal_records.sql())
                    .map_err(process_sqlite_error)?;
                let rows = stmt
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(process_sqlite_error)?;
                let mut records = Vec::new();
                for row in rows {
                    let json = row.map_err(process_sqlite_error)?;
                    records.push(serde_json::from_str(&json).map_err(process_decode_error)?);
                }
                Ok(records)
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn list_non_terminal_processes_page(
    registry: &SqliteProcessRegistry,
    limit: std::num::NonZeroUsize,
    continuation: Option<lash_core_execution::ProcessRegistryCursor>,
) -> Result<lash_core_execution::NonTerminalProcessPage, lash_core_execution::PluginError> {
    let page_size = limit
        .get()
        .min(lash_core_execution::MAX_NON_TERMINAL_PROCESS_PAGE_SIZE);
    if let Some(cursor) = continuation.as_ref()
        && cursor.backend() != CURSOR_BACKEND
    {
        return Err(
            lash_core_execution::PluginError::ProcessRegistryCursorBackendMismatch {
                expected: CURSOR_BACKEND.to_string(),
                actual: cursor.backend().to_string(),
            },
        );
    }
    registry
        .conn
        .call(move |conn| {
            Ok((|| {
                let through_process_id = match continuation.as_ref() {
                    Some(cursor) => cursor.through_process_id().clone(),
                    None => match conn
                        .query_row(
                            process_sql()
                                .process_sqlite
                                .select_max_non_terminal_process_id
                                .sql(),
                            [],
                            |row| {
                                row.get::<_, Option<String>>(0)?
                                    .map(|value| crate::sql_process_id(0, value))
                                    .transpose()
                            },
                        )
                        .map_err(process_sqlite_error)?
                    {
                        Some(process_id) => process_id,
                        None => {
                            return Ok(lash_core_execution::NonTerminalProcessPage {
                                records: Vec::new(),
                                continuation: None,
                            });
                        }
                    },
                };
                let row_limit = i64::try_from(page_size + 1).unwrap_or(i64::MAX);
                let process_queries = &process_sql().process_sqlite;
                let (sql, after_process_id) = match continuation.as_ref() {
                    Some(cursor) => (
                        process_queries.list_next_non_terminal_process_page.sql(),
                        Some(cursor.after_process_id().as_str()),
                    ),
                    None => (process_queries.list_first_non_terminal_process_page.sql(), None),
                };
                let mut stmt = conn.prepare_cached(sql).map_err(process_sqlite_error)?;
                let rows = stmt
                    .query_map(
                        params![through_process_id.as_str(), after_process_id, row_limit],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(process_sqlite_error)?;
                let mut records = Vec::new();
                for row in rows {
                    let record: ProcessRecord =
                        serde_json::from_str(&row.map_err(process_sqlite_error)?)
                            .map_err(process_decode_error)?;
                    records.push(record);
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
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn non_terminal_page_plans_use_the_partial_index_without_a_temp_sort() {
        let registry = crate::SqliteStoreSet::memory()
            .await
            .expect("open in-memory process registry")
            .process_registry();
        let plans = registry
            .conn
            .call(|conn| {
                conn.execute_batch(
                    "WITH RECURSIVE n(i) AS (
                         SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10000
                     )
                     INSERT INTO processes (
                         process_id, originator_id,
                         identity_kind, created_at_ms, updated_at_ms,
                         last_event_sequence, change_seq, status,
                         lifetime_scope_kind, lifetime_scope_id, lifetime, record_json
                     )
                     SELECT printf('plan-%05d', i), 'host', 'test', 0, 0, 0, i,
                            CASE WHEN i <= 100 THEN 'running' ELSE 'completed' END,
                            NULL, NULL, 'detached',
                            '{}'
                     FROM n;
                     ANALYZE;",
                )?;
                let explain = |sql: &str, params: &[&dyn rusqlite::ToSql]| {
                    let mut stmt = conn.prepare_cached(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                    stmt.query_map(params, |row| row.get::<_, String>(3))?
                        .collect::<Result<Vec<_>, _>>()
                };
                let process_queries = &process_sql().process_sqlite;
                Ok([
                    explain(process_queries.count_non_terminal_processes.sql(), &[])?.join(" | "),
                    explain(
                        process_queries.select_max_non_terminal_process_id.sql(),
                        &[],
                    )?
                    .join(" | "),
                    explain(
                        process_queries.list_first_non_terminal_process_page.sql(),
                        &[&"zz", &Option::<String>::None, &65_i64],
                    )?
                    .join(" | "),
                    explain(
                        process_queries.list_next_non_terminal_process_page.sql(),
                        &[&"zz", &"aa", &65_i64],
                    )?
                    .join(" | "),
                ])
            })
            .await
            .expect("explain SQLite non-terminal page queries");
        for plan in &plans {
            assert!(
                plan.contains("idx_processes_non_terminal"),
                "non-terminal page query must use the partial index: {plan}"
            );
            assert!(
                !plan.contains("USE TEMP B-TREE"),
                "non-terminal page query must not sort through a temp B-tree: {plan}"
            );
        }
        assert!(
            plans[2].contains("process_id<?"),
            "the first-page bound must be an index range: {}",
            plans[2]
        );
        assert!(
            plans[3].contains("process_id>?") && plans[3].contains("process_id<?"),
            "both continuation bounds must be index ranges: {}",
            plans[3]
        );
    }
}
