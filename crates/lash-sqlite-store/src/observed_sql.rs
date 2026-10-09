//! SQLite's single trace callback: physical rows and per-execution work,
//! including cached statements, on the connection's own worker.
use lash_core_execution::facade_support::sql::{SqliteWork, Window};
use rusqlite::{
    StatementStatus,
    trace::{StmtRef, TraceEvent},
};
use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    static BEFORE: RefCell<HashMap<String, SqliteWork>> = RefCell::new(HashMap::new());
}

fn counters(statement: &StmtRef<'_>) -> SqliteWork {
    let count = |status| u64::try_from(statement.get_status(status)).unwrap_or_default();
    SqliteWork {
        vm_steps: count(StatementStatus::VmStep),
        fullscan_steps: count(StatementStatus::FullscanStep),
        sorts: count(StatementStatus::Sort),
        autoindex_rows: count(StatementStatus::AutoIndex),
        reprepares: count(StatementStatus::RePrepare),
    }
}

pub(crate) fn enable(connection: &rusqlite::Connection) {
    connection.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT
            | rusqlite::trace::TraceEventCodes::SQLITE_TRACE_ROW
            | rusqlite::trace::TraceEventCodes::SQLITE_TRACE_PROFILE,
        Some(trace),
    );
}

pub(crate) fn trace(event: TraceEvent<'_>) {
    let window = Window::current();
    match event {
        TraceEvent::Stmt(statement, text) if !text.starts_with("--") => {
            if let Some(window) = &window {
                let sql = statement.sql();
                window.statement(&sql);
                BEFORE.with(|before| {
                    before
                        .borrow_mut()
                        .insert(sql.into_owned(), counters(&statement));
                });
            }
        }
        TraceEvent::Row(statement) => {
            if let Some(window) = &window {
                window.row(&statement.sql());
            }
        }
        TraceEvent::Profile(statement, _) => {
            if let Some(window) = &window {
                let sql = statement.sql();
                if let Some(before) = BEFORE.with(|before| before.borrow_mut().remove(sql.as_ref()))
                {
                    window.work(&sql, counters(&statement).difference(before));
                }
            }
            #[cfg(feature = "perf-witness")]
            if lash_core_execution::perf_witness::sql_receipts_enabled() {
                lash_core_execution::perf_witness::record_sql_statement_bytes(
                    &statement.sql(),
                    statement.expanded_sql().map_or(0, |sql| sql.len()),
                );
            } else {
                lash_core_execution::perf_witness::record_sql_statement(&statement.sql());
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    /// SQL-WORK: all five windows export statements, normalized shapes,
    /// returned rows and SQL VM work, separately from logical graph writes.
    #[tokio::test]
    async fn sql_work_receipts_cover_two_history_sizes() {
        let stores = crate::SqliteStoreSet::memory()
            .await
            .expect("SQLite stores");
        let receipts = lash_core_execution::testing::sql_work::receipts(&stores, "sqlite").await;
        assert_eq!(receipts.len(), 10);
        for receipt in receipts {
            assert!(receipt.sqlite_work.expect("SQLite work").vm_steps > 0);
            eprintln!(
                "SQL_WORK {}",
                serde_json::to_string(&receipt).expect("receipt")
            );
        }
    }
    /// SQL-OWNER: production summaries carry the retained actor fence and
    /// caller operation identity, independent of logical transition metrics.
    #[tokio::test]
    async fn physical_summary_joins_the_owner_commit_and_caller_window() {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = captured.clone();
        let observer = lash_core_execution::facade_support::StoreObserver::default().with_sql_sink(
            move |receipt, outcome| {
                if receipt.operation == "sql-work.owner" {
                    sink.lock().expect("sink").push((receipt.clone(), outcome));
                }
            },
        );
        let mut options = crate::SqliteStoreSetOptions::memory();
        options.observer = observer;
        let stores = crate::SqliteStoreSet::memory_with_options_and_clock(
            options,
            std::sync::Arc::new(lash_core_execution::facade_support::SystemClock),
        )
        .await
        .expect("stores");
        let (identity, outer) = lash_core_execution::facade_support::sql::collect(
            "caller/session/turn/record",
            "sqlite",
            lash_core_execution::testing::sql_work::owner_commit(&stores),
        )
        .await;
        let receipts = captured.lock().expect("receipts");
        assert_eq!(receipts.len(), 1);
        let (receipt, outcome) = &receipts[0];
        assert_eq!(*outcome, "success");
        assert_eq!(receipt.owner.as_ref(), Some(&identity));
        assert_eq!(
            receipt.parent_operation.as_deref(),
            Some(outer.operation.as_str())
        );
        assert!(receipt.statements > 0);
        eprintln!(
            "SQL_WORK_OWNER {}",
            serde_json::to_string(receipt).expect("receipt")
        );
    }
}
