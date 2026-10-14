//! SQLite's single trace callback: physical rows and per-execution work,
//! including cached statements, on the connection's own worker.
use lash_core_execution::facade_support::sql::{SqliteWork, Window};
use rusqlite::{StatementStatus, ffi};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, c_int, c_uint, c_void};

thread_local! {
    static BEFORE: RefCell<HashMap<usize, (SqliteWork, i64)>> = RefCell::new(HashMap::new());
}

// SQLite owns each callback's statement and connection for its entire duration.
// The public StmtRef omits the row/connection handles needed for these counters.
#[expect(
    unsafe_code,
    reason = "SQLite trace supplies live borrowed statement handles"
)]
unsafe fn counters(statement: *mut ffi::sqlite3_stmt) -> SqliteWork {
    let count = |status: StatementStatus| unsafe {
        u64::try_from(ffi::sqlite3_stmt_status(statement, status as c_int, 0)).unwrap_or_default()
    };
    SqliteWork {
        vm_steps: count(StatementStatus::VmStep),
        fullscan_steps: count(StatementStatus::FullscanStep),
        sorts: count(StatementStatus::Sort),
        autoindex_rows: count(StatementStatus::AutoIndex),
        reprepares: count(StatementStatus::RePrepare),
    }
}

#[expect(
    unsafe_code,
    reason = "installs the synchronous SQLite trace callback on a live connection"
)]
pub(crate) fn enable(connection: &rusqlite::Connection) {
    unsafe {
        ffi::sqlite3_trace_v2(
            connection.handle(),
            (ffi::SQLITE_TRACE_STMT | ffi::SQLITE_TRACE_ROW | ffi::SQLITE_TRACE_PROFILE) as c_uint,
            Some(trace),
            std::ptr::null_mut(),
        );
    }
}

/// What one connection call wrote, for the performance witness: the rows its
/// statements changed and the pages its transactions wrote to the WAL.
#[cfg(feature = "perf-witness")]
pub(crate) struct WriteWitness {
    changes: u64,
}

#[cfg(feature = "perf-witness")]
impl WriteWitness {
    pub(crate) fn start(connection: &rusqlite::Connection) -> Self {
        // Pages written before this call belong to no measured caller.
        let _ = pages_written(connection);
        Self {
            changes: connection.total_changes(),
        }
    }

    /// Record the rows changed since the witness started or last recorded:
    /// a batch member's own rows (FIG-5968).
    pub(crate) fn member(&mut self, connection: &rusqlite::Connection) {
        let changes = connection.total_changes();
        lash_core_execution::perf_witness::record_sql_writes(
            changes.saturating_sub(self.changes),
            0,
        );
        self.changes = changes;
    }

    pub(crate) fn finish(self, connection: &rusqlite::Connection) {
        lash_core_execution::perf_witness::record_sql_writes(
            connection.total_changes().saturating_sub(self.changes),
            pages_written(connection),
        );
    }
}

/// The pages `connection` wrote to the WAL since the last read, which resets
/// the count (`SQLITE_DBSTATUS_CACHE_WRITE`).
#[cfg(feature = "perf-witness")]
fn pages_written(connection: &rusqlite::Connection) -> u64 {
    let mut current = 0;
    let mut highwater = 0;
    // SAFETY: the handle is this live connection's, used on its own thread
    // for one status read that neither retains nor closes it.
    #[expect(
        unsafe_code,
        reason = "rusqlite exposes no safe sqlite3_db_status; perf-witness builds only"
    )]
    let status = unsafe {
        rusqlite::ffi::sqlite3_db_status(
            connection.handle(),
            rusqlite::ffi::SQLITE_DBSTATUS_CACHE_WRITE,
            &mut current,
            &mut highwater,
            1,
        )
    };
    if status == rusqlite::ffi::SQLITE_OK {
        u64::try_from(current).unwrap_or_default()
    } else {
        0
    }
}

#[expect(
    unsafe_code,
    reason = "all callback pointers are borrowed from SQLite until this callback returns"
)]
unsafe extern "C" fn trace(
    code: c_uint,
    _: *mut c_void,
    ptr: *mut c_void,
    detail: *mut c_void,
) -> c_int {
    // Never unwind across SQLite's C boundary.
    let _ = std::panic::catch_unwind(|| unsafe {
        let statement = ptr.cast::<ffi::sqlite3_stmt>();
        let sql_ptr = ffi::sqlite3_sql(statement);
        if sql_ptr.is_null() {
            return;
        }
        let sql = CStr::from_ptr(sql_ptr).to_string_lossy();
        let window = Window::current();
        match code {
            ffi::SQLITE_TRACE_STMT => {
                if !detail.is_null() {
                    let expanded_trace = CStr::from_ptr(detail.cast()).to_bytes();
                    // Trigger trace entries are synthetic comments on the
                    // outer statement. A real SQL statement may start with a
                    // comment too, and must still have one start receipt.
                    if expanded_trace.starts_with(b"--")
                        && expanded_trace != CStr::from_ptr(sql_ptr).to_bytes()
                    {
                        return;
                    }
                }
                if let Some(window) = &window {
                    window.statement(&sql);
                    let expanded = ffi::sqlite3_expanded_sql(statement);
                    if !expanded.is_null() {
                        let bytes = CStr::from_ptr(expanded).to_bytes().len();
                        window.payload(
                            &sql,
                            0,
                            (bytes + placeholder_bytes(&sql)).saturating_sub(sql.len()) as u64,
                            0,
                        );
                        ffi::sqlite3_free(expanded.cast());
                    } else {
                        window.unmeasured_bound(&sql);
                    }
                    BEFORE.with(|before| {
                        before.borrow_mut().insert(
                            ptr.addr(),
                            (
                                counters(statement),
                                ffi::sqlite3_total_changes64(ffi::sqlite3_db_handle(statement)),
                            ),
                        )
                    });
                }
            }
            ffi::SQLITE_TRACE_ROW => {
                if let Some(window) = &window {
                    window.row(&sql);
                    let bytes = (0..ffi::sqlite3_column_count(statement))
                        .map(|column| match ffi::sqlite3_column_type(statement, column) {
                            ffi::SQLITE_INTEGER | ffi::SQLITE_FLOAT => 8,
                            ffi::SQLITE_TEXT | ffi::SQLITE_BLOB => {
                                ffi::sqlite3_column_bytes(statement, column) as u64
                            }
                            _ => 0,
                        })
                        .sum();
                    window.payload(&sql, 0, 0, bytes);
                }
                #[cfg(feature = "perf-witness")]
                lash_core_execution::perf_witness::record_sql_row_read();
            }
            ffi::SQLITE_TRACE_PROFILE => {
                if let Some((before, changes)) =
                    BEFORE.with(|before| before.borrow_mut().remove(&ptr.addr()))
                    && let Some(window) = &window
                {
                    window.work(&sql, counters(statement).difference(before));
                    let affected =
                        ffi::sqlite3_total_changes64(ffi::sqlite3_db_handle(statement)) - changes;
                    window.payload(&sql, affected.max(0) as u64, 0, 0);
                }
                #[cfg(feature = "perf-witness")]
                if lash_core_execution::perf_witness::collecting()
                    && ffi::sqlite3_stmt_readonly(statement) == 0
                    && ffi::sqlite3_bind_parameter_count(statement) > 0
                {
                    let expanded = ffi::sqlite3_expanded_sql(statement);
                    if !expanded.is_null() {
                        let bytes = bound_value_bytes(&sql, CStr::from_ptr(expanded).to_bytes());
                        ffi::sqlite3_free(expanded.cast());
                        lash_core_execution::perf_witness::record_sql_bytes_written(bytes);
                    }
                }
                #[cfg(feature = "perf-witness")]
                if lash_core_execution::perf_witness::sql_receipts_enabled() {
                    let expanded = ffi::sqlite3_expanded_sql(statement);
                    let bytes = if expanded.is_null() {
                        0
                    } else {
                        CStr::from_ptr(expanded).to_bytes().len()
                    };
                    if !expanded.is_null() {
                        ffi::sqlite3_free(expanded.cast());
                    }
                    lash_core_execution::perf_witness::record_sql_statement_bytes(&sql, bytes);
                } else {
                    lash_core_execution::perf_witness::record_sql_statement(&sql);
                }
            }
            _ => {}
        }
    });
    0
}

/// Count placeholder spelling in the original SQL, excluding quoted text and
/// comments. Expanded SQL changes only these spans; its delta gives actual
/// bound SQL literal bytes, including quoting/hex expansion and repeated uses.
fn placeholder_bytes(sql: &str) -> usize {
    placeholders(sql).map(|span| span.len()).sum()
}

/// The bytes of the values bound to `sql`, read from its `expanded` text:
/// a text's own bytes (not its quoting), a blob's bytes (not its hex
/// spelling), eight for a number and none for NULL. Every use of a repeated
/// parameter counts. Unlike WAL pages, this does not depend on where the
/// rows land in the B-trees, so the same writes count the same every run.
#[cfg(feature = "perf-witness")]
fn bound_value_bytes(sql: &str, expanded: &[u8]) -> u64 {
    let mut total = 0;
    // Expanded text is the original with each placeholder spelled as its
    // literal, so text between placeholders is identical in both.
    let mut original = 0;
    let mut at = 0;
    for span in placeholders(sql) {
        at += span.start - original;
        original = span.end;
        let Some(rest) = expanded.get(at..) else {
            break;
        };
        let (spelled, value) = literal(rest);
        at += spelled;
        total += value;
    }
    total
}

/// The spelling length and value bytes of the SQL literal `text` starts
/// with, as `sqlite3_expanded_sql` writes it.
#[cfg(feature = "perf-witness")]
fn literal(text: &[u8]) -> (usize, u64) {
    match text {
        [b'x', b'\'', hex @ ..] => {
            let digits = hex.iter().take_while(|byte| **byte != b'\'').count();
            (digits + 3, (digits / 2) as u64)
        }
        [b'\'', ..] => {
            let mut index = 1;
            let mut value = 0;
            while index < text.len() {
                if text[index] == b'\'' {
                    if text.get(index + 1) != Some(&b'\'') {
                        index += 1;
                        break;
                    }
                    index += 1;
                }
                value += 1;
                index += 1;
            }
            (index, value)
        }
        [b'N', b'U', b'L', b'L', ..] => (4, 0),
        _ => {
            let spelled = text
                .iter()
                .take_while(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-')
                })
                .count();
            (spelled, 8)
        }
    }
}

/// The byte spans of `sql`'s parameter placeholders, skipping quoted text
/// and comments.
fn placeholders(sql: &str) -> impl Iterator<Item = std::ops::Range<usize>> + '_ {
    let bytes = sql.as_bytes();
    let mut index = 0;
    std::iter::from_fn(move || {
        while index < bytes.len() {
            let start = index;
            match bytes[index] {
                b'\'' | b'"' | b'`' | b'[' => {
                    let quote = if bytes[index] == b'[' {
                        b']'
                    } else {
                        bytes[index]
                    };
                    index += 1;
                    while index < bytes.len() {
                        if bytes[index] == quote {
                            index += 1;
                            if bytes.get(index) != Some(&quote) {
                                break;
                            }
                        }
                        index += 1;
                    }
                }
                b'-' if bytes.get(index + 1) == Some(&b'-') => {
                    while index < bytes.len() && bytes[index] != b'\n' {
                        index += 1;
                    }
                }
                b'/' if bytes.get(index + 1) == Some(&b'*') => {
                    index += 2;
                    while index + 1 < bytes.len() && &bytes[index..index + 2] != b"*/" {
                        index += 1;
                    }
                    index = (index + 2).min(bytes.len());
                }
                b'?' | b':' | b'@' | b'$' => {
                    index += 1;
                    while bytes
                        .get(index)
                        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                    {
                        index += 1;
                    }
                    return Some(start..index);
                }
                _ => index += 1,
            }
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SQL-PAYLOAD: counts writes separately from returned rows, keeps result
    /// payload distinct from SQLite's expanded bind spelling, and cached
    /// re-execution cannot inherit a previous statement's changes.
    #[test]
    fn sql_payload_counts_actual_rows_and_cached_execution_bytes() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute(
                "CREATE TABLE sample (text TEXT, bytes BLOB, number INTEGER)",
                [],
            )
            .unwrap();
        enable(&connection);
        for iteration in 0..2 {
            let window = Window::new(format!("payload-{iteration}"), "sqlite");
            Window::on_worker(Some(window.clone()), || {
                connection
                    .prepare_cached("INSERT INTO sample VALUES (?1, ?2, ?3)")
                    .unwrap()
                    .execute(rusqlite::params!["é'a", &[0_u8, 1, 2][..], 42_i64])
                    .unwrap();
                connection
                    .prepare_cached("-- a real statement, not a trigger\nSELECT text, bytes, number FROM sample LIMIT 1")
                    .unwrap()
                    .query_row([], |_| Ok(()))
                    .unwrap();
            });
            let receipt = window.snapshot();
            assert_eq!(receipt.statements, 2);
            assert_eq!(receipt.rows_affected, 1);
            assert_eq!(receipt.rows_returned, 1);
            assert_eq!(receipt.unmeasured_bound_statements, 0);
            assert_eq!(receipt.bound_bytes, 18); // 'é''a', x'000102', 42
            assert_eq!(receipt.result_bytes, 15); // 4 UTF-8 + 3 blob + 8 integer
            assert_eq!(
                receipt
                    .shapes
                    .values()
                    .map(|s| s.rows_affected)
                    .sum::<u64>(),
                1
            );
            eprintln!("SQL_PAYLOAD {}", serde_json::to_string(&receipt).unwrap());
        }
    }

    /// SQL-BYTES-WRITTEN: a write counts the bytes of the values it binds,
    /// not their SQL spelling: a text without its quoting, a blob without its
    /// hex, eight per number, none for NULL, and every use of a repeated
    /// parameter; placeholders inside comments and quoted text are not
    /// parameters. The replay gate's depth slopes bind this count because it
    /// does not move with B-tree page layout (FIG-6280).
    #[cfg(feature = "perf-witness")]
    #[test]
    fn bound_value_bytes_count_values_not_their_spelling() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute("CREATE TABLE sample (a, b, c, d, e, f)", [])
            .unwrap();
        let sql = "INSERT INTO sample VALUES (?1, :blob, ?3, -- ?9\n ?4, '?', ?1)";
        let mut statement = connection.prepare(sql).unwrap();
        statement.raw_bind_parameter(1, "it's é").unwrap();
        statement
            .raw_bind_parameter(2, &[0_u8, 1, 2, 3, 4][..])
            .unwrap();
        statement.raw_bind_parameter(3, -12.5_f64).unwrap();
        statement
            .raw_bind_parameter(4, rusqlite::types::Null)
            .unwrap();
        let expanded = statement.expanded_sql().unwrap();
        // 7 text bytes twice, 5 blob bytes, one number, NULL.
        assert_eq!(bound_value_bytes(sql, expanded.as_bytes()), 7 * 2 + 5 + 8);
    }

    /// GRAPH-INSTALL-WORK: count successful store insertions, including
    /// reinstalls after collection, rather than recorder-global first-seen ids.
    #[tokio::test]
    async fn graph_install_bytes_follow_deduplication_and_collection() {
        use lash_core_store::checkpoint_graph::*;
        use lash_durable::{ActorKey, CommitLabel, DurableStore, Epoch};
        let stores = crate::SqliteStoreSet::memory().await.unwrap();
        let identity = lash_core_execution::testing::sql_work::owner_commit(&stores).await;
        let store = stores.durable_store();
        let actor = ActorKey::session("sql-work-owner").unwrap();
        let mut graph = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(1 << 20)));
        let root = graph
            .encode_value(NodeKind::TEXT_LEAF, &"install witness".to_owned())
            .unwrap();
        let bytes = graph.nodes()[&root].bytes.len() as u64;
        for (sample, expected) in [bytes, 0, bytes].into_iter().enumerate() {
            if sample == 2 {
                store.audit_orphans().await.unwrap();
            }
            let mut tx = store.begin(&actor, Epoch(identity.epoch)).await.unwrap();
            tx.stage(graph.nodes().values().cloned());
            let (answer, receipt) = lash_core_execution::facade_support::sql::collect(
                format!("install-{sample}"),
                "sqlite",
                store.commit(tx, CommitLabel::new("sql-work.install")),
            )
            .await;
            answer.unwrap();
            assert_eq!(receipt.newly_installed_graph_bytes, expected);
            eprintln!("SQL_INSTALL {}", serde_json::to_string(&receipt).unwrap());
        }
    }

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
