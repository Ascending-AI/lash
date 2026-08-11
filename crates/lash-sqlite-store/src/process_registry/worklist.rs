use super::*;

const CURSOR_BACKEND: &str = "sqlite";
const COUNT_NON_TERMINAL_SQL: &str =
    "SELECT COUNT(*) FROM processes INDEXED BY idx_processes_live_worklist
     WHERE status IN ('running', 'waiting')";
const MAX_WORKLIST_PROCESS_ID_SQL: &str =
    "SELECT MAX(process_id) FROM processes INDEXED BY idx_processes_live_worklist
     WHERE status IN ('running', 'waiting')";
const FIRST_WORKLIST_PAGE_SQL: &str = "SELECT record_json FROM processes
     INDEXED BY idx_processes_live_worklist
     WHERE status IN ('running', 'waiting') AND process_id <= ?1
     ORDER BY process_id ASC LIMIT ?3";
const CONTINUE_WORKLIST_PAGE_SQL: &str = "SELECT record_json FROM processes
     INDEXED BY idx_processes_live_worklist
     WHERE status IN ('running', 'waiting')
       AND process_id <= ?1 AND process_id > ?2
     ORDER BY process_id ASC LIMIT ?3";

pub(super) async fn count_non_terminal_processes(
    registry: &SqliteProcessRegistry,
) -> Result<usize, lash_core::PluginError> {
    registry
        .conn
        .call(|conn| {
            Ok((|| {
                let count: i64 = conn
                    .query_row(COUNT_NON_TERMINAL_SQL, [], |row| row.get(0))
                    .map_err(process_sqlite_error)?;
                usize::try_from(count).map_err(|_| {
                    lash_core::PluginError::Session(format!(
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
) -> Result<Vec<ProcessRecord>, lash_core::PluginError> {
    registry
        .conn
        .call(|conn| {
            Ok((|| {
                let mut stmt = conn
                    .prepare(
                        "SELECT record_json FROM processes
                         WHERE status IN ('running', 'waiting')
                         ORDER BY process_id ASC",
                    )
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

pub(super) async fn list_non_terminal_page(
    registry: &SqliteProcessRegistry,
    limit: std::num::NonZeroUsize,
    continuation: Option<lash_core::ProcessWorklistCursor>,
) -> Result<lash_core::ProcessWorklistPage, lash_core::PluginError> {
    if let Some(cursor) = continuation.as_ref()
        && cursor.backend() != CURSOR_BACKEND
    {
        return Err(
            lash_core::PluginError::ProcessWorklistCursorBackendMismatch {
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
                    Some(cursor) => cursor.through_process_id().to_string(),
                    None => match conn
                        .query_row(MAX_WORKLIST_PROCESS_ID_SQL, [], |row| {
                            row.get::<_, Option<String>>(0)
                        })
                        .map_err(process_sqlite_error)?
                    {
                        Some(process_id) => process_id,
                        None => {
                            return Ok(lash_core::ProcessWorklistPage {
                                records: Vec::new(),
                                continuation: None,
                            });
                        }
                    },
                };
                let row_limit = i64::try_from(limit.get().saturating_add(1)).unwrap_or(i64::MAX);
                let (sql, after_process_id) = match continuation.as_ref() {
                    Some(cursor) => (CONTINUE_WORKLIST_PAGE_SQL, Some(cursor.after_process_id())),
                    None => (FIRST_WORKLIST_PAGE_SQL, None),
                };
                let mut stmt = conn.prepare(sql).map_err(process_sqlite_error)?;
                let rows = stmt
                    .query_map(
                        params![through_process_id, after_process_id, row_limit],
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
                let has_more = records.len() > limit.get();
                records.truncate(limit.get());
                let continuation = has_more.then(|| {
                    lash_core::ProcessWorklistCursor::new(
                        CURSOR_BACKEND,
                        records.last().expect("non-empty bounded page").id.clone(),
                        through_process_id,
                    )
                });
                Ok(lash_core::ProcessWorklistPage {
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
    async fn worklist_plans_use_the_partial_index_without_a_temp_sort() {
        let registry = SqliteProcessRegistry::memory()
            .await
            .expect("open in-memory process registry");
        let plans = registry
            .conn
            .call(|conn| {
                conn.execute_batch(
                    "WITH RECURSIVE n(i) AS (
                         SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10000
                     )
                     INSERT INTO processes (
                         process_id, registration_fingerprint, originator_id,
                         identity_kind, is_waiting, created_at_ms, updated_at_ms,
                         change_seq, status, record_json
                     )
                     SELECT printf('plan-%05d', i), 'fp', 'host', 'test', 0, 0, 0, i,
                            CASE WHEN i <= 100 THEN 'running' ELSE 'completed' END,
                            '{}'
                     FROM n;
                     ANALYZE;",
                )?;
                let explain = |sql: &str, params: &[&dyn rusqlite::ToSql]| {
                    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                    stmt.query_map(params, |row| row.get::<_, String>(3))?
                        .collect::<Result<Vec<_>, _>>()
                };
                Ok([
                    explain(COUNT_NON_TERMINAL_SQL, &[])?.join(" | "),
                    explain(MAX_WORKLIST_PROCESS_ID_SQL, &[])?.join(" | "),
                    explain(
                        FIRST_WORKLIST_PAGE_SQL,
                        &[&"zz", &Option::<String>::None, &65_i64],
                    )?
                    .join(" | "),
                    explain(CONTINUE_WORKLIST_PAGE_SQL, &[&"zz", &"aa", &65_i64])?.join(" | "),
                ])
            })
            .await
            .expect("explain SQLite worklist queries");
        for plan in &plans {
            assert!(
                plan.contains("idx_processes_live_worklist"),
                "worklist query must use the partial index: {plan}"
            );
            assert!(
                !plan.contains("USE TEMP B-TREE"),
                "worklist query must not sort through a temp B-tree: {plan}"
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
